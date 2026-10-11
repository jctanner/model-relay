use std::{collections::BTreeMap, env, path::Path, sync::Arc};
use vertex_shim::{
    App, Model, auth,
    backend::{Backend, Upstream},
    credentials::OAuth,
    logging,
    mock_adc::MockAdc,
    telemetry::Store,
};

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(_) => {
            // Error Debug/Display can include configured URLs or file contents.
            logging::emit(
                "error",
                "service_failed",
                serde_json::json!({"error_type":"service_configuration_or_runtime_error"}),
            );
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let backend = match env::var("SHIM_BACKEND")
        .unwrap_or_else(|_| "mock".into())
        .as_str()
    {
        "mock" => Backend::Mock,
        "anthropic" => {
            let synthetic = env::var("SHIM_UPSTREAM_SYNTHETIC").is_ok_and(|v| v == "true");
            let auth_mode = env::var("SHIM_UPSTREAM_AUTH").unwrap_or_else(|_| "bearer-file".into());
            let token_file = match auth_mode.as_str() {
                "bearer-file" => env::var("SHIM_UPSTREAM_TOKEN_FILE")?.into(),
                "claude-oauth" => std::path::PathBuf::new(),
                _ => return Err("Unknown upstream credential mode".into()),
            };
            let mut upstream = Upstream::new(
                &env::var("SHIM_UPSTREAM_BASE_URL")?,
                token_file,
                synthetic,
                env::var("SHIM_UPSTREAM_WORKSPACE").ok(),
                vertex_shim::backend::upstream_timeout()?,
            )?;
            if auth_mode == "claude-oauth" {
                upstream = upstream.with_oauth(OAuth::new(env::var("SHIM_OAUTH_DIR")?.into())?);
            }
            Backend::Anthropic(upstream)
        }
        _ => return Err("Unknown backend; no fallback".into()),
    };
    let registry = env::var("SHIM_KEYS_FILE").unwrap_or_else(|_| "/config/keys.json".into());
    let _: auth::Registry = serde_json::from_slice(&std::fs::read(&registry)?)?;
    let models: BTreeMap<String, Model> = serde_json::from_slice(&std::fs::read(
        env::var("SHIM_MODELS_FILE").unwrap_or_else(|_| "/config/models.json".into()),
    )?)?;
    if matches!(backend, Backend::Anthropic(_))
        && models.values().any(|model| {
            model
                .upstream_model
                .as_ref()
                .is_none_or(|name| !vertex_shim::model_policy::valid_id(name))
        })
    {
        return Err("Anthropic backend requires an explicit upstream_model for every model".into());
    }
    let admin_key = std::fs::read_to_string(
        env::var("SHIM_ADMIN_KEY_FILE").unwrap_or_else(|_| "/run/secrets/admin-api-key".into()),
    )?;
    let admin_key = admin_key.trim();
    if admin_key.is_empty() {
        return Err("Empty admin key".into());
    }
    let db_path = env::var("SHIM_DB").unwrap_or_else(|_| "/data/telemetry.sqlite".into());
    let store = Store::open(Path::new(&db_path))?;
    let mock_adc = if let Ok(key_id) = env::var("SHIM_MOCK_ADC_KEY_ID") {
        Some(Arc::new(MockAdc::new(
            key_id,
            env::var("SHIM_MOCK_ADC_PROJECT").unwrap_or_else(|_| "vertex-shim-local".into()),
            env::var("SHIM_MOCK_ADC_TOKEN_TTL_SECONDS")
                .unwrap_or_else(|_| "600".into())
                .parse::<u64>()?,
        )?))
    } else {
        None
    };
    let mut upstreams = BTreeMap::new();
    if let Ok(path) = env::var("SHIM_UPSTREAM_PROFILES_FILE") {
        let profiles: BTreeMap<String, vertex_shim::projects::UpstreamProfile> =
            serde_json::from_slice(&std::fs::read(path)?)?;
        if profiles.len() > 100 {
            return Err("Too many upstream profiles".into());
        }
        for (id, profile) in profiles {
            if id == "default" || !vertex_shim::projects::valid_id(&id) {
                return Err("Invalid upstream profile ID".into());
            }
            upstreams.insert(id, profile.backend()?);
        }
    }
    let accounts = vertex_shim::accounts::Manager::open(
        store.clone(),
        env::var("SHIM_ACCOUNTS_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                Path::new(&db_path)
                    .parent()
                    .unwrap_or(Path::new("/data"))
                    .join("oauth-accounts")
            }),
        env::var("SHIM_CLAUDE_BIN")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                if Path::new("/build/bin/claude").exists() {
                    "/build/bin/claude".into()
                } else {
                    "/usr/local/bin/claude".into()
                }
            }),
        env::var("SHIM_MANAGED_UPSTREAM_BASE_URL")
            .unwrap_or_else(|_| "https://api.anthropic.com".into()),
        env::var("SHIM_OAUTH_TOKEN_URL")
            .unwrap_or_else(|_| "https://platform.claude.com/v1/oauth/token".into()),
        env::var("SHIM_UPSTREAM_SYNTHETIC").is_ok_and(|v| v == "true"),
    )
    .await?;
    for id in upstreams.keys() {
        if accounts.contains_profile(id) {
            return Err("Upstream profile ID collision".into());
        }
    }
    let app = App {
        routing: None,
        inventory: None,
        project_policy: Default::default(),
        accounts: Some(accounts.clone()),
        upstreams: Arc::new(upstreams),
        mock_adc,
        backend,
        store: store.clone(),
        registry: registry.into(),
        admin_verifier: auth::fingerprint(admin_key),
        models: Arc::new(models),
    };
    let inference = tokio::net::TcpListener::bind(
        env::var("SHIM_LISTEN").unwrap_or_else(|_| "0.0.0.0:8080".into()),
    )
    .await?;
    let admin = tokio::net::TcpListener::bind(
        env::var("SHIM_ADMIN_LISTEN").unwrap_or_else(|_| "127.0.0.1:8081".into()),
    )
    .await?;
    let metadata = if app.mock_adc.is_some() {
        Some(
            tokio::net::TcpListener::bind(
                env::var("SHIM_MOCK_ADC_LISTEN").unwrap_or_else(|_| "0.0.0.0:8082".into()),
            )
            .await?,
        )
    } else {
        None
    };
    logging::emit(
        "info",
        "startup",
        serde_json::json!({
            "backend": app.backend.name(),
            "synthetic": app.backend.synthetic(),
            "inference_listen": inference.local_addr()?.to_string(),
            "admin_listen": admin.local_addr()?.to_string(),
            "mock_adc_listen": metadata.as_ref().map(|listener| listener.local_addr().map(|addr| addr.to_string())).transpose()?,
        }),
    );
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let account_worker = accounts.background(stop_rx.clone());
    let shutdown = |mut rx: tokio::sync::watch::Receiver<bool>| async move {
        while !*rx.borrow() {
            if rx.changed().await.is_err() {
                break;
            }
        }
    };
    let listener_a = axum::serve(inference, vertex_shim::inference(app.clone()))
        .with_graceful_shutdown(shutdown(stop_rx.clone()));
    let listener_b = axum::serve(admin, vertex_shim::admin(app.clone()))
        .with_graceful_shutdown(shutdown(stop_rx.clone()));
    let listener_c = async {
        match metadata {
            Some(listener) => {
                axum::serve(listener, vertex_shim::mock_adc::metadata(app))
                    .with_graceful_shutdown(shutdown(stop_rx))
                    .await
            }
            None => Ok(()),
        }
    };
    let signal = async move {
        #[cfg(unix)]
        {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler");
            let reason =
                tokio::select! { _=tokio::signal::ctrl_c()=>"sigint", _=term.recv()=>"sigterm" };
            logging::emit(
                "info",
                "shutdown_started",
                serde_json::json!({"reason":reason}),
            );
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
            logging::emit(
                "info",
                "shutdown_started",
                serde_json::json!({"reason":"ctrl_c"}),
            );
        }
        let _ = stop_tx.send(true);
    };
    let (a, b, c, (), ()) =
        tokio::join!(listener_a, listener_b, listener_c, signal, account_worker);
    a?;
    b?;
    c?;
    store.flush().await;
    logging::emit(
        "info",
        "shutdown",
        serde_json::json!({"telemetry":store.health()}),
    );
    Ok(())
}
