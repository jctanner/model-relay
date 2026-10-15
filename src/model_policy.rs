//! Persisted per-profile inventory and exact-ID project policy.
use crate::{ApiError, App, backend::Backend, models::CatalogModel};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;

pub const GLM: &str = "rits/zai-org/glm-5-3";
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 200
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-@".contains(&b))
        && id
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectPolicy {
    /// None inherits the upstream; Some([]) denies all.
    #[serde(default)]
    pub allowed: Option<Vec<String>>,
    #[serde(default)]
    pub blocked: Vec<String>,
}
impl ProjectPolicy {
    pub fn validate(&self) -> bool {
        self.allowed.as_ref().is_none_or(|v| valid_ids(v)) && valid_ids(&self.blocked)
    }
    pub fn permits(&self, id: &str) -> bool {
        !self.blocked.iter().any(|s| s == id)
            && self
                .allowed
                .as_ref()
                .is_none_or(|v| v.iter().any(|s| s == id))
    }
}
pub fn valid_ids(ids: &[String]) -> bool {
    ids.len() <= 1000 && ids.iter().all(|id| valid_id(id))
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub id: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub discovered: Option<CatalogModel>,
    #[serde(default)]
    pub manual: bool,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub max_input_tokens: Option<u64>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
}
impl Entry {
    fn metadata(&self) -> CatalogModel {
        let mut model = self.discovered.clone().unwrap_or(CatalogModel {
            id: self.id.clone(),
            display_name: self.id.clone(),
            created_at: String::new(),
            max_input_tokens: None,
            max_tokens: None,
            line: None,
            lifecycle: None,
        });
        if let Some(name) = &self.display_name {
            model.display_name = name.clone();
        }
        model.max_input_tokens = self.max_input_tokens.or(model.max_input_tokens);
        model.max_tokens = self.max_tokens.or(model.max_tokens);
        model
    }
}
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Inventory {
    #[serde(default)]
    pub version: u64,
    #[serde(default)]
    pub configured: bool,
    #[serde(default)]
    pub models: Vec<Entry>,
    #[serde(default)]
    pub refreshed_at: Option<String>,
    #[serde(default)]
    pub refresh_error: Option<String>,
}
impl Inventory {
    pub fn vertex() -> Self {
        Self {
            configured: true,
            models: [
                ("claude-opus-4-6", "Claude Opus 4.6"),
                ("claude-sonnet-4-6", "Claude Sonnet 4.6"),
                ("claude-haiku-4-5-20251001", "Claude Haiku 4.5"),
            ]
            .into_iter()
            .map(|(id, name)| Entry {
                id: id.into(),
                enabled: true,
                manual: true,
                display_name: Some(name.into()),
                discovered: None,
                max_input_tokens: None,
                max_tokens: None,
            })
            .collect(),
            ..Self::default()
        }
    }
    pub fn enmaas() -> Self {
        Self {
            configured: true,
            models: vec![Entry {
                id: GLM.into(),
                enabled: true,
                manual: true,
                display_name: Some("GLM 5.3".into()),
                discovered: None,
                max_input_tokens: None,
                max_tokens: None,
            }],
            ..Self::default()
        }
    }
    pub fn permits(&self, id: &str) -> bool {
        !self.configured
            || self.models.iter().any(|m| {
                m.id == id
                    && m.enabled
                    && (m.manual || m.discovered.is_some())
                    && m.discovered.as_ref().and_then(|m| m.lifecycle.as_deref()) != Some("retired")
            })
    }
    pub fn catalog(&self) -> Vec<CatalogModel> {
        self.models
            .iter()
            .filter(|m| self.permits(&m.id) && (m.manual || m.discovered.is_some()))
            .map(Entry::metadata)
            .collect()
    }
}
fn sql<T>(r: rusqlite::Result<T>) -> Result<T, ApiError> {
    r.map_err(|_| ApiError::internal())
}
pub fn load(c: &rusqlite::Connection, profile: &str) -> Result<Inventory, ApiError> {
    let value: Option<String> = sql(c
        .query_row(
            "SELECT payload FROM model_inventories WHERE profile_id=?1",
            [profile],
            |r| r.get(0),
        )
        .optional())?;
    value
        .map(|v| serde_json::from_str(&v).map_err(|_| ApiError::internal()))
        .transpose()
        .map(Option::unwrap_or_default)
}
pub fn save(
    c: &rusqlite::Connection,
    profile: &str,
    inventory: &Inventory,
) -> Result<(), ApiError> {
    sql(c.execute("INSERT INTO model_inventories(profile_id,payload) VALUES(?1,?2) ON CONFLICT(profile_id) DO UPDATE SET payload=excluded.payload",
        rusqlite::params![profile, json!(inventory).to_string()]))?;
    Ok(())
}
fn select(app: &App, id: &str) -> Result<App, ApiError> {
    let mut selected = app.clone();
    selected.inventory = None;
    selected.routing = None;
    selected.backend = if id == "default" {
        app.backend.clone()
    } else {
        app.upstreams
            .get(id)
            .cloned()
            .or_else(|| app.accounts.as_ref().and_then(|m| m.backend(id)))
            .ok_or(ApiError(
                StatusCode::NOT_FOUND,
                "not_found_error",
                "Unknown upstream profile",
            ))?
    };
    Ok(selected)
}
pub fn routes() -> Router<App> {
    Router::new()
        .route("/api/private/v1/model-access", post(access))
        .route(
            "/api/private/v1/upstream-profiles/{profile}/models/available",
            get(available),
        )
        .route(
            "/api/private/v1/upstream-profiles/{profile}/models",
            get(detail).put(update),
        )
        .route(
            "/api/private/v1/upstream-profiles/{profile}/models/refresh",
            post(refresh),
        )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AccessInput {
    #[serde(default)]
    profile_id: String,
    #[serde(default)]
    routing: Option<crate::routing::Routing>,
    #[serde(default)]
    backends: Option<Vec<String>>,
    #[serde(default)]
    project_policy: ProjectPolicy,
    #[serde(default)]
    models: Vec<String>,
    #[serde(default)]
    blocked_models: Vec<String>,
    #[serde(default)]
    catalog_models: bool,
    #[serde(default)]
    vertex_catalog_models: bool,
}
async fn access(
    State(app): State<App>,
    Json(input): Json<AccessInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !input.project_policy.validate()
        || !valid_ids(&input.models)
        || !valid_ids(&input.blocked_models)
    {
        return Err(ApiError::bad("Invalid model policy"));
    }
    let mut selected = app.clone();
    let mut config = input.routing.unwrap_or_else(|| crate::routing::Routing {
        default_upstream: Some(input.profile_id),
        model_overrides: BTreeMap::new(),
    });
    config.normalize(&app)?;
    let context = crate::routing::context(
        &app,
        config,
        input
            .backends
            .unwrap_or_else(|| vec!["mock".into(), "anthropic".into()]),
    )
    .await?;
    let mut ids: std::collections::BTreeSet<String> = context
        .targets
        .values()
        .flat_map(|t| t.inventory.models.iter().map(|m| m.id.clone()))
        .collect();
    ids.extend(context.config.model_overrides.keys().cloned());
    selected.routing = Some(context);
    selected.inventory = None;
    selected.project_policy = input.project_policy;
    let key: crate::auth::Key = serde_json::from_value(json!({"client_id":"preview", "key_id":"preview", "sha256":"",
        "models":input.models, "blocked_models":input.blocked_models, "backends":[],
        "catalog_models":input.catalog_models, "vertex_catalog_models":input.vertex_catalog_models})).map_err(|_| ApiError::internal())?;
    ids.extend(selected.models.keys().cloned());
    ids.extend(key.models.iter().chain(key.blocked_models.iter()).cloned());
    ids.extend(
        selected
            .project_policy
            .allowed
            .iter()
            .flatten()
            .chain(selected.project_policy.blocked.iter())
            .cloned(),
    );
    let canonical = |id: &str| crate::models::canonical(&selected, id);
    let items: Vec<_> = ids.into_iter().map(|id| {
        let target = canonical(&id);
        let reason = if let Some(reason) = selected.routing.as_ref().and_then(|r| r.denial(&target)) { Some(reason) }
            else if selected.project_policy.blocked.iter().any(|s| canonical(s) == target) { Some("project_blocked") }
            else if selected.project_policy.allowed.as_ref().is_some_and(|v| !v.iter().any(|s| canonical(s) == target)) { Some("project_not_allowed") }
            else if key.blocked_models.iter().any(|s| canonical(s) == target) { Some("token_blocked") }
            else { None };
        let granted = key.models.iter().any(|s| canonical(s) == target);
        json!({"id":id, "canonical_id":target, "upstream_profile":selected.routing.as_ref().and_then(|r| r.config.destination(&target)), "messages": reason.is_none() && (granted || key.catalog_models),
            "vertex": reason.is_none() && (granted || key.vertex_catalog_models),
            "reason":reason.or(if !granted && !key.catalog_models && !key.vertex_catalog_models { Some("token_not_granted") } else { None })})
    }).collect();
    Ok(Json(json!({"items":items})))
}
async fn available(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut selected = select(&app, &id)?;
    selected.inventory = Some(app.store.control(false, move |c| load(c, &id)).await?);
    Ok(Json(
        json!({"items": crate::models::catalog(&selected).await?}),
    ))
}
async fn detail(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<Inventory>, ApiError> {
    select(&app, &id)?;
    app.store
        .control(false, move |c| Ok(Json(load(c, &id)?)))
        .await
}
async fn update(
    State(app): State<App>,
    Path(id): Path<String>,
    Json(mut input): Json<Inventory>,
) -> Result<Json<Inventory>, ApiError> {
    let selected = select(&app, &id)?;
    if matches!(&selected.backend, Backend::Anthropic(u) if u.provider == "vertex") {
        for model in &mut input.models {
            model.id = crate::models::canonical(&app, &model.id);
            if model.enabled {
                crate::vertex::model_id(&model.id)?;
            }
        }
    }
    let mut ids = std::collections::BTreeSet::new();
    if input.models.len() > 1000
        || input.models.iter().any(|m| {
            !valid_id(&m.id)
                || !ids.insert(&m.id)
                || !m.metadata().validate()
                || m.discovered
                    .as_ref()
                    .is_some_and(|d| d.id != m.id || !d.validate())
                || m.max_input_tokens == Some(0)
                || m.max_tokens == Some(0)
        })
    {
        return Err(ApiError::bad("Invalid model inventory"));
    }
    // EnMaaS deliberately exposes only the verified hosted GLM ID.
    if matches!(&select(&app, &id)?.backend, Backend::Anthropic(u) if u.provider == "enmaas")
        && input.models.iter().any(|m| m.enabled && m.id != GLM)
    {
        return Err(ApiError::bad("EnMaaS permits only rits/zai-org/glm-5-3"));
    }
    app.store
        .control(true, move |c| {
            let current = load(c, &id)?;
            if current.version != input.version {
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    "conflict_error",
                    "Model inventory changed; reload before saving",
                ));
            }
            // Discovery is server-owned. Editors may change only policy and overrides.
            for entry in &mut input.models {
                entry.discovered = current
                    .models
                    .iter()
                    .find(|m| m.id == entry.id)
                    .and_then(|m| m.discovered.clone());
            }
            input.refreshed_at = current.refreshed_at;
            input.refresh_error = current.refresh_error;
            input.configured = true;
            input.version += 1;
            save(c, &id, &input)?;
            Ok(Json(input))
        })
        .await
}
async fn refresh(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<Inventory>, ApiError> {
    let selected = select(&app, &id)?;
    if matches!(&selected.backend, Backend::Anthropic(u) if u.provider == "vertex") {
        return Err(ApiError::bad(
            "Vertex uses manual model inventory; edit Models to add or enable Claude IDs",
        ));
    }
    if let Backend::Anthropic(u) = &selected.backend {
        u.clear_catalog().await;
    }
    let result = crate::models::catalog(&selected).await;
    // Merge against the latest policy inside the transaction, never a stale editor snapshot.
    app.store
        .control(true, move |c| {
            let mut inventory = load(c, &id)?;
            inventory.version += 1;
            inventory.refreshed_at = Some(chrono::Utc::now().to_rfc3339());
            match result {
                Err(e) => inventory.refresh_error = Some(e.1.into()),
                Ok(models) => {
                    inventory.refresh_error = None;
                    for entry in &mut inventory.models {
                        entry.discovered = None;
                    }
                    let mut entries: BTreeMap<_, _> = inventory
                        .models
                        .into_iter()
                        .map(|m| (m.id.clone(), m))
                        .collect();
                    for model in models {
                        let id = model.id.clone();
                        let entry = entries.entry(id.clone()).or_insert_with(|| Entry {
                            id,
                            enabled: false,
                            discovered: None,
                            manual: false,
                            display_name: None,
                            max_input_tokens: None,
                            max_tokens: None,
                        });
                        entry.discovered = Some(model);
                    }
                    inventory.models = entries.into_values().collect();
                }
            }
            save(c, &id, &inventory)?;
            Ok(Json(inventory))
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{HeaderMap, Request},
        response::IntoResponse,
    };
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use tower::ServiceExt;

    async fn call(
        router: &Router,
        method: &str,
        path: &str,
        key: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("authorization", format!("Bearer {key}"))
                    .header("content-type", "application/json")
                    .header("anthropic-version", "2023-06-01")
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
        let value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"raw":String::from_utf8_lossy(&bytes)}));
        (status, value)
    }
    fn app(dir: &std::path::Path, backend: Backend) -> App {
        let registry = dir.join("keys.json");
        std::fs::write(&registry, r#"{"keys":[]}"#).unwrap();
        App {
            routing: None,
            inventory: None,
            project_policy: Default::default(),
            accounts: None,
            upstreams: Arc::new(BTreeMap::from([("gateway".into(), backend)])),
            backend: Backend::Mock,
            store: crate::telemetry::Store::open(&dir.join("db.sqlite")).unwrap(),
            registry,
            admin_verifier: crate::auth::fingerprint("admin"),
            mock_adc: None,
            models: Arc::new(BTreeMap::from([(
                "glm-alias".into(),
                crate::Model {
                    window_tokens: None,
                    upstream_model: Some(GLM.into()),
                },
            )])),
        }
    }

    #[tokio::test]
    async fn policies_enforce_both_interfaces_counting_aliases_and_live_updates() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handler = |headers: HeaderMap, Json(body): Json<serde_json::Value>| async move {
            assert_eq!(headers["x-api-key"], "provider-secret");
            assert!(!headers.contains_key("authorization"));
            assert!(!headers.contains_key("anthropic-beta"));
            assert_eq!(body["model"], GLM);
            assert_eq!(body["output_config"]["effort"], "medium");
            if body["stream"] == true {
                ([("content-type", "text/event-stream")], concat!(
                    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"test\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
                    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tool\",\"name\":\"check\",\"input\":{}}}\n\n",
                    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\n",
                    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":1}}\n\n",
                    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
                )).into_response()
            } else {
                Json(json!({"type":"message","id":"test","model":GLM,"content":[{"type":"text","text":"OK"}],"usage":{"input_tokens":1,"output_tokens":1}})).into_response()
            }
        };
        let upstream_router = Router::new()
            .route("/v1/messages", post(handler))
            .route(
                "/v1/messages/count_tokens",
                post(|Json(body): Json<serde_json::Value>| async move {
                    assert_eq!(body["model"], GLM);
                    assert!(body.get("stream").is_none());
                    Json(json!({"input_tokens":3}))
                }),
            )
            .route(
                "/v1/models",
                get(|| async { Json(json!({"data":[{"id":"unapproved-new-model"}]})) }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, upstream_router).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let token = dir.path().join("token");
        std::fs::write(&token, "provider-secret").unwrap();
        let backend = Backend::Anthropic(
            crate::backend::Upstream::new(
                &format!("http://{address}"),
                token,
                true,
                None,
                std::time::Duration::from_secs(3),
            )
            .unwrap()
            .with_enmaas(),
        );
        let app = app(dir.path(), backend);
        app.store
            .control(true, |c| save(c, "gateway", &Inventory::enmaas()))
            .await
            .unwrap();
        let admin = crate::admin(app.clone());
        let client = crate::inference(app.clone());
        let project =
            json!({"project_id":"glm-test","display_name":"GLM","upstream_profile":"gateway"});
        assert_eq!(
            call(
                &admin,
                "POST",
                "/api/private/v1/projects",
                "admin",
                project.clone()
            )
            .await
            .0,
            StatusCode::CREATED
        );
        let issued = call(
            &admin,
            "POST",
            "/api/private/v1/projects/glm-test/credentials",
            "admin",
            json!({"client_id":"test","models":[GLM],"backends":["anthropic"]}),
        )
        .await
        .1;
        let key = issued["api_key"].as_str().unwrap();
        let body = json!({"model":GLM,"max_tokens":16,"messages":[{"role":"user","content":"OK"}],"output_config":{"effort":"high"}});
        let vertex = format!(
            "/v1/projects/glm-test/locations/global/publishers/anthropic/models/{GLM}:rawPredict"
        );
        assert_eq!(
            call(&client, "POST", "/v1/messages", key, body.clone())
                .await
                .0,
            StatusCode::OK
        );
        assert_eq!(call(&client,"POST",&vertex,key,json!({"anthropic_version":"vertex-2023-10-16","max_tokens":16,"messages":body["messages"]})).await.0, StatusCode::OK);
        assert_eq!(
            call(
                &client,
                "GET",
                "/v1/models/rits/zai-org/glm-5-3",
                key,
                json!(null)
            )
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
                json!({"model":GLM,"messages":body["messages"]})
            )
            .await
            .1["input_tokens"],
            3
        );
        let mut streamed = body.clone();
        streamed["stream"] = json!(true);
        let stream = call(&client, "POST", "/v1/messages", key, streamed).await;
        assert_eq!(stream.0, StatusCode::OK);
        assert!(
            stream.1["raw"]
                .as_str()
                .unwrap()
                .contains("input_json_delta")
        );
        let vertex_stream = vertex.replace(":rawPredict", ":streamRawPredict");
        let stream = call(&client, "POST", &vertex_stream, key, json!({"anthropic_version":"vertex-2023-10-16","max_tokens":16,"messages":body["messages"]})).await;
        assert_eq!(stream.0, StatusCode::OK);
        assert!(
            stream.1["raw"]
                .as_str()
                .unwrap()
                .contains("input_json_delta")
        );
        assert_eq!(call(&client,"POST","/v1/projects/glm-test/locations/global/publishers/anthropic/models/count-tokens:rawPredict",key,json!({"anthropic_version":"vertex-2023-10-16","model":GLM,"messages":body["messages"]})).await.1["input_tokens"],3);
        let mut inventory = call(
            &admin,
            "POST",
            "/api/private/v1/upstream-profiles/gateway/models/refresh",
            "admin",
            json!(null),
        )
        .await
        .1;
        assert_eq!(inventory["models"].as_array().unwrap().len(), 2);
        assert!(
            !inventory["models"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["id"] == "unapproved-new-model")
                .unwrap()["enabled"]
                .as_bool()
                .unwrap()
        );
        let stale = inventory.clone();
        for m in inventory["models"].as_array_mut().unwrap() {
            m["enabled"] = json!(false);
        }
        assert_eq!(
            call(
                &admin,
                "PUT",
                "/api/private/v1/upstream-profiles/gateway/models",
                "admin",
                inventory.clone()
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            call(
                &admin,
                "PUT",
                "/api/private/v1/upstream-profiles/gateway/models",
                "admin",
                stale
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        for (path, request) in [
            ("/v1/messages", body.clone()),
            ("/v1/messages/count_tokens", body.clone()),
            (
                vertex.as_str(),
                json!({"anthropic_version":"vertex-2023-10-16","max_tokens":16,"messages":body["messages"]}),
            ),
        ] {
            assert_eq!(
                call(&client, "POST", path, key, request).await.0,
                StatusCode::FORBIDDEN
            );
        }
        assert_eq!(
            call(&client, "GET", "/v1/models", key, json!(null)).await.1["data"],
            json!([])
        );
        let saved: Inventory = app
            .store
            .control(false, |c| load(c, "gateway"))
            .await
            .unwrap();
        assert!(saved.configured);
        assert!(!saved.permits(GLM));
        let mut enabled = saved;
        enabled
            .models
            .iter_mut()
            .find(|m| m.id == GLM)
            .unwrap()
            .enabled = true;
        assert_eq!(
            call(
                &admin,
                "PUT",
                "/api/private/v1/upstream-profiles/gateway/models",
                "admin",
                json!(enabled)
            )
            .await
            .0,
            StatusCode::OK
        );
        for policy in [
            json!({"allowed":[]}),
            json!({"allowed":[GLM],"blocked":["glm-alias"]}),
        ] {
            let mut p = project.clone();
            p["model_policy"] = policy;
            assert_eq!(
                call(
                    &admin,
                    "PUT",
                    "/api/private/v1/projects/glm-test",
                    "admin",
                    p
                )
                .await
                .0,
                StatusCode::OK
            );
            assert_eq!(
                call(&client, "POST", "/v1/messages", key, body.clone())
                    .await
                    .0,
                StatusCode::FORBIDDEN
            );
            assert_eq!(
                call(
                    &client,
                    "GET",
                    "/v1/models?client_api=vertex",
                    key,
                    json!(null)
                )
                .await
                .1["data"],
                json!([])
            );
        }
        assert_eq!(
            call(
                &admin,
                "PUT",
                "/api/private/v1/projects/glm-test",
                "admin",
                {
                    let mut p = project;
                    p["model_policy"] = json!({"allowed":null,"blocked":[]});
                    p
                }
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            call(&client, "POST", "/v1/messages", key, body.clone())
                .await
                .0,
            StatusCode::OK
        );
        let keypath = format!(
            "/api/private/v1/projects/glm-test/credentials/{}",
            issued["key_id"].as_str().unwrap()
        );
        assert_eq!(call(&admin,"PUT",&keypath,"admin",json!({"client_id":"test","catalog_models":true,"vertex_catalog_models":true,"blocked_models":["glm-alias"],"backends":["anthropic"]})).await.0,StatusCode::OK);
        assert_eq!(
            call(&client, "POST", "/v1/messages", key, body).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(&client, "GET", "/v1/models", key, json!(null)).await.1["data"],
            json!([])
        );
        // Legacy editors cannot accidentally clear new restrictions by omission.
        assert_eq!(call(&admin,"PUT",&keypath,"admin",json!({"client_id":"test","catalog_models":true,"vertex_catalog_models":true,"backends":["anthropic"]})).await.0,StatusCode::OK);
        assert_eq!(
            call(&client, "GET", "/v1/models", key, json!(null)).await.1["data"],
            json!([])
        );
        server.abort();
    }

    #[test]
    fn legacy_inheritance_empty_policy_and_namespaced_ids() {
        assert!(Inventory::default().permits(GLM));
        assert!(
            !Inventory {
                configured: true,
                ..Inventory::default()
            }
            .permits(GLM)
        );
        assert!(ProjectPolicy::default().permits(GLM));
        assert!(
            !ProjectPolicy {
                allowed: Some(vec![]),
                blocked: vec![]
            }
            .permits(GLM)
        );
        assert!(valid_id(GLM));
        for id in ["", "../model", "a//b", "https://x", "a?secret"] {
            assert!(!valid_id(id));
        }
    }
}
