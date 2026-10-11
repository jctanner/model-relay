//! Opt-in, synthetic Google metadata credentials. Tokens never leave the shim
//! as upstream credentials, and neither tokens nor their hashes enter logs.
use crate::{ApiError, App, auth, logging, models};
use axum::{
    Form, Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

const PREFIX: &str = "mock-gcp-";
const MAX_TOKENS: usize = 4096;
const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

#[derive(Clone)]
pub(crate) struct Binding {
    pub project_id: Option<String>,
    pub project_epoch: String,
    pub client_id: String,
    pub key_id: String,
    pub verifier: String,
}
struct Issued {
    binding: Binding,
    expires: Instant,
}

pub struct MockAdc {
    key_id: String,
    project: String,
    ttl: Duration,
    tokens: Mutex<HashMap<String, Issued>>,
    issued: AtomicU64,
}

impl MockAdc {
    pub fn new(key_id: String, project: String, ttl_seconds: u64) -> Result<Self, &'static str> {
        if key_id.is_empty()
            || key_id.len() > 128
            || !key_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            || project.is_empty()
            || project.len() > 63
            || !project
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            || !(1..=3600).contains(&ttl_seconds)
        {
            return Err("Invalid mock ADC configuration");
        }
        Ok(Self {
            key_id,
            project,
            ttl: Duration::from_secs(ttl_seconds),
            tokens: Mutex::new(HashMap::new()),
            issued: AtomicU64::new(0),
        })
    }

    fn issue(&self, registry: &auth::Registry, backend: &str) -> Result<Value, ApiError> {
        let candidates: Vec<_> = registry
            .keys
            .iter()
            .filter(|key| key.key_id == self.key_id)
            .collect();
        let key = candidates
            .first()
            .filter(|_| candidates.len() == 1)
            .filter(|key| key.mock_adc && key.backends.iter().any(|b| b == backend))
            .ok_or(ApiError(
                StatusCode::FORBIDDEN,
                "permission_error",
                "Mock ADC identity is not authorized",
            ))?;
        self.issue_key(key, backend)
    }

    fn issue_key(&self, key: &auth::Key, backend: &str) -> Result<Value, ApiError> {
        if !key.mock_adc || !key.backends.iter().any(|b| b == backend) {
            return Err(ApiError(
                StatusCode::FORBIDDEN,
                "permission_error",
                "Mock ADC identity is not authorized",
            ));
        }
        let now = Instant::now();
        let mut tokens = self.tokens.lock().map_err(|_| ApiError::internal())?;
        tokens.retain(|_, token| token.expires > now);
        if tokens.len() >= MAX_TOKENS {
            return Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "overloaded_error",
                "Mock ADC token capacity reached",
            ));
        }
        let access = format!(
            "{PREFIX}{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        tokens.insert(
            auth::fingerprint(&access),
            Issued {
                binding: Binding {
                    project_id: key.project_id.clone(),
                    project_epoch: key.project_epoch.clone(),
                    client_id: key.client_id.clone(),
                    key_id: key.key_id.clone(),
                    verifier: key.sha256.clone(),
                },
                expires: now + self.ttl,
            },
        );
        self.issued.fetch_add(1, Ordering::Relaxed);
        logging::emit(
            "info",
            "mock_adc_token_issued",
            json!({"expires_in":self.ttl.as_secs(),"synthetic":true}),
        );
        Ok(json!({"access_token":access,"token_type":"Bearer","expires_in":self.ttl.as_secs()}))
    }

    pub(crate) fn resolve(&self, access: &str) -> Result<Option<Binding>, ApiError> {
        if !access.starts_with(PREFIX) {
            return Ok(None);
        }
        if access.len() != PREFIX.len() + 64 {
            return Err(ApiError::unauthorized());
        }
        let mut tokens = self.tokens.lock().map_err(|_| ApiError::internal())?;
        let fingerprint = auth::fingerprint(access);
        let issued = tokens
            .get(&fingerprint)
            .filter(|token| token.expires > Instant::now());
        match issued {
            Some(token) => Ok(Some(token.binding.clone())),
            None => {
                tokens.remove(&fingerprint);
                Err(ApiError::unauthorized())
            }
        }
    }

    pub(crate) fn project_id(&self) -> &str {
        &self.project
    }

    pub fn status(&self) -> Value {
        let active = self
            .tokens
            .lock()
            .map(|mut tokens| {
                tokens.retain(|_, token| token.expires > Instant::now());
                tokens.len()
            })
            .ok();
        json!({"enabled":true,"synthetic":true,"expires_in":self.ttl.as_secs(),"issued_tokens":self.issued.load(Ordering::Relaxed),"active_tokens":active})
    }
}

#[derive(Default, Deserialize)]
struct MetadataQuery {
    scopes: Option<String>,
}

pub fn metadata(app: App) -> Router {
    Router::new()
        .route(
            "/health",
            get(|State(app): State<App>| async move {
                Json(
                    app.mock_adc
                        .as_ref()
                        .map(|adc| adc.status())
                        .unwrap_or(json!({"enabled":false})),
                )
            }),
        )
        .route("/computeMetadata/v1/{*path}", get(get_metadata))
        .route("/v1/token", post(exchange))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .with_state(app)
}

async fn get_metadata(
    State(app): State<App>,
    Path(path): Path<String>,
    Query(query): Query<MetadataQuery>,
    headers: HeaderMap,
) -> Response {
    let response = async {
        let adc = app.mock_adc.as_ref().ok_or(ApiError(
            StatusCode::NOT_FOUND,
            "not_found_error",
            "Metadata disabled",
        ))?;
        if headers.get_all("metadata-flavor").iter().count() != 1
            || headers
                .get("metadata-flavor")
                .is_none_or(|value| value != "Google")
            || headers.contains_key("x-forwarded-for")
        {
            return Err(ApiError(
                StatusCode::FORBIDDEN,
                "permission_error",
                "Metadata-Flavor: Google required",
            ));
        }
        let path = path.trim_end_matches('/');
        let text = match path {
            "instance" => "service-accounts/\n".to_owned(),
            "project/project-id" => adc.project.clone(),
            "instance/service-accounts" => "default/\n".to_owned(),
            "instance/service-accounts/default/email" => {
                format!("mock@{}.iam.gserviceaccount.com", adc.project)
            }
            "instance/service-accounts/default/scopes" => format!("{SCOPE}\n"),
            "universe/universe-domain" => "googleapis.com".to_owned(),
            "instance/service-accounts/default/token" => {
                if query
                    .scopes
                    .as_ref()
                    .is_some_and(|scopes| scopes.split(',').any(|s| s != SCOPE))
                {
                    return Err(ApiError(
                        StatusCode::FORBIDDEN,
                        "permission_error",
                        "Unsupported mock ADC scope",
                    ));
                }
                let registry = models::registry(&app).await?;
                let candidates: Vec<_> = registry
                    .keys
                    .iter()
                    .filter(|key| key.key_id == adc.key_id)
                    .collect();
                let key = candidates
                    .first()
                    .filter(|_| candidates.len() == 1)
                    .ok_or(ApiError(
                        StatusCode::FORBIDDEN,
                        "permission_error",
                        "Mock ADC identity is not authorized",
                    ))?;
                if key.project_id.as_deref() != Some(adc.project.as_str())
                    && key.project_id.as_deref() != Some(crate::projects::DEFAULT_PROJECT)
                {
                    return Err(ApiError::unauthorized());
                }
                crate::projects::project_for_key(&app, key).await?;
                let backend = key.backends.first().ok_or_else(ApiError::unauthorized)?;
                return Ok(Json(adc.issue(&registry, backend)?).into_response());
            }
            _ => {
                return Err(ApiError(
                    StatusCode::NOT_FOUND,
                    "not_found_error",
                    "Unknown metadata resource",
                ));
            }
        };
        logging::emit("info", "mock_adc_metadata_read", json!({"synthetic":true}));
        Ok(text.into_response())
    }
    .await
    .unwrap_or_else(|error: ApiError| {
        logging::emit(
            "warn",
            "mock_adc_rejected",
            json!({"http_status":error.0.as_u16(),"error_type":error.1}),
        );
        error.into_response()
    });
    let mut response = response;
    response
        .headers_mut()
        .insert("metadata-flavor", "Google".parse().unwrap());
    response
        .headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    fn registry() -> auth::Registry {
        serde_json::from_value(json!({"keys":[{"client_id":"c","key_id":"k","sha256":auth::fingerprint("local"),"models":["model"],"backends":["mock"],"mock_adc":true}]})).unwrap()
    }
    fn bearer(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
        headers
    }
    #[test]
    fn expiry_concurrent_tokens_rotation_and_revocation() {
        let adc = MockAdc::new("k".into(), "vertex-shim-local".into(), 600).unwrap();
        let mut keys = registry();
        let first = adc.issue(&keys, "mock").unwrap()["access_token"]
            .as_str()
            .unwrap()
            .to_owned();
        let second = adc.issue(&keys, "mock").unwrap()["access_token"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_ne!(first, second);
        for token in [&first, &second] {
            let identity =
                auth::authenticate(&bearer(token), &keys, "model", "mock", Some(&adc)).unwrap();
            assert_eq!(identity.scheme, "mock-adc");
            assert_eq!(identity.client_id, "c");
        }
        assert!(auth::authenticate(&bearer(&first), &keys, "other", "mock", Some(&adc)).is_err());
        assert!(auth::admin(&bearer(&first), &auth::fingerprint("admin")).is_err());
        keys.keys.push(keys.keys[0].clone());
        assert!(auth::authenticate(&bearer(&first), &keys, "model", "mock", Some(&adc)).is_err());
        keys.keys.pop();
        keys.keys[0].mock_adc = false;
        assert!(auth::authenticate(&bearer(&first), &keys, "model", "mock", Some(&adc)).is_err());
        keys.keys[0].mock_adc = true;
        keys.keys[0].sha256 = auth::fingerprint("rotated");
        assert!(auth::authenticate(&bearer(&first), &keys, "model", "mock", Some(&adc)).is_err());
        adc.tokens
            .lock()
            .unwrap()
            .get_mut(&auth::fingerprint(&second))
            .unwrap()
            .expires = Instant::now();
        assert!(adc.resolve(&second).is_err());
        assert!(adc.resolve("mock-gcp-invalid").is_err());
        assert!(adc.resolve("opaque-local-key").unwrap().is_none());
    }
    #[test]
    fn issuance_grants_config_and_bounded_storage() {
        assert!(MockAdc::new("k".into(), "project".into(), 0).is_err());
        let adc = MockAdc::new("k".into(), "vertex-shim-local".into(), 600).unwrap();
        let mut keys = registry();
        assert!(adc.issue(&keys, "anthropic").is_err());
        keys.keys.push(keys.keys[0].clone());
        assert!(adc.issue(&keys, "mock").is_err());
        keys.keys.pop();
        let binding = Binding {
            project_id: None,
            project_epoch: String::new(),
            client_id: "c".into(),
            key_id: "k".into(),
            verifier: "unused".into(),
        };
        let mut tokens = adc.tokens.lock().unwrap();
        for n in 0..MAX_TOKENS {
            tokens.insert(
                n.to_string(),
                Issued {
                    binding: binding.clone(),
                    expires: Instant::now() + Duration::from_secs(600),
                },
            );
        }
        drop(tokens);
        assert_eq!(
            adc.issue(&keys, "mock").unwrap_err().0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert!(!adc.status().to_string().contains("unused"));
    }
}

/// Google external-account ADC speaks RFC 8693; this local issuer validates an
/// opaque workload key instead of a Google identity assertion. No Google call.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Exchange {
    grant_type: String,
    audience: String,
    scope: String,
    requested_token_type: String,
    subject_token_type: String,
    subject_token: String,
    options: Option<String>,
}
pub fn audience(project: &str) -> String {
    format!(
        "//iam.googleapis.com/projects/000000000001/locations/global/workloadIdentityPools/{project}/providers/vertex-shim"
    )
}
async fn exchange(
    State(app): State<App>,
    Form(input): Form<Exchange>,
) -> Result<Response, ApiError> {
    if input.grant_type != "urn:ietf:params:oauth:grant-type:token-exchange"
        || input.requested_token_type != "urn:ietf:params:oauth:token-type:access_token"
        || input.subject_token_type != "urn:ietf:params:oauth:token-type:access_token"
        || input.scope != SCOPE
        || input.options.is_some()
    {
        return Err(ApiError::bad("Unsupported mock token exchange"));
    }
    if input.subject_token.starts_with(PREFIX) {
        return Err(ApiError::unauthorized());
    }
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-api-key",
        input
            .subject_token
            .parse()
            .map_err(|_| ApiError::unauthorized())?,
    );
    let (app, key, identity) = crate::projects::authenticate_exchange(app, &headers).await?;
    if input.audience != audience(&identity.project_id) {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "permission_error",
            "Token audience does not match project",
        ));
    }
    let adc = app.mock_adc.as_ref().ok_or_else(ApiError::unauthorized)?;
    let mut token = adc.issue_key(
        &key,
        key.backends.first().ok_or_else(ApiError::unauthorized)?,
    )?;
    token["issued_token_type"] = json!("urn:ietf:params:oauth:token-type:access_token");
    token["scope"] = json!(SCOPE);
    Ok(([("cache-control", "no-store")], Json(token)).into_response())
}
