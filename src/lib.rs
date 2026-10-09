pub mod auth;
pub mod backend;
pub mod credentials;
pub mod logging;
pub mod mock_adc;
pub mod models;
pub mod projects;
pub mod streaming;
pub mod telemetry;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response, Sse, sse::Event},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, convert::Infallible, path::PathBuf, sync::Arc, time::Instant};
use telemetry::{Filter, Record, Store};

#[derive(Debug)]
pub struct ApiError(pub StatusCode, pub &'static str, pub &'static str);
impl ApiError {
    pub fn bad(message: &'static str) -> Self {
        Self(StatusCode::BAD_REQUEST, "invalid_request_error", message)
    }
    pub fn unauthorized() -> Self {
        Self(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "Invalid or missing local key",
        )
    }
    pub fn internal() -> Self {
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            "Internal service error",
        )
    }
    pub fn busy() -> Self {
        Self(
            StatusCode::SERVICE_UNAVAILABLE,
            "overloaded_error",
            "Query capacity reached",
        )
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.0,
            Json(json!({"type":"error","error":{"type":self.1,"message":self.2}})),
        )
            .into_response()
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Model {
    pub window_tokens: Option<u64>,
    #[serde(default)]
    pub upstream_model: Option<String>,
}

#[derive(Clone)]
pub struct App {
    pub upstreams: Arc<BTreeMap<String, backend::Backend>>,
    pub backend: backend::Backend,
    pub store: Store,
    pub registry: PathBuf,
    pub admin_verifier: String,
    pub models: Arc<BTreeMap<String, Model>>,
    pub mock_adc: Option<Arc<mock_adc::MockAdc>>,
}

pub fn inference(app: App) -> Router {
    Router::new()
        .route(
            "/health",
            get(|State(app):State<App>| async move {
                Json(json!({"status":"ready","backend":app.backend.name(),"synthetic":app.backend.synthetic(),"version":env!("CARGO_PKG_VERSION"),"build_id":option_env!("SHIM_BUILD_ID").unwrap_or("unknown")}))
            }),
        )
        .route(
            "/v1/projects/{project}/locations/{region}/publishers/anthropic/models/{operation}",
            post(predict),
        )
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_messages))
        .route("/v1/models", get(models::list))
        .route("/v1/models/{id}", get(models::detail))
        .route("/api/hello", axum::routing::head(|| async { StatusCode::NO_CONTENT }))
        .layer(DefaultBodyLimit::max(8 * 1024 * 1024))
        .layer(middleware::from_fn(log_rejection))
        .with_state(app)
}

async fn log_rejection(request: axum::extract::Request, next: Next) -> Response {
    let start = Instant::now();
    let mut response = next.run(request).await;
    // Accepted requests are logged when their lifecycle ends, including SSE
    // completion/cancellation. Never inspect or log arbitrary paths or headers.
    if !response.status().is_success() && !response.headers().contains_key("x-request-id") {
        let id = uuid::Uuid::new_v4().to_string();
        logging::emit(
            "warn",
            "request_rejected",
            json!({"request_id":id,"http_status":response.status().as_u16(),"latency_ms":start.elapsed().as_millis() as u64}),
        );
        response
            .headers_mut()
            .insert("x-request-id", id.parse().unwrap());
    }
    response
}

pub fn admin(app: App) -> Router {
    Router::new()
        .route("/api/private/v1/health", get(health))
        .route("/api/private/v1/auth/status", get(auth_status))
        .route("/api/private/v1/telemetry/requests", get(list))
        .route("/api/private/v1/telemetry/requests/{id}", get(detail))
        .route(
            "/api/private/v1/telemetry/requests/{id}/events",
            get(events),
        )
        .route("/api/private/v1/telemetry/summary", get(summary))
        .route("/api/private/v1/telemetry/export", get(export))
        .merge(projects::routes())
        .layer(DefaultBodyLimit::max(64 * 1024))
        .route_layer(middleware::from_fn_with_state(
            app.clone(),
            projects::protect,
        ))
        .with_state(app)
}

async fn predict(
    State(app): State<App>,
    Path((project, _region, action)): Path<(String, String, String)>,
    headers: HeaderMap,
    bytes: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let (route_model, method) = action
        .rsplit_once(':')
        .ok_or_else(|| ApiError::bad("Missing Vertex operation"))?;
    let body: Value =
        serde_json::from_slice(&bytes).map_err(|_| ApiError::bad("Invalid JSON body"))?;
    if !body.is_object()
        || body.get("anthropic_version").and_then(Value::as_str) != Some("vertex-2023-10-16")
    {
        return Err(ApiError::bad(
            "Expected anthropic_version vertex-2023-10-16",
        ));
    }
    let counting = route_model == "count-tokens" && method == "rawPredict";
    if method != "rawPredict" && method != "streamRawPredict" {
        return Err(ApiError::bad("Unsupported Vertex operation"));
    }
    let model = if counting {
        body.get("model")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::bad("Counting requires model in body"))?
    } else {
        route_model
    };
    // Reload the verifier registry for every request, making rotation/revocation
    // effective without a container/image restart. Read off the async executor.
    let (app, key, identity) = projects::authenticate(app, &headers).await?;
    projects::check_route(&app, &key, &project).await?;
    let (model_config, window_from_catalog) = models::resolve_vertex(&app, &key, model).await?;
    if !counting
        && (body
            .get("max_tokens")
            .and_then(Value::as_u64)
            .is_none_or(|v| v == 0)
            || body.get("model").is_some_and(|v| v != model))
    {
        return Err(ApiError::bad("Invalid max_tokens or conflicting model"));
    }
    let spec = InferenceSpec {
        model: model.into(),
        config: model_config,
        window_from_catalog,
        identity,
        counting,
        streaming: method == "streamRawPredict",
        client_api: "vertex",
    };
    execute(app, headers, bytes.len(), body, spec).await
}

async fn messages(
    State(app): State<App>,
    headers: HeaderMap,
    bytes: axum::body::Bytes,
) -> Result<Response, ApiError> {
    anthropic(app, headers, bytes, false).await
}
async fn count_messages(
    State(app): State<App>,
    headers: HeaderMap,
    bytes: axum::body::Bytes,
) -> Result<Response, ApiError> {
    anthropic(app, headers, bytes, true).await
}

async fn anthropic(
    app: App,
    headers: HeaderMap,
    bytes: axum::body::Bytes,
    counting: bool,
) -> Result<Response, ApiError> {
    if headers
        .get("anthropic-version")
        .and_then(|h| h.to_str().ok())
        != Some("2023-06-01")
    {
        return Err(ApiError::bad("Expected anthropic-version 2023-06-01"));
    }
    let body: Value =
        serde_json::from_slice(&bytes).map_err(|_| ApiError::bad("Invalid JSON body"))?;
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad("Expected model"))?;
    if body.get("anthropic_version").is_some() {
        return Err(ApiError::bad(
            "Vertex version belongs on the Vertex interface",
        ));
    }
    let streaming = match body.get("stream") {
        None => false,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| ApiError::bad("Expected boolean stream"))?,
    };
    if counting && streaming {
        return Err(ApiError::bad("Counting cannot stream"));
    }
    if !counting
        && body
            .get("max_tokens")
            .and_then(Value::as_u64)
            .is_none_or(|n| n == 0)
    {
        return Err(ApiError::bad("Invalid max_tokens"));
    }
    let (app, key, identity) = projects::authenticate(app, &headers).await?;
    if !models::permitted(&app, &key, model) {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "permission_error",
            "Model access denied",
        ));
    }
    let entry = models::catalog(&app)
        .await?
        .into_iter()
        .find(|m| m.id == model)
        .ok_or_else(|| ApiError::bad("Unknown upstream model"))?;
    let spec = InferenceSpec {
        model: model.into(),
        identity,
        counting,
        streaming,
        client_api: "anthropic",
        window_from_catalog: true,
        config: Model {
            window_tokens: entry.max_input_tokens,
            upstream_model: Some(entry.id),
        },
    };
    execute(app, headers, bytes.len(), body, spec).await
}

struct InferenceSpec {
    model: String,
    config: Model,
    identity: auth::Identity,
    counting: bool,
    streaming: bool,
    client_api: &'static str,
    window_from_catalog: bool,
}

async fn execute(
    app: App,
    headers: HeaderMap,
    bytes: usize,
    body: Value,
    spec: InferenceSpec,
) -> Result<Response, ApiError> {
    let InferenceSpec {
        model,
        config: model_config,
        identity,
        counting,
        streaming,
        client_api,
        window_from_catalog,
    } = spec;
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::bad("Expected messages array"))?;
    if messages.is_empty() {
        return Err(ApiError::bad("Expected at least one message"));
    }
    let run_id = headers
        .get("x-shim-run-id")
        .map(|h| {
            h.to_str()
                .map(str::to_owned)
                .map_err(|_| ApiError::bad("Invalid run label"))
        })
        .transpose()?;
    if run_id.as_ref().is_some_and(|s| {
        s.len() > 128
            || !s
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    }) {
        return Err(ApiError::bad("Invalid run label"));
    }
    let operation = if counting {
        "count_tokens"
    } else if streaming {
        "stream_generation"
    } else {
        "generation"
    };
    let mut record = Record::new(
        &model,
        operation,
        identity,
        &body,
        bytes,
        run_id,
        model_config.window_tokens,
    );
    record.client_api = client_api.into();
    record.backend = app.backend.name().into();
    record.synthetic = app.backend.synthetic();
    record.upstream_auth = match &app.backend {
        backend::Backend::Mock => "none",
        backend::Backend::Anthropic(upstream) => upstream.credentials.kind(),
    }
    .into();
    record.resolved_model = model_config
        .upstream_model
        .as_deref()
        .unwrap_or(&model)
        .into();
    if record.context["window_tokens"].is_number() {
        record.context["window_source"] = json!(if window_from_catalog
            && matches!(&app.backend, backend::Backend::Anthropic(_))
        {
            "upstream models API"
        } else {
            "deployment override"
        });
    }
    let beta = headers
        .get("anthropic-beta")
        .map(|h| h.to_str().map_err(|_| ApiError::bad("Invalid beta header")))
        .transpose()?;
    if beta.is_some_and(|value| value.len() > 4096) {
        return Err(ApiError::bad("Beta header too long"));
    }
    let mut lifecycle = Lifecycle::new(app.store.clone(), record);
    if let backend::Backend::Anthropic(upstream) = &app.backend {
        let id = lifecycle.record.request_id.clone();
        let mut response =
            backend::forward(upstream, lifecycle, body.clone(), counting, streaming, beta)
                .await
                .unwrap_or_else(IntoResponse::into_response);
        response
            .headers_mut()
            .insert("x-request-id", id.parse().unwrap());
        return Ok(response);
    }
    // Fixture counts are deliberately constant and synthetic, not a tokenizer.
    if counting {
        lifecycle.record.merge_usage(&json!({"input_tokens":12}));
        lifecycle.finish();
        return Ok((
            [("x-request-id", lifecycle.record.request_id.clone())],
            Json(json!({"input_tokens":12})),
        )
            .into_response());
    }
    let output_tokens = body["max_tokens"].as_u64().unwrap().min(3);
    let usage = json!({"input_tokens":12,"output_tokens":output_tokens,"cache_creation_input_tokens":0,"cache_read_input_tokens":0});
    let message = json!({"id":format!("msg_{}",lifecycle.record.request_id),"type":"message","role":"assistant","model":model,
        "content":[{"type":"text","text":"Greetings from vertex-shim."}],"stop_reason":"end_turn","stop_sequence":null,"usage":usage});
    if !streaming {
        lifecycle.record.usage_source = "message.usage (synthetic)".into();
        lifecycle.record.merge_usage(&usage);
        app.store.event(&lifecycle.record, 0, "message.usage");
        lifecycle.finish();
        return Ok((
            [("x-request-id", lifecycle.record.request_id.clone())],
            Json(message),
        )
            .into_response());
    }
    let mut start = message.clone();
    start["content"] = json!([]);
    start["stop_reason"] = Value::Null;
    start["usage"]["output_tokens"] = json!(0);
    let frames = vec![
        json!({"type":"message_start","message":start}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Greetings from vertex-shim."}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":output_tokens}}),
        json!({"type":"message_stop"}),
    ];
    let id = lifecycle.record.request_id.clone();
    lifecycle.record.usage_source =
        "message_start.message.usage + message_delta.usage (synthetic)".into();
    let stream = futures_util::stream::unfold(
        (lifecycle, frames.into_iter(), 0u32),
        |(mut life, mut frames, sequence)| async move {
            let frame = frames.next()?;
            tokio::time::sleep(std::time::Duration::from_millis(15)).await;
            if let Some(usage) = frame
                .pointer("/message/usage")
                .or_else(|| frame.get("usage"))
            {
                life.record.merge_usage(usage);
                life.store
                    .event(&life.record, sequence, "SSE usage snapshot");
                life.store.request(&life.record);
            }
            if frame["type"] == "message_stop" {
                life.finish();
            }
            let event = Event::default()
                .event(frame["type"].as_str().unwrap())
                .data(frame.to_string());
            Some((Ok::<_, Infallible>(event), (life, frames, sequence + 1)))
        },
    );
    Ok(([("x-request-id", id)], Sse::new(stream)).into_response())
}

pub(crate) struct Lifecycle {
    store: Store,
    record: Record,
    start: Instant,
    finished: bool,
}
impl Lifecycle {
    fn new(store: Store, record: Record) -> Self {
        store.request(&record);
        Self {
            store,
            record,
            start: Instant::now(),
            finished: false,
        }
    }
    fn finish(&mut self) {
        self.record.completion = "complete".into();
        if self
            .record
            .usage
            .get("input_tokens")
            .and_then(Value::as_u64)
            .is_some()
            && (self.record.operation == "count_tokens"
                || self
                    .record
                    .usage
                    .get("output_tokens")
                    .and_then(Value::as_u64)
                    .is_some())
            && self.record.observation_error.is_none()
        {
            self.record.usage_status = "complete".into();
        }
        self.record.finished_at = Some(chrono::Utc::now().to_rfc3339());
        self.record.latency_ms = Some(self.start.elapsed().as_millis() as u64);
        self.store.request(&self.record);
        if !self.finished {
            logging::completed(&self.record);
        }
        self.finished = true;
    }
    fn fail(&mut self, status: u16, error: &str) {
        self.record.http_status = status;
        self.record.completion = "failed".into();
        self.record.error_type = Some(error.into());
        if self.record.usage_status == "complete" {
            self.record.usage_status = "partial".into();
        }
        self.record.finished_at = Some(chrono::Utc::now().to_rfc3339());
        self.record.latency_ms = Some(self.start.elapsed().as_millis() as u64);
        self.store.request(&self.record);
        if !self.finished {
            logging::completed(&self.record);
        }
        self.finished = true;
    }
}
impl Drop for Lifecycle {
    fn drop(&mut self) {
        if !self.finished {
            self.record.completion = "cancelled".into();
            self.record.finished_at = Some(chrono::Utc::now().to_rfc3339());
            self.record.latency_ms = Some(self.start.elapsed().as_millis() as u64);
            self.store.request(&self.record);
            logging::completed(&self.record);
        }
    }
}

async fn auth_status(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    auth::admin(&headers, &app.admin_verifier)?;
    let mut status = match &app.backend {
        backend::Backend::Mock => json!({"mode":"none","state":"mock"}),
        backend::Backend::Anthropic(upstream) => upstream.credentials.status().await,
    };
    if let Some(adc) = &app.mock_adc {
        status["mock_adc"] = adc.status();
    }
    Ok(Json(status))
}

async fn health(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    auth::admin(&headers, &app.admin_verifier)?;
    Ok(Json(app.store.health()))
}
async fn page(app: &App, headers: &HeaderMap, filter: Filter) -> Result<Value, ApiError> {
    auth::admin(headers, &app.admin_verifier)?;
    filter.validate()?;
    app.store
        .read(move |connection| filter.list(connection))
        .await
}
async fn list(
    State(app): State<App>,
    headers: HeaderMap,
    Query(filter): Query<Filter>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(page(&app, &headers, filter).await?))
}
async fn summary(
    State(app): State<App>,
    headers: HeaderMap,
    Query(filter): Query<Filter>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(telemetry::summary(
        page(&app, &headers, filter).await?,
    )))
}
async fn export(
    State(app): State<App>,
    headers: HeaderMap,
    Query(filter): Query<Filter>,
) -> Result<Response, ApiError> {
    let jsonl = filter.format.as_deref() == Some("jsonl");
    let page = page(&app, &headers, filter).await?;
    if !jsonl {
        return Ok(Json(page).into_response());
    }
    let body: String = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| format!("{v}\n"))
        .collect();
    let mut headers = HeaderMap::new();
    headers.insert("content-type", "application/x-ndjson".parse().unwrap());
    if let Some(cursor) = page["next_cursor"].as_u64() {
        headers.insert("x-next-cursor", cursor.to_string().parse().unwrap());
    }
    Ok((headers, body).into_response())
}
async fn detail(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    auth::admin(&headers, &app.admin_verifier)?;
    let value = app
        .store
        .read(move |conn| {
            use rusqlite::OptionalExtension;
            let payload: Option<String> = conn
                .query_row(
                    "SELECT payload FROM requests WHERE request_id=?1",
                    [id],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(payload
                .map(|s| serde_json::from_str(&s).unwrap())
                .unwrap_or(Value::Null))
        })
        .await?;
    if value.is_null() {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "not_found_error",
            "Unknown request",
        ));
    }
    Ok(Json(value))
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct EventFilter {
    cursor: Option<u32>,
    limit: Option<u16>,
}
async fn events(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(filter): Query<EventFilter>,
) -> Result<Json<Value>, ApiError> {
    auth::admin(&headers, &app.admin_verifier)?;
    if filter.limit.is_some_and(|n| n == 0 || n > 500) {
        return Err(ApiError::bad("Invalid limit"));
    }
    let value=app.store.read(move |conn| {
        let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM requests WHERE request_id=?1)",[&id],|row|row.get(0))?;
        if !exists {return Ok(Value::Null);}
        let limit=filter.limit.unwrap_or(100) as usize;
        let mut statement=conn.prepare("SELECT sequence,payload FROM events WHERE request_id=?1 AND (?2 IS NULL OR sequence>?2) ORDER BY sequence LIMIT ?3")?;
        let rows=statement.query_map(rusqlite::params![id,filter.cursor,(limit+1) as u64],|row|Ok((row.get::<_,u32>(0)?,row.get::<_,String>(1)?)))?;
        let mut values=rows.collect::<rusqlite::Result<Vec<_>>>()?; let more=values.len()>limit; values.truncate(limit);
        let cursor=if more {values.last().map(|r|r.0)} else {None};
        Ok(json!({"items":values.into_iter().map(|(_,s)|serde_json::from_str::<Value>(&s).unwrap()).collect::<Vec<_>>(),"next_cursor":cursor}))
    }).await?;
    if value.is_null() {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "not_found_error",
            "Unknown request",
        ));
    }
    Ok(Json(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    #[tokio::test]
    async fn vertex_routes_auth_and_listener_separation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys.json");
        std::fs::write(&path,json!({"keys":[{"client_id":"c","key_id":"k","sha256":auth::fingerprint("local"),"models":["model"],"backends":["mock"]}]}).to_string()).unwrap();
        let store = Store::open(&dir.path().join("db.sqlite")).unwrap();
        let app = App {
            upstreams: Arc::new(BTreeMap::new()),
            mock_adc: None,
            backend: backend::Backend::Mock,
            store: store.clone(),
            registry: path,
            admin_verifier: auth::fingerprint("admin"),
            models: Arc::new(BTreeMap::from([(
                "model".into(),
                Model {
                    window_tokens: None,
                    upstream_model: None,
                },
            )])),
        };
        let router = inference(app.clone());
        let route = "/v1/projects/local/locations/r/publishers/anthropic/models/model:rawPredict";
        let body=json!({"anthropic_version":"vertex-2023-10-16","max_tokens":100,"messages":[{"role":"user","content":"secret prompt"}]}).to_string();
        let response = router
            .clone()
            .oneshot(
                Request::post(route)
                    .header("x-api-key", "local")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let value: Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(value["usage"]["output_tokens"], 3);
        assert_eq!(
            router
                .oneshot(
                    Request::get("/api/private/v1/telemetry/requests")
                        .body(Body::empty())
                        .unwrap()
                )
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        store.flush().await;
        let admin = admin(app);
        assert_eq!(
            admin
                .clone()
                .oneshot(
                    Request::get("/api/private/v1/health")
                        .header("authorization", "Bearer local")
                        .body(Body::empty())
                        .unwrap()
                )
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let response = admin
            .oneshot(
                Request::get("/api/private/v1/telemetry/requests")
                    .header("authorization", "Bearer admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let text = String::from_utf8(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        assert!(!text.contains("secret prompt"));
    }
}
