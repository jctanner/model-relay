use crate::{
    ApiError, Lifecycle,
    credentials::{Credentials, OAuth},
    models::CatalogModel,
    streaming::Observer,
};
use axum::{
    body::Body,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use reqwest::{Client, Url};
use serde_json::Value;
use serde_json::json;
use std::{path::PathBuf, time::Duration};

#[derive(Clone)]
pub enum Backend {
    Mock,
    Anthropic(Upstream),
}
impl Backend {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Mock => "mock",
            Self::Anthropic(_) => "anthropic",
        }
    }
    pub fn synthetic(&self) -> bool {
        match self {
            Self::Mock => true,
            Self::Anthropic(u) => u.synthetic,
        }
    }
}

fn safe_id(value: Option<&str>, prefix: &str) -> Option<String> {
    value
        .filter(|id| {
            id.starts_with(prefix)
                && id.len() <= 200
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
        .map(str::to_owned)
}

fn error_type(value: &Value) -> &'static str {
    match value.pointer("/error/type").and_then(Value::as_str) {
        Some("authentication_error") => "authentication_error",
        Some("permission_error") => "permission_error",
        Some("invalid_request_error") => "invalid_request_error",
        Some("rate_limit_error") => "rate_limit_error",
        Some("overloaded_error") => "overloaded_error",
        Some("not_found_error") => "not_found_error",
        _ => "upstream_http_error",
    }
}

fn observe(life: &mut Lifecycle, value: &Value, sequence: u32, source: &'static str) {
    let message = value.get("message").unwrap_or(value);
    if let Some(id) = safe_id(message.get("id").and_then(Value::as_str), "msg_") {
        life.record.upstream_message_id = Some(id);
    }
    if let Some(usage) = message.get("usage").or_else(|| value.get("usage")) {
        life.record.merge_usage(usage);
        life.store.event(&life.record, sequence, source);
    }
    if let Some(reason) = value
        .pointer("/delta/stop_reason")
        .or_else(|| value.get("stop_reason"))
        .and_then(Value::as_str)
        && [
            "end_turn",
            "max_tokens",
            "stop_sequence",
            "tool_use",
            "pause_turn",
            "refusal",
            "model_context_window_exceeded",
        ]
        .contains(&reason)
    {
        life.record.stop_reason = Some(reason.into());
    }
    life.store.request(&life.record);
}

pub(crate) async fn forward(
    upstream: &Upstream,
    mut life: Lifecycle,
    body: Value,
    counting: bool,
    streaming: bool,
    beta: Option<&str>,
) -> Result<Response, ApiError> {
    life.record.usage_source = "upstream usage not yet observed".into();
    let mut response = match upstream
        .send(body, &life.record.resolved_model, counting, streaming, beta)
        .await
    {
        Ok(response) => response,
        Err(error) => {
            life.fail(error.0.as_u16(), error.1);
            return Err(error);
        }
    };
    let status = response.status();
    life.record.http_status = status.as_u16();
    life.record.upstream_request_id = safe_id(
        response
            .headers()
            .get("request-id")
            .and_then(|h| h.to_str().ok()),
        "req_",
    );
    let mut headers = HeaderMap::new();
    // reqwest transparently decodes gzip. Never copy encoding/length headers
    // for the original compressed entity or hop-by-hop response headers.
    for name in ["content-type", "retry-after"] {
        if let Some(value) = response.headers().get(name) {
            headers.insert(name, value.clone());
        }
    }
    headers.insert(
        "x-request-id",
        life.record
            .request_id
            .parse()
            .map_err(|_| ApiError::internal())?,
    );
    if streaming && status.is_success() {
        if !response
            .headers()
            .get("content-type")
            .and_then(|h| h.to_str().ok())
            .is_some_and(|s| s.starts_with("text/event-stream"))
        {
            life.fail(502, "invalid_upstream_content_type");
            return Err(ApiError(
                StatusCode::BAD_GATEWAY,
                "api_error",
                "Expected upstream SSE response",
            ));
        }
        life.record.usage_source = "message_start.message.usage + message_delta.usage".into();
        let stream = Box::pin(response.bytes_stream());
        let observed = futures_util::stream::unfold(
            (stream, life, Observer::default(), 0u32, false),
            |(mut stream, mut life, mut observer, mut sequence, failed)| async move {
                if failed {
                    return None;
                }
                match stream.next().await {
                    Some(Ok(bytes)) => {
                        let mut terminal = false;
                        observer.feed(&bytes, |value| {
                            if life.record.first_event_ms.is_none() {
                                life.record.first_event_ms =
                                    Some(life.start.elapsed().as_millis() as u64);
                            }
                            observe(&mut life, &value, sequence, "SSE usage snapshot");
                            sequence = sequence.saturating_add(1);
                            if value["type"] == "message_stop" {
                                terminal = true;
                            }
                            if value["type"] == "error" {
                                life.fail(life.record.http_status, error_type(&value));
                            }
                        });
                        if let Some(error) = observer.error {
                            life.record.observation_error = Some(error.into());
                            if life.record.usage_status == "complete" {
                                life.record.usage_status = "partial".into();
                            }
                            life.store.request(&life.record);
                        }
                        if terminal && !life.finished {
                            life.finish();
                        }
                        Some((
                            Ok::<_, std::io::Error>(bytes),
                            (stream, life, observer, sequence, false),
                        ))
                    }
                    Some(Err(error)) => {
                        let error_type = if error.is_timeout() {
                            "timeout_error"
                        } else {
                            "upstream_stream_error"
                        };
                        life.fail(life.record.http_status, error_type);
                        Some((
                            Err(std::io::Error::other("Upstream stream failed")),
                            (stream, life, observer, sequence, true),
                        ))
                    }
                    None => {
                        if !life.finished {
                            life.record.observation_error = Some(
                                if observer.has_pending() {
                                    "incomplete_sse_event"
                                } else {
                                    "missing_message_stop"
                                }
                                .into(),
                            );
                            life.fail(life.record.http_status, "incomplete_upstream_stream");
                        }
                        None
                    }
                }
            },
        );
        return Ok((status, headers, Body::from_stream(observed)).into_response());
    }
    let mut bytes = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if bytes.len() + chunk.len() <= 8 * 1024 * 1024 => {
                bytes.extend_from_slice(&chunk)
            }
            Ok(Some(_)) => {
                life.fail(502, "upstream_response_too_large");
                return Err(ApiError(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    "Upstream response too large",
                ));
            }
            Ok(None) => break,
            Err(_) => {
                life.fail(502, "upstream_read_error");
                return Err(ApiError(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    "Upstream response read failed",
                ));
            }
        }
    }
    let value = serde_json::from_slice::<Value>(&bytes);
    if status.is_success() {
        let value = match value {
            Ok(value) if value.is_object() => value,
            _ => {
                life.fail(502, "invalid_upstream_json");
                return Err(ApiError(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    "Invalid upstream JSON",
                ));
            }
        };
        if counting {
            life.record.usage_source = "count_tokens.input_tokens".into();
            life.record
                .merge_usage(&json!({"input_tokens":value.get("input_tokens")}));
            life.store
                .event(&life.record, 0, "count_tokens.input_tokens");
        } else {
            life.record.usage_source = "message.usage".into();
            observe(&mut life, &value, 0, "message.usage");
        }
        life.finish();
    } else {
        life.fail(
            status.as_u16(),
            value.as_ref().map_or("upstream_http_error", error_type),
        );
    }
    Ok((status, headers, bytes).into_response())
}

/// Whole-request upstream timeout, streaming included: `SHIM_UPSTREAM_TIMEOUT_SECONDS`
/// (default 120, clamped to 1–600). Both the environment-configured upstream and managed
/// upstream profiles use it.
pub fn upstream_timeout() -> Result<Duration, std::num::ParseIntError> {
    Ok(Duration::from_secs(
        std::env::var("SHIM_UPSTREAM_TIMEOUT_SECONDS")
            .unwrap_or_else(|_| "120".into())
            .parse::<u64>()?
            .clamp(1, 600),
    ))
}

#[derive(Clone)]
pub struct Upstream {
    pub provider: &'static str,
    pub client: Client,
    pub origin: Url,
    pub credentials: Credentials,
    pub synthetic: bool,
    pub workspace: Option<String>,
    catalog_cache: std::sync::Arc<tokio::sync::Mutex<Option<CachedCatalog>>>,
}

struct CachedCatalog {
    token_fingerprint: String,
    fetched: std::time::Instant,
    models: Vec<CatalogModel>,
}

impl Upstream {
    pub fn new(
        origin: &str,
        token_file: PathBuf,
        synthetic: bool,
        workspace: Option<String>,
        timeout: Duration,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let origin = Url::parse(origin)?;
        if origin.host_str().is_none()
            || !origin.username().is_empty()
            || origin.password().is_some()
            || origin.query().is_some()
            || origin.fragment().is_some()
            || origin.path() != "/"
        {
            return Err("Upstream must be an origin without userinfo, query, or path".into());
        }
        if origin.scheme() != "https" && !(synthetic && origin.scheme() == "http") {
            return Err("Real upstream credentials require HTTPS; HTTP is available only for synthetic fixtures".into());
        }
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(5))
            .timeout(timeout)
            .gzip(true)
            .build()?;
        Ok(Self {
            provider: "anthropic",
            client,
            origin,
            credentials: Credentials::BearerFile(token_file),
            synthetic,
            workspace,
            catalog_cache: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        })
    }
    pub fn with_oauth(mut self, oauth: std::sync::Arc<OAuth>) -> Self {
        self.credentials = Credentials::ClaudeOAuth(oauth);
        self
    }
    pub fn with_enmaas(mut self) -> Self {
        self.provider = "enmaas";
        self
    }
    pub async fn clear_catalog(&self) {
        *self.catalog_cache.lock().await = None;
    }
    pub async fn catalog(&self) -> Result<Vec<CatalogModel>, ApiError> {
        // Validate credentials on every read, including cached metadata. Rotation
        // invalidates the cache; failures never serve stale catalog entries.
        let token = self.credentials.token().await?;
        let fingerprint = crate::auth::fingerprint(&token);
        tokio::time::timeout(Duration::from_millis(2500), async {
            let mut cache = self.catalog_cache.lock().await;
            if let Some(cached) = cache.as_ref()
                && cached.token_fingerprint == fingerprint
                && cached.fetched.elapsed() < Duration::from_secs(60)
            {
                return Ok(cached.models.clone());
            }
            let unavailable = || {
                ApiError(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "upstream_catalogue_unavailable",
                    "Upstream model catalog unavailable",
                )
            };
            let url = self
                .origin
                .join("/v1/models")
                .map_err(|_| ApiError::internal())?;
            let mut response = self
                .authenticated(
                    self.client.get(url).query(&[("limit", "1000")]),
                    &token,
                    None,
                )
                .send()
                .await
                .map_err(|_| unavailable())?;
            if !response.status().is_success() {
                return Err(unavailable());
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| unavailable())? {
                if bytes.len() + chunk.len() > 1024 * 1024 {
                    return Err(unavailable());
                }
                bytes.extend_from_slice(&chunk);
            }
            let value: Value = serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
            if value.get("has_more").and_then(Value::as_bool) != Some(false)
                && !(self.provider == "enmaas" && value.get("has_more").is_none())
            {
                return Err(unavailable());
            }
            let mut data = value["data"].clone();
            if self.provider == "enmaas" {
                for entry in data.as_array_mut().ok_or_else(unavailable)? {
                    let id = entry["id"].as_str().ok_or_else(unavailable)?.to_owned();
                    let fields = entry.as_object_mut().ok_or_else(unavailable)?;
                    fields.entry("display_name").or_insert(json!(id));
                    fields.entry("created_at").or_insert(json!(""));
                }
            }
            let mut models: Vec<CatalogModel> =
                serde_json::from_value(data).map_err(|_| unavailable())?;
            let mut ids = std::collections::BTreeSet::new();
            if models.len() > 1000
                || models
                    .iter()
                    .any(|m| !m.validate() || !ids.insert(m.id.clone()))
            {
                return Err(unavailable());
            }
            models.retain(|m| m.lifecycle.as_deref() != Some("retired"));
            *cache = Some(CachedCatalog {
                token_fingerprint: fingerprint,
                fetched: std::time::Instant::now(),
                models: models.clone(),
            });
            Ok(models)
        })
        .await
        .map_err(|_| {
            ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "upstream_catalogue_timeout",
                "Upstream model catalog timed out",
            )
        })?
    }

    fn authenticated(
        &self,
        request: reqwest::RequestBuilder,
        token: &str,
        beta: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let request = if self.provider == "enmaas" {
            request.header("x-api-key", token)
        } else {
            request.bearer_auth(token)
        };
        let mut request = request.header("anthropic-version", "2023-06-01").header(
            "user-agent",
            concat!("vertex-shim/", env!("CARGO_PKG_VERSION")),
        );
        if self.provider == "enmaas" {
            return request;
        }
        let oauth = matches!(self.credentials, Credentials::ClaudeOAuth(_));
        let beta = match (oauth, beta) {
            (true, Some(beta)) => Some(format!("oauth-2025-04-20,{beta}")),
            (true, None) => Some("oauth-2025-04-20".to_owned()),
            (false, beta) => beta.map(str::to_owned),
        };
        if let Some(beta) = beta {
            request = request.header("anthropic-beta", beta);
        }
        if let Some(workspace) = &self.workspace {
            request = request.header("anthropic-workspace-id", workspace);
        }
        request
    }
    pub async fn send(
        &self,
        mut body: Value,
        model: &str,
        counting: bool,
        streaming: bool,
        beta: Option<&str>,
    ) -> Result<reqwest::Response, ApiError> {
        let token = self.credentials.token().await?;
        let payload = body
            .as_object_mut()
            .ok_or_else(|| ApiError::bad("Expected object"))?;
        payload.remove("anthropic_version");
        payload.insert("model".into(), model.into());
        if self.provider == "enmaas" && !counting {
            // The live Messages endpoint accepts Claude's output_config contract.
            let config = payload.entry("output_config").or_insert(json!({}));
            config
                .as_object_mut()
                .ok_or_else(|| ApiError::bad("output_config must be an object"))?
                .insert("effort".into(), json!("medium"));
        }
        if counting {
            payload.remove("stream");
        } else {
            payload.insert("stream".into(), streaming.into());
        }
        let path = if counting {
            "/v1/messages/count_tokens"
        } else {
            "/v1/messages"
        };
        let url = self.origin.join(path).map_err(|_| ApiError::internal())?;
        // Construct outbound headers from scratch. Client/admin credentials,
        // arbitrary custom headers, and Google auth never enter this request.
        let request = self.authenticated(self.client.post(url).json(&body), &token, beta);
        request.send().await.map_err(|error| {
            if error.is_timeout() {
                ApiError(
                    axum::http::StatusCode::GATEWAY_TIMEOUT,
                    "timeout_error",
                    "Upstream timed out",
                )
            } else {
                ApiError(
                    axum::http::StatusCode::BAD_GATEWAY,
                    "upstream_connection_error",
                    "Upstream connection failed",
                )
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn real_credentials_require_an_explicit_https_origin() {
        for url in [
            "http://example.com",
            "https://user:secret@example.com",
            "https://example.com/path",
            "https://example.com?token=secret",
            "https://example.com#fragment",
        ] {
            assert!(
                Upstream::new(url, "/unused".into(), false, None, Duration::from_secs(1)).is_err()
            );
        }
        assert!(
            Upstream::new(
                "https://example.com",
                "/unused".into(),
                false,
                None,
                Duration::from_secs(1)
            )
            .is_ok()
        );
        assert!(
            Upstream::new(
                "http://fixture:8090",
                "/unused".into(),
                true,
                None,
                Duration::from_secs(1)
            )
            .is_ok()
        );
    }
}
