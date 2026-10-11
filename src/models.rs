use crate::{ApiError, App, auth, backend::Backend, logging};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Deserialize, Serialize)]
pub struct CatalogModel {
    pub id: String,
    pub display_name: String,
    pub created_at: String,
    #[serde(default)]
    pub max_input_tokens: Option<u64>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub line: Option<String>,
    #[serde(default)]
    pub lifecycle: Option<String>,
}

impl CatalogModel {
    pub fn validate(&self) -> bool {
        crate::model_policy::valid_id(&self.id)
            && !self.display_name.is_empty()
            && self.display_name.len() <= 200
            && !self.display_name.chars().any(char::is_control)
            && (self.created_at.is_empty()
                || chrono::DateTime::parse_from_rfc3339(&self.created_at).is_ok())
            && self.line.as_deref().is_none_or(|s| {
                !s.is_empty()
                    && s.len() <= 50
                    && s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            })
            && self
                .lifecycle
                .as_deref()
                .is_none_or(|s| ["active", "deprecated", "retired"].contains(&s))
    }
    pub fn json(&self) -> Value {
        let mut value = serde_json::to_value(self).expect("serializable model");
        value["type"] = json!("model");
        if self.created_at.is_empty() {
            value.as_object_mut().unwrap().remove("created_at");
        }
        value
    }
}

pub(crate) async fn registry(app: &App) -> Result<auth::Registry, ApiError> {
    let path = app.registry.clone();
    let registry = tokio::task::spawn_blocking(move || {
        let bytes = std::fs::read(path).map_err(|_| ApiError::internal())?;
        serde_json::from_slice(&bytes).map_err(|_| ApiError::internal())
    })
    .await
    .map_err(|_| ApiError::internal())??;
    crate::projects::registry(app, registry).await
}

pub(crate) async fn catalog(app: &App) -> Result<Vec<CatalogModel>, ApiError> {
    if let Some(context) = &app.routing {
        let mut models = std::collections::BTreeMap::new();
        for (profile, target) in &context.targets {
            let Some(backend) = &target.backend else {
                return Err(ApiError(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "upstream_profile_unavailable",
                    "Model upstream profile is unavailable",
                ));
            };
            if !context.backends.iter().any(|b| b == backend.name()) {
                continue;
            }
            let mut selected = app.clone();
            selected.routing = None;
            selected.backend = backend.clone();
            selected.inventory = Some(target.inventory.clone());
            for model in Box::pin(catalog(&selected)).await? {
                if context.config.destination(&model.id) == Some(profile.as_str())
                    && context.denial(&model.id).is_none()
                {
                    models.insert(model.id.clone(), model);
                }
            }
        }
        return Ok(models.into_values().collect());
    }
    if let Some(inventory) = &app.inventory
        && inventory.configured
    {
        return Ok(inventory.catalog());
    }
    match &app.backend {
        Backend::Anthropic(upstream) => upstream.catalog().await,
        Backend::Mock => {
            let mut models = std::collections::BTreeMap::new();
            for (id, model) in app.models.iter() {
                let id = model.upstream_model.as_ref().unwrap_or(id).clone();
                models.insert(
                    id.clone(),
                    CatalogModel {
                        display_name: id.clone(),
                        id,
                        created_at: "1970-01-01T00:00:00Z".into(),
                        max_input_tokens: model.window_tokens,
                        max_tokens: None,
                        line: None,
                        lifecycle: Some("active".into()),
                    },
                );
            }
            Ok(models.into_values().collect())
        }
    }
}

pub(crate) fn permitted(app: &App, key: &auth::Key, id: &str) -> bool {
    policy_permitted(app, key, id)
        && (key.catalog_models
            || key
                .models
                .iter()
                .any(|grant| canonical(app, grant) == canonical(app, id)))
}

pub(crate) fn canonical(app: &App, id: &str) -> String {
    app.models
        .get(id)
        .and_then(|m| m.upstream_model.clone())
        .unwrap_or_else(|| upstream_id(id))
}
pub(crate) fn policy_permitted(app: &App, key: &auth::Key, id: &str) -> bool {
    let id = canonical(app, id);
    app.routing.as_ref().is_none_or(|r| r.denial(&id).is_none())
        && app.inventory.as_ref().is_none_or(|i| i.permits(&id))
        && app
            .project_policy
            .allowed
            .as_ref()
            .is_none_or(|ids| ids.iter().any(|s| canonical(app, s) == id))
        && !app
            .project_policy
            .blocked
            .iter()
            .chain(key.blocked_models.iter())
            .any(|s| canonical(app, s) == id)
}
fn vertex_id(id: &str) -> String {
    match id.rsplit_once('-') {
        Some((base, date)) if date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) => {
            format!("{base}@{date}")
        }
        _ => id.into(),
    }
}

fn upstream_id(id: &str) -> String {
    match id.rsplit_once('@') {
        Some((base, date)) if date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) => {
            format!("{base}-{date}")
        }
        _ => id.into(),
    }
}

fn vertex_catalog(app: &App, key: &auth::Key, all: &[CatalogModel]) -> Vec<CatalogModel> {
    let mut models: Vec<_> = if key.vertex_catalog_models {
        all.iter()
            .map(|model| {
                let mut model = model.clone();
                model.id = vertex_id(&model.id);
                if let Some(window) = app.models.get(&model.id).and_then(|m| m.window_tokens) {
                    model.max_input_tokens = Some(window);
                }
                model
            })
            .collect()
    } else {
        Vec::new()
    };
    for (alias, deployment) in app
        .models
        .iter()
        .filter(|(alias, _)| key.models.contains(alias))
    {
        if models.iter().any(|model| model.id == *alias) {
            continue;
        }
        let upstream = deployment.upstream_model.as_deref().unwrap_or(alias);
        let mut metadata = all
            .iter()
            .find(|model| model.id == upstream)
            .cloned()
            .unwrap_or(CatalogModel {
                id: alias.clone(),
                display_name: alias.clone(),
                created_at: "1970-01-01T00:00:00Z".into(),
                max_input_tokens: None,
                max_tokens: None,
                line: None,
                lifecycle: None,
            });
        metadata.id = alias.clone();
        if let Some(window) = deployment.window_tokens {
            metadata.max_input_tokens = Some(window);
        }
        models.push(metadata);
    }
    for model in all
        .iter()
        .filter(|m| key.models.iter().any(|grant| canonical(app, grant) == m.id))
    {
        if !models.iter().any(|m| canonical(app, &m.id) == model.id) {
            models.push(model.clone());
        }
    }
    models.retain(|m| policy_permitted(app, key, &m.id));
    models
}

pub(crate) async fn resolve_vertex(
    app: &App,
    key: &auth::Key,
    id: &str,
) -> Result<(crate::Model, bool), ApiError> {
    if !policy_permitted(app, key, id) {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "permission_error",
            "Model access denied",
        ));
    }
    let explicit = app
        .models
        .get(id)
        .filter(|_| key.models.iter().any(|m| m == id));
    if !key.vertex_catalog_models
        && explicit.is_none()
        && key
            .models
            .iter()
            .any(|grant| canonical(app, grant) == canonical(app, id))
        && let Some(entry) = catalog(app)
            .await?
            .into_iter()
            .find(|m| m.id == canonical(app, id))
    {
        return Ok((
            crate::Model {
                window_tokens: entry.max_input_tokens,
                upstream_model: Some(entry.id),
            },
            true,
        ));
    }
    if !key.vertex_catalog_models {
        return explicit
            .cloned()
            .map(|model| (model, false))
            .ok_or(ApiError(
                StatusCode::FORBIDDEN,
                "permission_error",
                "Model/backend access denied",
            ));
    }
    let target = app
        .models
        .get(id)
        .and_then(|model| model.upstream_model.clone())
        .unwrap_or_else(|| upstream_id(id));
    let entry = catalog(app)
        .await?
        .into_iter()
        .find(|model| model.id == target);
    if let Some(entry) = entry {
        let override_window = app.models.get(id).and_then(|model| model.window_tokens);
        return Ok((
            crate::Model {
                window_tokens: override_window.or(entry.max_input_tokens),
                upstream_model: Some(entry.id),
            },
            override_window.is_none(),
        ));
    }
    // Explicit fixture/custom deployments continue to work outside the upstream catalog.
    explicit
        .cloned()
        .map(|model| (model, false))
        .ok_or_else(|| ApiError::bad("Unknown upstream model"))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub limit: Option<usize>,
    pub after_id: Option<String>,
    pub before_id: Option<String>,
    pub client_api: Option<String>,
}

pub async fn list(
    State(app): State<App>,
    headers: HeaderMap,
    Query(page): Query<Page>,
) -> Result<Json<Value>, ApiError> {
    let (app, key, _) = crate::projects::authenticate(app, &headers).await?;
    let limit = page.limit.unwrap_or(20);
    if !(1..=1000).contains(&limit)
        || (page.after_id.is_some() && page.before_id.is_some())
        || page
            .client_api
            .as_deref()
            .is_some_and(|api| api != "vertex")
    {
        return Err(ApiError::bad("Invalid model pagination"));
    }
    let start = std::time::Instant::now();
    let all = match catalog(&app).await {
        Ok(models) => models,
        Err(error) => {
            logging::emit(
                "error",
                "model_discovery_failed",
                json!({"backend":app.backend.name(),"error_type":error.1,"http_status":error.0.as_u16()}),
            );
            return Err(error);
        }
    };
    let models: Vec<_> = if page.client_api.as_deref() == Some("vertex") {
        vertex_catalog(&app, &key, &all)
    } else {
        all.into_iter()
            .filter(|m| permitted(&app, &key, &m.id))
            .collect()
    };
    let position = |id: &str| {
        models
            .iter()
            .position(|m| m.id == id)
            .ok_or_else(|| ApiError::bad("Unknown model cursor"))
    };
    let from = page
        .after_id
        .as_deref()
        .map(position)
        .transpose()?
        .map_or(0, |p| p + 1);
    let to = page
        .before_id
        .as_deref()
        .map(position)
        .transpose()?
        .unwrap_or(models.len());
    let (from, to, has_more) = if page.before_id.is_some() {
        let start = to.saturating_sub(limit);
        (start, to, start > 0)
    } else {
        let end = (from + limit).min(to);
        (from, end, end < to)
    };
    let data: Vec<Value> = models[from..to].iter().map(CatalogModel::json).collect();
    logging::emit(
        "info",
        "model_discovery",
        json!({"backend":app.backend.name(),"model_count":data.len(),"latency_ms":start.elapsed().as_millis() as u64}),
    );
    Ok(Json(
        json!({"has_more":has_more,"first_id":data.first().map(|m| &m["id"]),"last_id":data.last().map(|m| &m["id"]),"data":data}),
    ))
}

pub async fn detail(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(page): Query<Page>,
) -> Result<Json<Value>, ApiError> {
    let (app, key, _) = crate::projects::authenticate(app, &headers).await?;
    if let Some(api) = page.client_api.as_deref() {
        if api != "vertex" {
            return Err(ApiError::bad("Invalid client API"));
        }
        return vertex_catalog(&app, &key, &catalog(&app).await?)
            .into_iter()
            .find(|m| m.id == id || canonical(&app, &m.id) == canonical(&app, &id))
            .map(|m| Json(m.json()))
            .ok_or(ApiError(
                StatusCode::FORBIDDEN,
                "permission_error",
                "Model access denied",
            ));
    }
    if !permitted(&app, &key, &id) {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "permission_error",
            "Model access denied",
        ));
    }
    catalog(&app)
        .await?
        .into_iter()
        .find(|m| m.id == id)
        .map(|m| Json(m.json()))
        .ok_or(ApiError(
            StatusCode::NOT_FOUND,
            "not_found_error",
            "Unknown upstream model",
        ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn vertex_catalog_permissions_aliases_and_context() {
        let dir = tempfile::tempdir().unwrap();
        let app = App {
            routing: None,
            inventory: None,
            project_policy: Default::default(),
            accounts: None,
            upstreams: std::sync::Arc::new(std::collections::BTreeMap::new()),
            backend: Backend::Mock,
            store: crate::telemetry::Store::open(&dir.path().join("db.sqlite")).unwrap(),
            registry: dir.path().join("keys.json"),
            admin_verifier: auth::fingerprint("admin"),
            mock_adc: None,
            models: std::sync::Arc::new(std::collections::BTreeMap::from([
                (
                    "claude-sonnet-4-5@20250929".into(),
                    crate::Model {
                        window_tokens: Some(200000),
                        upstream_model: Some("claude-sonnet-4-5-20250929".into()),
                    },
                ),
                (
                    "fixture-opus-deployment".into(),
                    crate::Model {
                        window_tokens: Some(1000000),
                        upstream_model: Some("claude-opus-4-6".into()),
                    },
                ),
            ])),
        };
        let mut key: auth::Key =
            serde_json::from_value(json!({"client_id":"client", "key_id":"key",
            "sha256":auth::fingerprint("local"), "models":["claude-sonnet-4-5@20250929"],
            "backends":["mock"], "catalog_models":true}))
            .unwrap();
        let all = catalog(&app).await.unwrap();
        let restricted = vertex_catalog(&app, &key, &all);
        assert_eq!(restricted.len(), 1);
        assert_eq!(restricted[0].id, "claude-sonnet-4-5@20250929");
        assert_eq!(
            resolve_vertex(&app, &key, "claude-opus-4-6")
                .await
                .err()
                .unwrap()
                .0,
            StatusCode::FORBIDDEN
        );
        key.vertex_catalog_models = true;
        let models = vertex_catalog(&app, &key, &all);
        assert_eq!(models.len(), 2);
        assert!(models.iter().any(|model| model.id == "claude-opus-4-6"));
        assert!(
            !models
                .iter()
                .any(|model| model.id == "fixture-opus-deployment")
        );
        let (model, from_catalog) = resolve_vertex(&app, &key, "claude-opus-4-6").await.unwrap();
        assert_eq!(model.upstream_model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(model.window_tokens, Some(1000000));
        assert!(from_catalog);
        let (model, from_catalog) = resolve_vertex(&app, &key, "claude-sonnet-4-5@20250929")
            .await
            .unwrap();
        assert_eq!(
            model.upstream_model.as_deref(),
            Some("claude-sonnet-4-5-20250929")
        );
        assert!(!from_catalog);
        let (model, _) = resolve_vertex(&app, &key, "claude-sonnet-4-5-20250929")
            .await
            .unwrap();
        assert_eq!(
            model.upstream_model.as_deref(),
            Some("claude-sonnet-4-5-20250929")
        );
        assert_eq!(
            resolve_vertex(&app, &key, "invented-model")
                .await
                .err()
                .unwrap()
                .0,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn catalogue_metadata_is_bounded_and_excludes_unrecognized_fields() {
        let value = json!({"id":"claude-sonnet-4-6","display_name":"Claude Sonnet 4.6",
            "created_at":"2026-02-17T00:00:00Z","max_input_tokens":1000000,
            "line":"new_family","lifecycle":"active","credential":"private_catalog_marker"});
        let mut model: CatalogModel = serde_json::from_value(value).unwrap();
        assert!(model.validate());
        assert!(!model.json().to_string().contains("private_catalog_marker"));
        model.id = "https://untrusted.example/model".into();
        assert!(!model.validate());
        model.id = "claude-sonnet-4-6".into();
        model.display_name = "untrusted\nlog line".into();
        assert!(!model.validate());
        model.display_name = "x".repeat(201);
        assert!(!model.validate());
    }
}
