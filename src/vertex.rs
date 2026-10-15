//! Managed Google credentials. Secret values never implement Debug or enter REST.
use crate::{
    ApiError,
    credentials::{atomic_json, bounded_file, private_directory, private_file},
};
use axum::http::StatusCode;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    os::unix::fs::MetadataExt,
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    pub project_id: String,
    pub region: String,
    // Omit or leave empty to preserve the stored key while editing settings.
    pub service_account_json: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
struct Settings {
    project_id: String,
    region: String,
}
impl Settings {
    fn valid(&self) -> bool {
        let project = self.project_id.as_bytes();
        (6..=63).contains(&project.len())
            && project[0].is_ascii_lowercase()
            && project.last().is_some_and(u8::is_ascii_alphanumeric)
            && project
                .iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
            && !self.region.is_empty()
            && self.region.len() <= 63
            && self.region.as_bytes()[0].is_ascii_lowercase()
            && self
                .region
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric)
            && self
                .region
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    }
    fn origin(&self) -> String {
        match self.region.as_str() {
            "global" => "https://aiplatform.googleapis.com".into(),
            "us" | "eu" => format!("https://aiplatform.{}.rep.googleapis.com", self.region),
            region => format!("https://{region}-aiplatform.googleapis.com"),
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct Saved {
    settings: Settings,
    service_account: Value,
}
#[derive(Deserialize)]
struct ServiceAccount {
    #[serde(rename = "type")]
    kind: String,
    client_email: String,
    private_key_id: String,
    private_key: String,
    token_uri: String,
}
impl ServiceAccount {
    fn parse(value: &Value) -> Result<Self, ApiError> {
        let account: Self = serde_json::from_value(value.clone())
            .map_err(|_| ApiError::bad("Invalid Google service-account JSON"))?;
        if account.kind != "service_account"
            || !account.client_email.ends_with(".iam.gserviceaccount.com")
            || !account.client_email.contains('@')
            || account.client_email.len() > 254
            || account.client_email.bytes().any(|b| !b.is_ascii_graphic())
            || account.private_key_id.is_empty()
            || account.private_key_id.len() > 200
            || !account.private_key_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            // Never trust an uploaded token_uri as a credential destination.
            || account.token_uri != TOKEN_URL
        {
            return Err(ApiError::bad("Invalid Google service-account JSON"));
        }
        account.assertion(TOKEN_URL)?;
        Ok(account)
    }
    fn assertion(&self, audience: &str) -> Result<String, ApiError> {
        let key = EncodingKey::from_rsa_pem(self.private_key.as_bytes())
            .map_err(|_| ApiError::bad("Invalid Google service-account private key"))?;
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(self.private_key_id.clone());
        let now = chrono::Utc::now().timestamp();
        encode(
            &header,
            &json!({"iss":self.client_email,"scope":SCOPE,
            "aud":audience,"iat":now,"exp":now + 3600}),
            &key,
        )
        .map_err(|_| ApiError::bad("Invalid Google service-account private key"))
    }
}
struct Token {
    access: String,
    expires: Instant,
    expires_at: String,
}
struct State {
    saved: Option<Saved>,
    token: Option<Token>,
    error: Option<&'static str>,
    retry_after: Option<Instant>,
}
pub struct Vertex {
    path: PathBuf,
    client: reqwest::Client,
    endpoint: String,
    state: Mutex<State>,
    #[cfg(test)]
    origin_override: Option<String>,
}
pub(crate) struct Authorization {
    pub token: String,
    pub origin: String,
    pub project_id: String,
    pub region: String,
}
fn unavailable() -> ApiError {
    ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "upstream_auth_unavailable",
        "Google credential unavailable; check private account status",
    )
}
impl Vertex {
    pub(crate) fn new(directory: PathBuf) -> Result<Self, ApiError> {
        private_directory(&directory)?;
        let path = directory.join("vertex-credentials.json");
        let loaded = (|| {
            let owner = std::fs::symlink_metadata(&directory)
                .map_err(|_| unavailable())?
                .uid();
            private_file(&path, owner)?;
            let saved: Saved = serde_json::from_str(&bounded_file(&path, 64 * 1024)?)
                .map_err(|_| unavailable())?;
            if !saved.settings.valid() {
                return Err(unavailable());
            }
            ServiceAccount::parse(&saved.service_account)?;
            Ok(saved)
        })();
        let (saved, error) = match loaded {
            Ok(saved) => (Some(saved), None),
            Err(_) => (None, Some("upstream_auth_unavailable")),
        };
        Ok(Self {
            path,
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(30))
                .build()
                .map_err(|_| ApiError::internal())?,
            endpoint: TOKEN_URL.into(),
            state: Mutex::new(State {
                saved,
                token: None,
                error,
                retry_after: None,
            }),
            #[cfg(test)]
            origin_override: None,
        })
    }
    pub(crate) async fn configure(&self, input: Input) -> Result<(), ApiError> {
        let settings = Settings {
            project_id: input.project_id,
            region: input.region,
        };
        if !settings.valid() {
            return Err(ApiError::bad("Invalid Vertex project or region"));
        }
        let mut state = self.state.lock().await;
        let service_account = match input.service_account_json.filter(|s| !s.is_empty()) {
            Some(text) => {
                if text.len() > 48 * 1024 {
                    return Err(ApiError::bad("Service-account JSON is too large"));
                }
                serde_json::from_str(&text)
                    .map_err(|_| ApiError::bad("Invalid Google service-account JSON"))?
            }
            None => state
                .saved
                .as_ref()
                .ok_or_else(|| ApiError::bad("Service-account JSON is required"))?
                .service_account
                .clone(),
        };
        ServiceAccount::parse(&service_account)?;
        let saved = Saved {
            settings,
            service_account,
        };
        atomic_json(&self.path, &json!(saved))?;
        state.saved = Some(saved);
        state.token = None;
        state.error = None;
        state.retry_after = None;
        Ok(())
    }
    pub(crate) async fn disconnect(&self) -> Result<(), ApiError> {
        let mut state = self.state.lock().await;
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(ApiError::internal()),
        }
        state.saved = None;
        state.token = None;
        state.error = None;
        state.retry_after = None;
        Ok(())
    }
    pub(crate) async fn status(&self) -> Value {
        let state = self.state.lock().await;
        json!({"mode":"google-service-account",
            "state":if state.saved.is_some() && state.error.is_none() {"ready"} else {"unavailable"},
            "configured":state.saved.is_some(),"error":state.error,
            "project_id":state.saved.as_ref().map(|s| &s.settings.project_id),
            "region":state.saved.as_ref().map(|s| &s.settings.region),
            "expires_at":state.token.as_ref().map(|t| &t.expires_at)})
    }
    pub(crate) async fn renew(&self) -> Result<(), ApiError> {
        let mut state = self.state.lock().await;
        state.token = None;
        state.retry_after = None;
        self.authorize_locked(&mut state).await.map(|_| ())
    }
    pub(crate) async fn authorize(&self) -> Result<Authorization, ApiError> {
        self.authorize_locked(&mut *self.state.lock().await).await
    }
    async fn authorize_locked(&self, state: &mut State) -> Result<Authorization, ApiError> {
        let saved = state.saved.as_ref().ok_or_else(unavailable)?;
        if state
            .token
            .as_ref()
            .is_none_or(|t| t.expires <= Instant::now() + Duration::from_secs(60))
        {
            if state
                .retry_after
                .is_some_and(|retry| retry > Instant::now())
            {
                return Err(unavailable());
            }
            match self.exchange(&saved.service_account).await {
                Ok(token) => {
                    state.token = Some(token);
                    state.error = None;
                    state.retry_after = None;
                }
                Err(_) => {
                    state.token = None;
                    state.error = Some("google_token_exchange_failed");
                    state.retry_after = Some(Instant::now() + Duration::from_secs(10));
                    return Err(unavailable());
                }
            }
        }
        let origin = saved.settings.origin();
        #[cfg(test)]
        let origin = self.origin_override.clone().unwrap_or(origin);
        Ok(Authorization {
            token: state.token.as_ref().ok_or_else(unavailable)?.access.clone(),
            origin,
            project_id: saved.settings.project_id.clone(),
            region: saved.settings.region.clone(),
        })
    }
    async fn exchange(&self, service_account: &Value) -> Result<Token, ApiError> {
        let account = ServiceAccount::parse(service_account)?;
        let assertion = account.assertion(&self.endpoint)?;
        let mut response = self
            .client
            .post(&self.endpoint)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", assertion.as_str()),
            ])
            .send()
            .await
            .map_err(|_| unavailable())?;
        if !response.status().is_success() {
            return Err(unavailable());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| unavailable())? {
            if bytes.len() + chunk.len() > 32 * 1024 {
                return Err(unavailable());
            }
            bytes.extend_from_slice(&chunk);
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
        let access = value["access_token"]
            .as_str()
            .filter(|s| {
                !s.is_empty() && s.len() <= 16 * 1024 && s.bytes().all(|b| b.is_ascii_graphic())
            })
            .ok_or_else(unavailable)?
            .to_owned();
        if !value["token_type"]
            .as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case("Bearer"))
        {
            return Err(unavailable());
        }
        let seconds = value["expires_in"]
            .as_u64()
            .filter(|s| (61..=86400).contains(s))
            .ok_or_else(unavailable)?;
        Ok(Token {
            access,
            expires: Instant::now() + Duration::from_secs(seconds),
            expires_at: (chrono::Utc::now() + chrono::Duration::seconds(seconds as i64))
                .to_rfc3339(),
        })
    }
}

pub(crate) fn model_id(id: &str) -> Result<String, ApiError> {
    if !id.starts_with("claude-") || !crate::model_policy::valid_id(id) || id.contains('/') {
        return Err(ApiError::bad("Vertex requires a Claude model ID"));
    }
    Ok(match id.rsplit_once('-') {
        Some((base, date)) if date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) => {
            format!("{base}@{date}")
        }
        _ => id.into(),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use axum::{Form, Json, Router, routing::post};
    use jsonwebtoken::{DecodingKey, Validation, decode};
    use rsa::{
        RsaPrivateKey,
        pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding},
    };
    use std::{
        collections::HashMap,
        sync::{
            Arc, OnceLock,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    // Fresh, test-only keys generated in memory. No reusable credential fixture.
    fn keypair() -> &'static (String, String) {
        static KEYS: OnceLock<(String, String)> = OnceLock::new();
        KEYS.get_or_init(|| {
            let key = RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap();
            (
                key.to_pkcs8_pem(LineEnding::LF).unwrap().to_string(),
                key.to_public_key()
                    .to_public_key_pem(LineEnding::LF)
                    .unwrap(),
            )
        })
    }
    pub(crate) fn service_account() -> Value {
        json!({"type":"service_account","project_id":"key-owner-project",
            "client_email":"fixture@key-owner-project.iam.gserviceaccount.com",
            "private_key_id":"fixture-key-1","private_key":keypair().0,
            "token_uri":TOKEN_URL})
    }
    pub(crate) fn input(project: &str, region: &str) -> Input {
        Input {
            project_id: project.into(),
            region: region.into(),
            service_account_json: Some(service_account().to_string()),
        }
    }
    pub(crate) async fn fixture(
        directory: PathBuf,
    ) -> (
        Vertex,
        Arc<AtomicUsize>,
        Arc<AtomicBool>,
        tokio::task::JoinHandle<()>,
    ) {
        let calls = Arc::new(AtomicUsize::new(0));
        let fail = Arc::new(AtomicBool::new(false));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let endpoint = format!("{origin}/token");
        let captured_endpoint = endpoint.clone();
        let seen = calls.clone();
        let failing = fail.clone();
        let router = Router::new().route("/token", post(move |Form(form): Form<HashMap<String, String>>| {
            let calls = seen.clone(); let fail = failing.clone(); let audience = captured_endpoint.clone();
            async move {
                let call = calls.fetch_add(1, Ordering::SeqCst) + 1;
                assert_eq!(form["grant_type"], "urn:ietf:params:oauth:grant-type:jwt-bearer");
                let mut validation = Validation::new(Algorithm::RS256);
                validation.set_audience(&[audience]);
                validation.set_issuer(&["fixture@key-owner-project.iam.gserviceaccount.com"]);
                let jwt = decode::<Value>(&form["assertion"],
                    &DecodingKey::from_rsa_pem(keypair().1.as_bytes()).unwrap(), &validation).unwrap();
                assert_eq!(jwt.claims["scope"], SCOPE);
                assert_eq!(jwt.header.kid.as_deref(), Some("fixture-key-1"));
                if fail.load(Ordering::SeqCst) {
                    return (StatusCode::BAD_REQUEST, Json(json!({"error":"invalid_grant","error_description":"NEVER-ECHO-GOOGLE-ERROR"})));
                }
                (StatusCode::OK, Json(json!({"access_token":format!("fixture-google-access-{call}"),
                    "token_type":"Bearer","expires_in":3600})))
            }
        }));
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let mut vertex = Vertex::new(directory).unwrap();
        vertex.endpoint = endpoint;
        vertex.origin_override = Some(origin);
        (vertex, calls, fail, server)
    }
    #[tokio::test]
    async fn google_tokens_are_single_flight_expire_rotate_and_disconnect() {
        let directory = tempfile::tempdir().unwrap();
        let (vertex, calls, _, server) = fixture(directory.path().join("account")).await;
        vertex
            .configure(input("real-upstream-project", "global"))
            .await
            .unwrap();
        let vertex = Arc::new(vertex);
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..128 {
            let vertex = vertex.clone();
            tasks.spawn(async move { vertex.authorize().await.unwrap().token });
        }
        while let Some(token) = tasks.join_next().await {
            assert_eq!(token.unwrap(), "fixture-google-access-1");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        vertex.state.lock().await.token.as_mut().unwrap().expires = Instant::now();
        assert_eq!(
            vertex.authorize().await.unwrap().token,
            "fixture-google-access-2"
        );
        // Changing routing settings preserves the saved key and invalidates OAuth cache.
        vertex
            .configure(Input {
                project_id: "other-upstream-project".into(),
                region: "us-east5".into(),
                service_account_json: None,
            })
            .await
            .unwrap();
        let auth = vertex.authorize().await.unwrap();
        assert_eq!(auth.token, "fixture-google-access-3");
        assert_eq!(auth.project_id, "other-upstream-project");
        assert_eq!(auth.region, "us-east5");
        let safe = vertex.status().await.to_string();
        for secret in [
            &keypair().0,
            "fixture-google-access-3",
            "fixture@key-owner-project.iam.gserviceaccount.com",
        ] {
            assert!(!safe.contains(secret));
        }
        vertex.disconnect().await.unwrap();
        assert!(!vertex.path.exists());
        assert!(vertex.authorize().await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        server.abort();
    }
    #[tokio::test]
    async fn google_failure_is_redacted_and_backed_off() {
        let directory = tempfile::tempdir().unwrap();
        let (vertex, calls, fail, server) = fixture(directory.path().join("account")).await;
        vertex
            .configure(input("real-upstream-project", "global"))
            .await
            .unwrap();
        fail.store(true, Ordering::SeqCst);
        for _ in 0..16 {
            assert_eq!(
                vertex.authorize().await.err().unwrap().1,
                "upstream_auth_unavailable"
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            vertex.status().await["error"],
            "google_token_exchange_failed"
        );
        assert!(!vertex.status().await.to_string().contains("NEVER-ECHO"));
        fail.store(false, Ordering::SeqCst);
        vertex.renew().await.unwrap();
        assert_eq!(vertex.status().await["state"], "ready");
        server.abort();
    }
    #[tokio::test]
    async fn invalid_secret_and_destinations_cannot_replace_working_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let vertex = Vertex::new(directory.path().join("account")).unwrap();
        vertex
            .configure(input("real-upstream-project", "global"))
            .await
            .unwrap();
        let initial = std::fs::read(&vertex.path).unwrap();
        for (field, value) in [
            ("type", "authorized_user"),
            ("token_uri", "https://attacker.example/token"),
            ("private_key", "PRIVATE-BUT-INVALID"),
            ("client_email", "attacker@example.com"),
        ] {
            let mut secret = service_account();
            secret[field] = json!(value);
            let mut replacement = input("real-upstream-project", "global");
            replacement.service_account_json = Some(secret.to_string());
            let error = vertex.configure(replacement).await.err().unwrap();
            assert!(!error.2.contains(value));
            assert_eq!(std::fs::read(&vertex.path).unwrap(), initial);
        }
        for (project, region) in [
            ("../../escape", "global"),
            ("real-upstream-project", "global.evil.example"),
            ("real-upstream-project", "../../other"),
        ] {
            assert!(vertex.configure(input(project, region)).await.is_err());
            assert_eq!(std::fs::read(&vertex.path).unwrap(), initial);
        }
        assert_eq!(
            std::fs::metadata(&vertex.path).unwrap().mode() & 0o777,
            0o600
        );
        // Restart reads the same private store; a loose file fails closed.
        assert_eq!(
            Vertex::new(directory.path().join("account"))
                .unwrap()
                .status()
                .await["configured"],
            true
        );
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&vertex.path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            Vertex::new(directory.path().join("account"))
                .unwrap()
                .status()
                .await["configured"],
            false
        );
    }
    #[test]
    fn vertex_endpoint_and_model_mapping() {
        for (region, origin) in [
            ("global", "https://aiplatform.googleapis.com"),
            ("us-east5", "https://us-east5-aiplatform.googleapis.com"),
            ("eu", "https://aiplatform.eu.rep.googleapis.com"),
        ] {
            assert_eq!(
                Settings {
                    project_id: "fixture-project".into(),
                    region: region.into()
                }
                .origin(),
                origin
            );
        }
        assert_eq!(
            model_id("claude-haiku-4-5-20251001").unwrap(),
            "claude-haiku-4-5@20251001"
        );
        assert_eq!(model_id("claude-opus-4-6").unwrap(), "claude-opus-4-6");
        assert!(model_id("rits/zai-org/glm-5-3").is_err());
        assert!(model_id("claude-../x").is_err());
    }

    #[tokio::test]
    async fn routed_messages_and_vertex_clients_preserve_streaming_counting_and_telemetry() {
        use axum::{
            body::Body,
            extract::OriginalUri,
            http::{HeaderMap, Request},
            response::IntoResponse,
        };
        use http_body_util::BodyExt;
        use tower::ServiceExt;
        let dir = tempfile::tempdir().unwrap();
        let (mut vertex, oauth_calls, _, oauth_server) = fixture(dir.path().join("account")).await;
        vertex
            .configure(input("real-vertex-project", "global"))
            .await
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        vertex.origin_override = Some(format!("http://{}", listener.local_addr().unwrap()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let router = Router::new().fallback(post(move |OriginalUri(uri): OriginalUri, headers: HeaderMap, Json(body): Json<Value>| {
            let captured = captured.clone(); async move {
                assert_eq!(headers["authorization"], "Bearer fixture-google-access-1");
                assert!(headers.get("x-api-key").is_none());
                assert!(headers.get("x-extra-sensitive").is_none());
                assert_eq!(headers["anthropic-beta"], "fixture-beta");
                assert_eq!(body["anthropic_version"], "vertex-2023-10-16");
                assert!(uri.path().starts_with("/v1/projects/real-vertex-project/locations/global/"));
                captured.lock().await.push((uri.path().to_owned(), body.clone()));
                if uri.path().ends_with("count-tokens:rawPredict") {
                    assert_eq!(body["model"], "claude-haiku-4-5@20251001");
                    assert!(body.get("stream").is_none());
                    return Json(json!({"input_tokens":12})).into_response();
                }
                assert!(body.get("model").is_none());
                if body["stream"] == true {
                    ([("content-type", "text/event-stream")], "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_fixture\",\"usage\":{\"input_tokens\":12,\"output_tokens\":0}}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n").into_response()
                } else {
                    Json(json!({"id":"msg_fixture","type":"message","model":"claude-haiku-4-5@20251001",
                        "content":[{"type":"text","text":"fixture"}],"stop_reason":"end_turn",
                        "usage":{"input_tokens":12,"output_tokens":2}})).into_response()
                }
            }
        }));
        let vertex_server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let upstream = crate::backend::Upstream::new(
            "https://aiplatform.googleapis.com",
            PathBuf::new(),
            false,
            None,
            Duration::from_secs(5),
        )
        .unwrap()
        .with_vertex(Arc::new(vertex));
        let registry = dir.path().join("keys.json");
        std::fs::write(&registry, "{\"keys\":[]}").unwrap();
        let store = crate::telemetry::Store::open(&dir.path().join("db.sqlite")).unwrap();
        let app = crate::App {
            routing: None,
            inventory: None,
            project_policy: Default::default(),
            accounts: None,
            backend: crate::backend::Backend::Mock,
            upstreams: Arc::new(std::collections::BTreeMap::from([(
                "vertex-account".into(),
                crate::backend::Backend::Anthropic(upstream),
            )])),
            store: store.clone(),
            registry,
            admin_verifier: crate::auth::fingerprint("admin"),
            models: Arc::new(Default::default()),
            mock_adc: Some(Arc::new(
                crate::mock_adc::MockAdc::new("disabled".into(), "vertex-client-test".into(), 600)
                    .unwrap(),
            )),
        };
        async fn call(router: &Router, path: &str, key: &str, body: Value) -> (StatusCode, String) {
            let response = router
                .clone()
                .oneshot(
                    Request::post(path)
                        .header("authorization", format!("Bearer {key}"))
                        .header("anthropic-version", "2023-06-01")
                        .header("content-type", "application/json")
                        .header("anthropic-beta", "fixture-beta")
                        .header("x-extra-sensitive", "must-never-forward")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            (
                response.status(),
                String::from_utf8(
                    response
                        .into_body()
                        .collect()
                        .await
                        .unwrap()
                        .to_bytes()
                        .to_vec(),
                )
                .unwrap(),
            )
        }
        let admin = crate::admin(app.clone());
        assert_eq!(call(&admin, "/api/private/v1/projects", "admin",
            json!({"project_id":"vertex-client-test","display_name":"Vertex client","upstream_profile":"vertex-account"})).await.0, StatusCode::CREATED);
        let issued = call(&admin, "/api/private/v1/projects/vertex-client-test/credentials", "admin",
            json!({"client_id":"fixture-client","backends":["anthropic"],"catalog_models":true,"vertex_catalog_models":true,"mock_adc":true})).await;
        assert_eq!(issued.0, StatusCode::CREATED);
        let issued: Value = serde_json::from_str(&issued.1).unwrap();
        let key = issued["api_key"].as_str().unwrap();
        let exchange = crate::mock_adc::metadata(app.clone()).oneshot(Request::post("/v1/token").header("content-type","application/x-www-form-urlencoded")
            .body(Body::from(format!("grant_type=urn:ietf:params:oauth:grant-type:token-exchange&audience={}&scope={SCOPE}&requested_token_type=urn:ietf:params:oauth:token-type:access_token&subject_token_type=urn:ietf:params:oauth:token-type:access_token&subject_token={key}", crate::mock_adc::audience("vertex-client-test")))).unwrap()).await.unwrap();
        assert_eq!(exchange.status(), StatusCode::OK);
        let google: Value =
            serde_json::from_slice(&exchange.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        let client = crate::inference(app);
        for vertex in [false, true] {
            for operation in ["rawPredict", "streamRawPredict", "count_tokens"] {
                let counting = operation == "count_tokens";
                let mut body = json!({"messages":[{"role":"user","content":"fixture"}]});
                if !counting {
                    body["max_tokens"] = json!(4);
                }
                let path = if vertex {
                    body["anthropic_version"] = json!("vertex-2023-10-16");
                    if counting {
                        body["model"] = json!("claude-haiku-4-5@20251001");
                    }
                    format!(
                        "/v1/projects/vertex-client-test/locations/us-east5/publishers/anthropic/models/{}:{}",
                        if counting {
                            "count-tokens"
                        } else {
                            "claude-haiku-4-5@20251001"
                        },
                        if counting { "rawPredict" } else { operation }
                    )
                } else {
                    body["model"] = json!("claude-haiku-4-5-20251001");
                    if !counting {
                        body["stream"] = json!(operation == "streamRawPredict");
                    }
                    if counting {
                        "/v1/messages/count_tokens".into()
                    } else {
                        "/v1/messages".into()
                    }
                };
                let result = call(
                    &client,
                    &path,
                    if vertex {
                        google["access_token"].as_str().unwrap()
                    } else {
                        key
                    },
                    body,
                )
                .await;
                assert_eq!(result.0, StatusCode::OK, "{path}: {}", result.1);
                if operation == "streamRawPredict" {
                    assert!(result.1.contains("event: message_stop"));
                } else {
                    assert!(serde_json::from_str::<Value>(&result.1).is_ok());
                }
            }
        }
        assert_eq!(requests.lock().await.len(), 6);
        for (path, _) in requests.lock().await.iter() {
            assert!(!path.contains("vertex-client-test"));
        }
        assert_eq!(oauth_calls.load(Ordering::SeqCst), 1);
        store.flush().await;
        let counted: u64 = store.control(false, |c| c.query_row("SELECT count(*) FROM requests WHERE json_extract(payload,'$.completion')='complete' AND json_extract(payload,'$.upstream_auth')='google-service-account' AND json_extract(payload,'$.upstream_profile')='vertex-account' AND json_extract(payload,'$.project_id')='vertex-client-test'", [], |r| r.get(0)).map_err(|_| ApiError::internal())).await.unwrap();
        assert_eq!(counted, 6);
        oauth_server.abort();
        vertex_server.abort();
    }
}
