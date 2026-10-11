//! Project model destinations. Routing never grants model access or falls back.
use crate::{ApiError, App, backend::Backend, model_policy::Inventory};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Routing {
    #[serde(default)]
    pub default_upstream: Option<String>,
    #[serde(default)]
    pub model_overrides: BTreeMap<String, String>,
}
impl Routing {
    pub fn profiles(&self) -> BTreeSet<String> {
        self.default_upstream
            .iter()
            .chain(self.model_overrides.values())
            .cloned()
            .collect()
    }
    pub fn destination(&self, canonical_model: &str) -> Option<&str> {
        self.model_overrides
            .get(canonical_model)
            .or(self.default_upstream.as_ref())
            .map(String::as_str)
    }
    pub fn normalize(&mut self, app: &App) -> Result<(), ApiError> {
        if self.model_overrides.len() > 1000
            || self
                .profiles()
                .iter()
                .any(|id| !crate::projects::valid_id(id))
        {
            return Err(ApiError::bad("Invalid project routing"));
        }
        let mut normalized = BTreeMap::new();
        for (model, profile) in &self.model_overrides {
            if !crate::model_policy::valid_id(model) {
                return Err(ApiError::bad("Invalid route model ID"));
            }
            let canonical = crate::models::canonical(app, model);
            if normalized
                .insert(canonical, profile.clone())
                .is_some_and(|old| old != *profile)
            {
                return Err(ApiError::bad(
                    "Conflicting routes for the same canonical model",
                ));
            }
        }
        self.model_overrides = normalized;
        Ok(())
    }
}
#[derive(Clone)]
pub struct Target {
    pub backend: Option<Backend>,
    pub inventory: Inventory,
}
#[derive(Clone)]
pub struct Context {
    pub config: Routing,
    pub targets: BTreeMap<String, Target>,
    pub backends: Vec<String>,
}
pub fn backend(app: &App, profile: &str) -> Option<Backend> {
    if profile == "default" {
        Some(app.backend.clone())
    } else {
        app.upstreams
            .get(profile)
            .cloned()
            .or_else(|| app.accounts.as_ref().and_then(|m| m.backend(profile)))
    }
}
pub async fn context(
    app: &App,
    config: Routing,
    backends: Vec<String>,
) -> Result<Context, ApiError> {
    let profiles = config.profiles();
    let inventories = app
        .store
        .control(false, move |c| {
            profiles
                .into_iter()
                .map(|id| crate::model_policy::load(c, &id).map(|inventory| (id, inventory)))
                .collect::<Result<BTreeMap<_, _>, _>>()
        })
        .await?;
    let targets = inventories
        .into_iter()
        .map(|(id, inventory)| {
            let target = Target {
                backend: backend(app, &id),
                inventory,
            };
            (id, target)
        })
        .collect();
    Ok(Context {
        config,
        targets,
        backends,
    })
}
impl Context {
    pub fn target(&self, model: &str) -> Option<&Target> {
        self.config
            .destination(model)
            .and_then(|id| self.targets.get(id))
    }
    pub fn denial(&self, model: &str) -> Option<&'static str> {
        if self.config.destination(model).is_none() {
            return Some("unrouted");
        }
        let Some(target) = self.target(model) else {
            return Some("upstream_unavailable");
        };
        let Some(backend) = &target.backend else {
            return Some("upstream_unavailable");
        };
        if !self.backends.iter().any(|b| b == backend.name()) {
            return Some("backend_not_granted");
        }
        if !target.inventory.permits(model) {
            return Some("upstream_disabled");
        }
        None
    }
}
pub fn select(mut app: App, model: &str) -> Result<(App, String), ApiError> {
    let context = app.routing.take().ok_or_else(ApiError::internal)?;
    let model = crate::models::canonical(&app, model);
    let profile = context
        .config
        .destination(&model)
        .ok_or(ApiError(
            StatusCode::FORBIDDEN,
            "permission_error",
            "No upstream route for this model",
        ))?
        .to_owned();
    let target = context
        .targets
        .get(&profile)
        .ok_or_else(ApiError::internal)?;
    let backend = target.backend.clone().ok_or(ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "upstream_profile_unavailable",
        "Model upstream profile is unavailable",
    ))?;
    if !context.backends.iter().any(|b| b == backend.name()) {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "permission_error",
            "Model/backend access denied",
        ));
    }
    app.backend = backend;
    app.inventory = Some(target.inventory.clone());
    Ok((app, profile))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Model, auth, mock_adc::MockAdc};
    use axum::{
        Json, Router,
        body::Body,
        extract::State,
        http::{HeaderMap, Request},
        response::IntoResponse,
        routing::{get, post},
    };
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use std::sync::Arc;
    use tower::ServiceExt;
    const CLAUDE: &str = "claude-route-20261011";
    const GLM: &str = crate::model_policy::GLM;
    async fn call(
        router: &Router,
        method: &str,
        path: &str,
        key: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("authorization", format!("Bearer {key}"))
                    .header("anthropic-version", "2023-06-01")
                    .header("content-type", "application/json")
                    .body(if body.is_null() {
                        Body::empty()
                    } else {
                        Body::from(body.to_string())
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| json!({"raw":String::from_utf8_lossy(&bytes)})),
        )
    }
    async fn fixture(
        State(id): State<u64>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> axum::response::Response {
        if id == 2 {
            assert_eq!(headers["x-api-key"], "provider-token");
            assert_eq!(body["model"], GLM);
            assert_eq!(body["output_config"]["effort"], "medium");
        } else {
            assert_eq!(headers["authorization"], "Bearer provider-token");
        }
        if body["stream"] == true {
            let sse = format!(
                "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"provider-{id}\",\"usage\":{{\"input_tokens\":{id},\"output_tokens\":0}}}}}}\n\nevent: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":1}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
            );
            ([("content-type", "text/event-stream")], sse).into_response()
        } else {
            Json(json!({"id":format!("provider-{id}"),"type":"message","model":body["model"],"content":[{"type":"text","text":"OK"}],"usage":{"input_tokens":id,"output_tokens":1}})).into_response()
        }
    }
    async fn upstream(id: u64, dir: &std::path::Path) -> (Backend, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::new().route("/v1/messages",post(fixture))
            .route("/v1/messages/count_tokens",post(move |Json(body): Json<Value>| async move {
                if id == 2 { assert_eq!(body["model"],GLM); }
                Json(json!({"input_tokens":id}))
            }))
            .route("/v1/models",get(move || async move { Json(json!({"data":[
                {"id":CLAUDE,"display_name":format!("Claude from {id}"),"created_at":"2026-10-11T00:00:00Z","max_input_tokens":1000*id},
                {"id":GLM,"display_name":format!("GLM from {id}"),"created_at":"2026-10-11T00:00:00Z"}],"has_more":false})) }))
            .with_state(id);
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let token = dir.join(format!("provider-{id}"));
        std::fs::write(&token, "provider-token").unwrap();
        let backend = crate::backend::Upstream::new(
            &format!("http://{address}"),
            token,
            true,
            None,
            std::time::Duration::from_secs(3),
        )
        .unwrap();
        (
            Backend::Anthropic(if id == 2 {
                backend.with_enmaas()
            } else {
                backend
            }),
            server,
        )
    }
    #[tokio::test]
    async fn routes_all_interfaces_aliases_catalog_metadata_and_existing_vertex_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let (first, first_server) = upstream(1, dir.path()).await;
        let (second, second_server) = upstream(2, dir.path()).await;
        let registry = dir.path().join("keys.json");
        std::fs::write(&registry, r#"{"keys":[]}"#).unwrap();
        let app = App {
            routing: None,
            inventory: None,
            project_policy: Default::default(),
            accounts: None,
            backend: Backend::Mock,
            upstreams: Arc::new(BTreeMap::from([
                ("claude-account".into(), first),
                ("glm-account".into(), second),
            ])),
            store: crate::telemetry::Store::open(&dir.path().join("db.sqlite")).unwrap(),
            registry,
            admin_verifier: auth::fingerprint("admin"),
            mock_adc: Some(Arc::new(
                MockAdc::new("disabled".into(), "routing-test".into(), 600).unwrap(),
            )),
            models: Arc::new(BTreeMap::from([(
                "glm-alias".into(),
                Model {
                    window_tokens: None,
                    upstream_model: Some(GLM.into()),
                },
            )])),
        };
        let mut inventory = Inventory::enmaas();
        inventory.models[0].display_name = Some("Personal GLM".into());
        app.store
            .control(true, move |c| {
                crate::model_policy::save(c, "glm-account", &inventory)
            })
            .await
            .unwrap();
        let admin = crate::admin(app.clone());
        let client = crate::inference(app.clone());
        let project_path = "/api/private/v1/projects/routing-test";
        let mut project = json!({"project_id":"routing-test","display_name":"Routing","routing":{"default_upstream":"claude-account","model_overrides":{"glm-alias":"glm-account"}}});
        let saved = call(
            &admin,
            "POST",
            "/api/private/v1/projects",
            "admin",
            project.clone(),
        )
        .await;
        assert_eq!(saved.0, StatusCode::CREATED);
        assert_eq!(saved.1["routing"]["model_overrides"][GLM], "glm-account");
        let issued = call(&admin,"POST","/api/private/v1/projects/routing-test/credentials","admin",json!({"client_id":"test","backends":["anthropic"],"catalog_models":true,"vertex_catalog_models":true,"mock_adc":true})).await.1;
        let key = issued["api_key"].as_str().unwrap();
        let exchange = crate::mock_adc::metadata(app.clone()).oneshot(Request::post("/v1/token").header("content-type","application/x-www-form-urlencoded")
            .body(Body::from(format!("grant_type=urn:ietf:params:oauth:grant-type:token-exchange&audience={}&scope=https://www.googleapis.com/auth/cloud-platform&requested_token_type=urn:ietf:params:oauth:token-type:access_token&subject_token_type=urn:ietf:params:oauth:token-type:access_token&subject_token={key}",crate::mock_adc::audience("routing-test")))).unwrap()).await.unwrap();
        assert_eq!(exchange.status(), StatusCode::OK);
        let google: Value =
            serde_json::from_slice(&exchange.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        let google = google["access_token"].as_str().unwrap();
        let models = call(&client, "GET", "/v1/models", key, Value::Null).await;
        assert_eq!(models.0, StatusCode::OK);
        let models = models.1["data"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(
            models.iter().find(|m| m["id"] == GLM).unwrap()["display_name"],
            "Personal GLM"
        );
        assert_eq!(
            models.iter().find(|m| m["id"] == CLAUDE).unwrap()["max_input_tokens"],
            1000
        );
        for (model, provider) in [(CLAUDE, 1), (GLM, 2), ("glm-alias", 2)] {
            let body =
                json!({"model":model,"max_tokens":16,"messages":[{"role":"user","content":"OK"}]});
            let generated = call(&client, "POST", "/v1/messages", key, body.clone()).await;
            assert_eq!(generated.0, StatusCode::OK, "{model}: {}", generated.1);
            assert_eq!(generated.1["id"], format!("provider-{provider}"));
            assert_eq!(
                call(
                    &client,
                    "POST",
                    "/v1/messages/count_tokens",
                    key,
                    body.clone()
                )
                .await
                .1["input_tokens"],
                provider
            );
            let vertex_model = if model == CLAUDE {
                "claude-route@20261011"
            } else {
                model
            };
            let vertex_path = format!(
                "/v1/projects/routing-test/locations/global/publishers/anthropic/models/{vertex_model}"
            );
            let vertex_body = json!({"anthropic_version":"vertex-2023-10-16","max_tokens":16,"messages":body["messages"]});
            let response = call(
                &client,
                "POST",
                &(vertex_path.clone() + ":rawPredict"),
                google,
                vertex_body.clone(),
            )
            .await;
            assert_eq!(response.0, StatusCode::OK, "{}", response.1);
            assert_eq!(response.1["id"], format!("provider-{provider}"));
            let streamed = call(
                &client,
                "POST",
                &(vertex_path + ":streamRawPredict"),
                google,
                vertex_body,
            )
            .await;
            assert_eq!(streamed.0, StatusCode::OK);
            assert!(
                streamed.1["raw"]
                    .as_str()
                    .unwrap()
                    .contains(&format!("provider-{provider}"))
            );
            let mut stream_body = body.clone();
            stream_body["stream"] = json!(true);
            let streamed = call(&client, "POST", "/v1/messages", key, stream_body).await;
            assert_eq!(streamed.0, StatusCode::OK);
            assert!(
                streamed.1["raw"]
                    .as_str()
                    .unwrap()
                    .contains(&format!("provider-{provider}"))
            );
            assert_eq!(call(&client,"POST","/v1/projects/routing-test/locations/global/publishers/anthropic/models/count-tokens:rawPredict",google,
                json!({"anthropic_version":"vertex-2023-10-16","model":vertex_model,"messages":body["messages"]})).await.1["input_tokens"],provider);
        }
        // Move GLM to the default while retaining a Google token issued before this change.
        project["routing"]["model_overrides"] = json!({});
        assert_eq!(
            call(&admin, "PUT", project_path, "admin", project.clone())
                .await
                .0,
            StatusCode::OK
        );
        let vertex_path = format!(
            "/v1/projects/routing-test/locations/global/publishers/anthropic/models/{GLM}:rawPredict"
        );
        let body = json!({"anthropic_version":"vertex-2023-10-16","max_tokens":16,"messages":[{"role":"user","content":"OK"}]});
        assert_eq!(
            call(&client, "POST", &vertex_path, google, body.clone())
                .await
                .1["id"],
            "provider-1"
        );
        // Strict mode denies models that have no explicit destination, including catalogs.
        project["routing"] = json!({"default_upstream":null,"model_overrides":{GLM:"glm-account"}});
        assert_eq!(
            call(&admin, "PUT", project_path, "admin", project.clone())
                .await
                .0,
            StatusCode::OK
        );
        assert_eq!(
            call(
                &client,
                "POST",
                "/v1/messages/count_tokens",
                key,
                json!({"model":CLAUDE,"messages":body["messages"]})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(&client, "GET", "/v1/models", key, Value::Null).await.1["data"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let exchange = crate::mock_adc::metadata(app.clone()).oneshot(Request::post("/v1/token").header("content-type","application/x-www-form-urlencoded")
            .body(Body::from(format!("grant_type=urn:ietf:params:oauth:grant-type:token-exchange&audience={}&scope=https://www.googleapis.com/auth/cloud-platform&requested_token_type=urn:ietf:params:oauth:token-type:access_token&subject_token_type=urn:ietf:params:oauth:token-type:access_token&subject_token={key}",crate::mock_adc::audience("routing-test")))).unwrap()).await.unwrap();
        assert_eq!(
            exchange.status(),
            StatusCode::OK,
            "Explicit-only projects support model-less token exchange"
        );
        let preview = call(
            &admin,
            "POST",
            "/api/private/v1/model-access",
            "admin",
            json!({"routing":project["routing"],"models":[CLAUDE,GLM],"catalog_models":true}),
        )
        .await;
        assert_eq!(preview.0, StatusCode::OK);
        assert_eq!(
            preview.1["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["id"] == CLAUDE)
                .unwrap()["reason"],
            "unrouted"
        );
        // Omitted routing on a legacy editor's PUT preserves current routes.
        let mut legacy = project.clone();
        legacy.as_object_mut().unwrap().remove("routing");
        assert_eq!(
            call(&admin, "PUT", project_path, "admin", legacy).await.1["routing"],
            project["routing"]
        );
        // Policy block aliases cannot bypass the canonical route.
        project["model_policy"] = json!({"blocked":["glm-alias"]});
        assert_eq!(
            call(&admin, "PUT", project_path, "admin", project.clone())
                .await
                .0,
            StatusCode::OK
        );
        assert_eq!(
            call(&client, "POST", &vertex_path, google, body.clone())
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        project["model_policy"] = json!({});
        call(&admin, "PUT", project_path, "admin", project.clone()).await;
        // Unavailable override is an error; a live default must never receive that model.
        project["routing"]["default_upstream"] = json!("claude-account");
        call(&admin, "PUT", project_path, "admin", project.clone()).await;
        let mut missing = app.clone();
        missing.upstreams = Arc::new(BTreeMap::from([(
            "claude-account".into(),
            app.upstreams["claude-account"].clone(),
        )]));
        assert_eq!(
            call(
                &crate::inference(missing),
                "POST",
                &vertex_path,
                google,
                body.clone()
            )
            .await
            .0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        // Reject conflicting aliases and nonexistent accounts at configuration time.
        project["routing"]["model_overrides"] =
            json!({GLM:"glm-account","glm-alias":"claude-account"});
        assert_eq!(
            call(&admin, "PUT", project_path, "admin", project.clone())
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        project["routing"]["model_overrides"] = json!({GLM:"missing-account"});
        assert_eq!(
            call(&admin, "PUT", project_path, "admin", project).await.0,
            StatusCode::BAD_REQUEST
        );
        // Per-token canonical blocks and backend grants still constrain routing.
        let credential_path = format!(
            "/api/private/v1/projects/routing-test/credentials/{}",
            issued["key_id"].as_str().unwrap()
        );
        assert_eq!(call(&admin,"PUT",&credential_path,"admin",json!({"client_id":"test","backends":["anthropic"],"catalog_models":true,"vertex_catalog_models":true,"mock_adc":true,"blocked_models":["glm-alias"]})).await.0,StatusCode::OK);
        assert_eq!(
            call(&client, "POST", &vertex_path, google, body.clone())
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(&client, "GET", "/v1/models", key, Value::Null).await.1["data"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        call(&admin,"PUT",&credential_path,"admin",json!({"client_id":"test","backends":["anthropic"],"catalog_models":true,"vertex_catalog_models":true,"mock_adc":true,"blocked_models":[]})).await;
        let route = json!({"project_id":"routing-test","display_name":"Routing","routing":{"default_upstream":"claude-account","model_overrides":{GLM:"default"}}});
        assert_eq!(
            call(&admin, "PUT", project_path, "admin", route).await.0,
            StatusCode::OK
        );
        assert_eq!(
            call(&client, "POST", &vertex_path, google, body).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(&client, "GET", "/v1/models", key, Value::Null).await.1["data"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        app.store.flush().await;
        let telemetry = app
            .store
            .read(|c| {
                crate::telemetry::Filter {
                    project_id: Some("routing-test".into()),
                    ..Default::default()
                }
                .list(c)
            })
            .await
            .unwrap();
        let records = telemetry["items"].as_array().unwrap();
        assert!(
            records
                .iter()
                .any(|r| r["resolved_model"] == GLM && r["upstream_profile"] == "glm-account")
        );
        assert!(records.iter().any(|r| r["resolved_model"] == CLAUDE && r["upstream_profile"] == "claude-account"));
        first_server.abort();
        second_server.abort();
    }
}
