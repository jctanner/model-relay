//! Persistent project and client-credential control plane. Provider credentials
//! remain server-owned; projects reference a configured upstream profile.
use crate::{ApiError, App, auth, telemetry::Filter};
use axum::{
    Json, Router,
    extract::{Path, Query, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub const DEFAULT_PROJECT: &str = "vertex-shim-local";
fn default_profile() -> String {
    "default".into()
}
fn yes() -> bool {
    true
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub project_id: String,
    pub display_name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default = "default_profile")]
    pub upstream_profile: String,
}
impl Project {
    fn validate(&self) -> Result<(), ApiError> {
        if !valid_id(&self.project_id)
            || self.display_name.is_empty()
            || self.display_name.len() > 200
            || self.display_name.chars().any(char::is_control)
            || self.labels.len() > 32
            || self.labels.iter().any(|(k, v)| {
                k.is_empty()
                    || k.len() > 64
                    || v.len() > 256
                    || k.chars().chain(v.chars()).any(char::is_control)
            })
        {
            return Err(ApiError::bad("Invalid project fields"));
        }
        if !valid_id(&self.upstream_profile) {
            return Err(ApiError::bad("Unknown upstream profile"));
        }
        Ok(())
    }
}
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 63
        && id.as_bytes()[0].is_ascii_lowercase()
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !id.ends_with('-')
}
fn missing() -> ApiError {
    ApiError(
        StatusCode::NOT_FOUND,
        "not_found_error",
        "Unknown project or credential",
    )
}
fn conflict() -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        "conflict_error",
        "Project already exists",
    )
}
fn sql<T>(result: rusqlite::Result<T>) -> Result<T, ApiError> {
    result.map_err(|_| ApiError::internal())
}
fn encode<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("serializable control record")
}
fn decode<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, ApiError> {
    serde_json::from_str(value).map_err(|_| ApiError::internal())
}

pub fn migrate(connection: &Connection) -> rusqlite::Result<()> {
    // Seed once. Deleted builtins must not reappear at process restart.
    connection.execute_batch("CREATE TABLE IF NOT EXISTS projects (project_id TEXT PRIMARY KEY, payload TEXT NOT NULL, epoch TEXT NOT NULL, deleted INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE IF NOT EXISTS project_credentials (key_id TEXT PRIMARY KEY, project_id TEXT NOT NULL, payload TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS credentials_project ON project_credentials(project_id);
        CREATE INDEX IF NOT EXISTS telemetry_project ON requests(json_extract(payload,'$.project_id'), sequence);
        CREATE TABLE IF NOT EXISTS control_migrations (version INTEGER PRIMARY KEY);")?;
    let transaction = connection.unchecked_transaction()?;
    if !transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM control_migrations WHERE version=1)",
        [],
        |r| r.get::<_, bool>(0),
    )? {
        let project = Project {
            project_id: DEFAULT_PROJECT.into(),
            display_name: "Built-in development project".into(),
            enabled: true,
            labels: BTreeMap::from([("builtin".into(), "true".into())]),
            upstream_profile: default_profile(),
        };
        transaction.execute(
            "INSERT INTO projects(project_id,payload,epoch) VALUES (?1,?2,?3)",
            params![
                DEFAULT_PROJECT,
                encode(&project),
                uuid::Uuid::new_v4().to_string()
            ],
        )?;
        transaction.execute("UPDATE requests SET payload=json_set(payload,'$.project_id',?1) WHERE json_extract(payload,'$.project_id') IS NULL",[DEFAULT_PROJECT])?;
        transaction.execute("INSERT INTO control_migrations(version) VALUES (1)", [])?;
    }
    transaction.commit()
}
fn load(connection: &Connection, id: &str) -> Result<(Project, String), ApiError> {
    let row: Option<(String, String)> = sql(connection
        .query_row(
            "SELECT payload,epoch FROM projects WHERE project_id=?1 AND deleted=0",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional())?;
    let (payload, epoch) = row.ok_or_else(missing)?;
    Ok((decode(&payload)?, epoch))
}

pub(crate) async fn registry(
    app: &App,
    mut registry: auth::Registry,
) -> Result<auth::Registry, ApiError> {
    app.store
        .control(false, move |connection| {
            let mut projects = BTreeMap::new();
            let mut statement =
                sql(connection
                    .prepare("SELECT project_id,payload,epoch FROM projects WHERE deleted=0"))?;
            let rows = sql(statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            }))?;
            for row in rows {
                let (id, payload, epoch) = sql(row)?;
                let project: Project = decode(&payload)?;
                if project.enabled {
                    projects.insert(id, epoch);
                }
            }
            let mut statement = sql(connection.prepare("SELECT payload FROM project_credentials"))?;
            let rows = sql(statement.query_map([], |row| row.get::<_, String>(0)))?;
            for row in rows {
                registry.keys.push(decode(&sql(row)?)?);
            }
            registry.keys.retain_mut(|key| {
                let id = key.project_id.get_or_insert_with(|| DEFAULT_PROJECT.into());
                if let Some(epoch) = projects.get(id) {
                    key.project_epoch = epoch.clone();
                    true
                } else {
                    false
                }
            });
            // Reject every ambiguous identity while leaving unrelated tenants usable.
            let mut ids = BTreeMap::new();
            let mut verifiers = BTreeMap::new();
            for key in &registry.keys {
                *ids.entry(key.key_id.clone()).or_insert(0usize) += 1;
                *verifiers.entry(key.sha256.clone()).or_insert(0usize) += 1;
            }
            registry
                .keys
                .retain(|k| ids[&k.key_id] == 1 && verifiers[&k.sha256] == 1);
            Ok(registry)
        })
        .await
}

pub(crate) async fn check_route(app: &App, key: &auth::Key, route: &str) -> Result<(), ApiError> {
    let id = key.project_id.as_deref().unwrap_or(DEFAULT_PROJECT);
    // "local" was the original curl fixture's project. It aliases only the
    // built-in project, never any administrator-created tenant.
    let legacy_alias = id == DEFAULT_PROJECT
        && (route == "local"
            || app
                .mock_adc
                .as_ref()
                .is_some_and(|adc| adc.project_id() == route));
    if route != id && !legacy_alias {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "permission_error",
            "Credential does not belong to the requested project",
        ));
    }
    Ok(())
}

pub fn routes() -> Router<App> {
    Router::new()
        .route("/api/private/v1/upstream-profiles", get(profiles))
        .route(
            "/api/private/v1/projects/{project}/auth/status",
            get(auth_status),
        )
        .route("/api/private/v1/projects", get(list).post(create))
        .route(
            "/api/private/v1/projects/{project}",
            get(detail).put(update).delete(remove),
        )
        .route(
            "/api/private/v1/projects/{project}/credentials",
            get(credentials).post(create_credential),
        )
        .route(
            "/api/private/v1/projects/{project}/credentials/{key}",
            get(credential)
                .put(update_credential)
                .delete(remove_credential),
        )
        .route(
            "/api/private/v1/projects/{project}/credentials/{key}/rotate",
            post(rotate_credential),
        )
        .route(
            "/api/private/v1/projects/{project}/telemetry/requests",
            get(requests),
        )
        .route(
            "/api/private/v1/projects/{project}/telemetry/summary",
            get(summary),
        )
}
pub async fn protect(
    State(app): State<App>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Response {
    let mut response = match auth::admin(&headers, &app.admin_verifier) {
        Ok(()) => next.run(request).await,
        Err(e) => e.into_response(),
    };
    response
        .headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    response
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    after_id: Option<String>,
    limit: Option<u16>,
}
async fn list(State(app): State<App>, Query(page): Query<Page>) -> Result<Json<Value>, ApiError> {
    let limit = page.limit.unwrap_or(100) as usize;
    if !(1..=500).contains(&limit) || page.after_id.as_ref().is_some_and(|s| !valid_id(s)) {
        return Err(ApiError::bad("Invalid project pagination"));
    }
    app.store.control(false, move |c| {
        let mut statement=sql(c.prepare("SELECT payload FROM projects WHERE deleted=0 AND project_id>?1 ORDER BY project_id LIMIT ?2"))?;
        let rows=sql(statement.query_map(params![page.after_id.unwrap_or_default(),(limit+1) as u64], |r| r.get::<_,String>(0)))?;
        let mut items=Vec::new(); for row in rows { items.push(decode::<Value>(&sql(row)?)?); }
        let more=items.len()>limit; items.truncate(limit);
        Ok(Json(json!({"next_after_id":if more {items.last().map(|p| p["project_id"].clone())} else {None},"items":items})))
    }).await
}
async fn detail(State(app): State<App>, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    app.store
        .control(false, move |c| Ok(Json(json!(load(c, &id)?.0))))
        .await
}
async fn create(
    State(app): State<App>,
    Json(project): Json<Project>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    project.validate()?;
    validate_profile(&app, &project.upstream_profile)?;
    app.store.control(true,move |c| {
        let exists: bool=sql(c.query_row("SELECT EXISTS(SELECT 1 FROM projects WHERE project_id=?1 AND deleted=0)",[&project.project_id],|r|r.get(0)))?;
        if exists { return Err(conflict()); }
        sql(c.execute("INSERT INTO projects(project_id,payload,epoch,deleted) VALUES (?1,?2,?3,0) ON CONFLICT(project_id) DO UPDATE SET payload=excluded.payload,epoch=excluded.epoch,deleted=0",params![project.project_id,encode(&project),uuid::Uuid::new_v4().to_string()]))?;
        Ok((StatusCode::CREATED,Json(json!(project))))
    }).await
}
async fn update(
    State(app): State<App>,
    Path(id): Path<String>,
    Json(project): Json<Project>,
) -> Result<Json<Value>, ApiError> {
    project.validate()?;
    validate_profile(&app, &project.upstream_profile)?;
    if project.project_id != id {
        return Err(ApiError::bad("Project ID is immutable"));
    }
    app.store.control(true,move |c| {
        let (previous,_)=load(c,&id)?;
        // Disabling changes the incarnation so re-enabling cannot revive issued tokens.
        sql(c.execute("UPDATE projects SET payload=?2,epoch=CASE WHEN ?3 THEN ?4 ELSE epoch END WHERE project_id=?1",params![id,encode(&project),(previous.enabled && !project.enabled) || previous.upstream_profile != project.upstream_profile,uuid::Uuid::new_v4().to_string()]))?;
        Ok(Json(json!(project)))
    }).await
}
async fn remove(State(app): State<App>, Path(id): Path<String>) -> Result<StatusCode, ApiError> {
    app.store
        .control(true, move |c| {
            load(c, &id)?;
            sql(c.execute("DELETE FROM project_credentials WHERE project_id=?1", [&id]))?;
            sql(c.execute(
                "UPDATE projects SET deleted=1,epoch=?2 WHERE project_id=?1",
                params![id, uuid::Uuid::new_v4().to_string()],
            ))?;
            Ok(StatusCode::NO_CONTENT)
        })
        .await
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialInput {
    client_id: String,
    #[serde(default)]
    models: Vec<String>,
    #[serde(default)]
    backends: Vec<String>,
    #[serde(default)]
    catalog_models: bool,
    #[serde(default)]
    vertex_catalog_models: bool,
    #[serde(default)]
    mock_adc: bool,
}
impl CredentialInput {
    fn key(self, project: String, key_id: String, verifier: String) -> Result<auth::Key, ApiError> {
        if self.client_id.is_empty()
            || self.client_id.len() > 128
            || !self
                .client_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            || self.models.len() > 1000
            || self.models.iter().any(|s| {
                s.is_empty()
                    || s.len() > 200
                    || !s
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"@._-".contains(&b))
            })
            || self.backends.len() > 2
            || self
                .backends
                .iter()
                .any(|b| b != "mock" && b != "anthropic")
        {
            return Err(ApiError::bad("Invalid credential grants"));
        }
        Ok(auth::Key {
            project_id: Some(project),
            project_epoch: String::new(),
            key_id,
            client_id: self.client_id,
            sha256: verifier,
            models: self.models,
            backends: self.backends,
            catalog_models: self.catalog_models,
            vertex_catalog_models: self.vertex_catalog_models,
            mock_adc: self.mock_adc,
        })
    }
}
fn public_key(key: &auth::Key) -> Value {
    let mut value = json!(key);
    value.as_object_mut().unwrap().remove("sha256");
    value
}
fn get_key(c: &Connection, project: &str, id: &str) -> Result<auth::Key, ApiError> {
    load(c, project)?;
    let value: Option<String> = sql(c
        .query_row(
            "SELECT payload FROM project_credentials WHERE project_id=?1 AND key_id=?2",
            params![project, id],
            |r| r.get(0),
        )
        .optional())?;
    decode(&value.ok_or_else(missing)?)
}
async fn credentials(
    State(app): State<App>,
    Path(project): Path<String>,
    Query(page): Query<Page>,
) -> Result<Json<Value>, ApiError> {
    let limit = page.limit.unwrap_or(100) as usize;
    if !(1..=500).contains(&limit) || page.after_id.as_ref().is_some_and(|s| s.len() > 128) {
        return Err(ApiError::bad("Invalid credential pagination"));
    }
    app.store.control(false,move |c| {
        load(c,&project)?;
        let mut st=sql(c.prepare("SELECT payload FROM project_credentials WHERE project_id=?1 AND key_id>?2 ORDER BY key_id LIMIT ?3"))?;
        let rows=sql(st.query_map(params![project,page.after_id.unwrap_or_default(),(limit+1) as u64],|r|r.get::<_,String>(0)))?;
        let mut items=Vec::new(); for row in rows { items.push(public_key(&decode(&sql(row)?)?)); }
        let more=items.len()>limit;items.truncate(limit);
        Ok(Json(json!({"items":items,"next_after_id":if more {items.last().map(|v|v["key_id"].clone())} else {None}})))
    }).await
}
async fn credential(
    State(app): State<App>,
    Path((project, id)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    app.store
        .control(false, move |c| {
            Ok(Json(public_key(&get_key(c, &project, &id)?)))
        })
        .await
}
fn secret() -> String {
    format!(
        "shim_{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}
async fn create_credential(
    State(app): State<App>,
    Path(project): Path<String>,
    Json(input): Json<CredentialInput>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let access = secret();
    let key = input.key(
        project.clone(),
        format!("key-{}", uuid::Uuid::new_v4().simple()),
        auth::fingerprint(&access),
    )?;
    app.store
        .control(true, move |c| {
            load(c, &project)?;
            sql(c.execute(
                "INSERT INTO project_credentials(key_id,project_id,payload) VALUES (?1,?2,?3)",
                params![key.key_id, project, encode(&key)],
            ))?;
            let mut result = public_key(&key);
            result["api_key"] = json!(access);
            Ok((StatusCode::CREATED, Json(result)))
        })
        .await
}
async fn update_credential(
    State(app): State<App>,
    Path((project, id)): Path<(String, String)>,
    Json(input): Json<CredentialInput>,
) -> Result<Json<Value>, ApiError> {
    app.store
        .control(true, move |c| {
            let old = get_key(c, &project, &id)?;
            let key = input.key(project, id.clone(), old.sha256)?;
            sql(c.execute(
                "UPDATE project_credentials SET payload=?2 WHERE key_id=?1",
                params![id, encode(&key)],
            ))?;
            Ok(Json(public_key(&key)))
        })
        .await
}
async fn rotate_credential(
    State(app): State<App>,
    Path((project, id)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    app.store
        .control(true, move |c| {
            let mut key = get_key(c, &project, &id)?;
            let access = secret();
            key.sha256 = auth::fingerprint(&access);
            sql(c.execute(
                "UPDATE project_credentials SET payload=?2 WHERE key_id=?1",
                params![id, encode(&key)],
            ))?;
            let mut result = public_key(&key);
            result["api_key"] = json!(access);
            Ok(Json(result))
        })
        .await
}
async fn remove_credential(
    State(app): State<App>,
    Path((project, id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    app.store
        .control(true, move |c| {
            get_key(c, &project, &id)?;
            sql(c.execute("DELETE FROM project_credentials WHERE key_id=?1", [id]))?;
            Ok(StatusCode::NO_CONTENT)
        })
        .await
}
async fn telemetry(
    app: App,
    project: String,
    mut filter: Filter,
    summary: bool,
) -> Result<Json<Value>, ApiError> {
    if !valid_id(&project) {
        return Err(ApiError::bad("Invalid project ID"));
    }
    if filter.project_id.as_ref().is_some_and(|id| id != &project) {
        return Err(ApiError::bad("Conflicting project filter"));
    }
    filter.project_id = Some(project.clone());
    filter.validate()?;
    // Historical records remain queryable after deleting a project.
    let mut value = app.store.read(move |c| filter.list(c)).await?;
    if summary {
        value = crate::telemetry::summary(value);
    }
    value["project_id"] = json!(project);
    Ok(Json(value))
}
async fn requests(
    State(app): State<App>,
    Path(project): Path<String>,
    Query(filter): Query<Filter>,
) -> Result<Json<Value>, ApiError> {
    telemetry(app, project, filter, false).await
}
async fn summary(
    State(app): State<App>,
    Path(project): Path<String>,
    Query(filter): Query<Filter>,
) -> Result<Json<Value>, ApiError> {
    telemetry(app, project, filter, true).await
}

fn validate_profile(app: &App, id: &str) -> Result<(), ApiError> {
    if id == "default" || app.upstreams.contains_key(id) {
        Ok(())
    } else {
        Err(ApiError::bad("Unknown upstream profile"))
    }
}
pub(crate) struct Selection {
    pub backend: crate::backend::Backend,
    pub profile_id: String,
}
pub(crate) async fn backend_for_key(app: &App, key: &auth::Key) -> Result<Selection, ApiError> {
    let id = key
        .project_id
        .clone()
        .unwrap_or_else(|| DEFAULT_PROJECT.into());
    let epoch = key.project_epoch.clone();
    let project = app
        .store
        .control(false, move |c| {
            let (project, current_epoch) = load(c, &id)?;
            if !project.enabled || current_epoch != epoch {
                return Err(ApiError::unauthorized());
            }
            Ok(project)
        })
        .await?;
    let backend = if project.upstream_profile == "default" {
        app.backend.clone()
    } else {
        app.upstreams
            .get(&project.upstream_profile)
            .cloned()
            .ok_or(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "upstream_profile_unavailable",
                "Project upstream profile is unavailable",
            ))?
    };
    if !key.backends.iter().any(|b| b == backend.name()) {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "permission_error",
            "Model/backend access denied",
        ));
    }
    Ok(Selection {
        backend,
        profile_id: project.upstream_profile,
    })
}
pub(crate) async fn authenticate(
    mut app: App,
    headers: &HeaderMap,
) -> Result<(App, auth::Key, auth::Identity), ApiError> {
    let registry = crate::models::registry(&app).await?;
    let (key, mut identity) =
        auth::authenticate_key(headers, &registry, "", app.mock_adc.as_deref())?;
    let selection = backend_for_key(&app, key).await?;
    app.backend = selection.backend;
    identity.upstream_profile = selection.profile_id;
    Ok((app, key.clone(), identity))
}

#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case", deny_unknown_fields)]
pub enum UpstreamProfile {
    Mock,
    BearerFile {
        base_url: String,
        token_file: std::path::PathBuf,
        #[serde(default)]
        synthetic: bool,
        workspace: Option<String>,
    },
    ClaudeOauth {
        base_url: String,
        oauth_dir: std::path::PathBuf,
        #[serde(default)]
        synthetic: bool,
        workspace: Option<String>,
    },
}
impl UpstreamProfile {
    pub fn backend(self) -> Result<crate::backend::Backend, Box<dyn std::error::Error>> {
        use crate::{
            backend::{Backend, Upstream},
            credentials::OAuth,
        };
        let timeout = std::time::Duration::from_secs(120);
        Ok(match self {
            Self::Mock => Backend::Mock,
            Self::BearerFile {
                base_url,
                token_file,
                synthetic,
                workspace,
            } => {
                if !token_file.is_absolute() {
                    return Err("Profile token file must be absolute".into());
                }
                Backend::Anthropic(Upstream::new(
                    &base_url, token_file, synthetic, workspace, timeout,
                )?)
            }
            Self::ClaudeOauth {
                base_url,
                oauth_dir,
                synthetic,
                workspace,
            } => {
                if !oauth_dir.is_absolute() {
                    return Err("Profile OAuth directory must be absolute".into());
                }
                Backend::Anthropic(
                    Upstream::new(
                        &base_url,
                        std::path::PathBuf::new(),
                        synthetic,
                        workspace,
                        timeout,
                    )?
                    .with_oauth(OAuth::new(oauth_dir)?),
                )
            }
        })
    }
}

async fn auth_status(
    State(app): State<App>,
    Path(project): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let p = app
        .store
        .control(false, move |c| Ok(load(c, &project)?.0))
        .await?;
    let backend = if p.upstream_profile == "default" {
        app.backend.clone()
    } else {
        app.upstreams
            .get(&p.upstream_profile)
            .cloned()
            .ok_or(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "upstream_profile_unavailable",
                "Project upstream profile is unavailable",
            ))?
    };
    let state = match &backend {
        crate::backend::Backend::Mock => json!({"mode":"none","state":"mock"}),
        crate::backend::Backend::Anthropic(u) => u.credentials.status().await,
    };
    Ok(Json(
        json!({"project_id":p.project_id,"enabled":p.enabled,"upstream_profile":p.upstream_profile,"backend":backend.name(),"synthetic":backend.synthetic(),"upstream_auth":state}),
    ))
}
async fn profiles(State(app): State<App>) -> Json<Value> {
    let info = |id: &str, b: &crate::backend::Backend| json!({"profile_id":id,"backend":b.name(),"synthetic":b.synthetic(),"auth_mode":match b {crate::backend::Backend::Mock=>"none",crate::backend::Backend::Anthropic(u)=>u.credentials.kind()}});
    let mut items = vec![info("default", &app.backend)];
    items.extend(app.upstreams.iter().map(|(id, b)| info(id, b)));
    Json(json!({"items":items}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Model, backend::Backend, mock_adc::MockAdc, telemetry::Store};
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use tower::ServiceExt;
    fn app(dir: &std::path::Path) -> App {
        let registry = dir.join("keys.json");
        std::fs::write(&registry,json!({"keys":[{"client_id":"legacy","key_id":"legacy-key","sha256":auth::fingerprint("legacy-secret"),"models":["claude-fixture"],"backends":["mock"],"mock_adc":true}]}).to_string()).unwrap();
        App {
            backend: Backend::Mock,
            upstreams: Arc::new(BTreeMap::from([("second-account".into(), Backend::Mock)])),
            store: Store::open(&dir.join("db.sqlite")).unwrap(),
            registry,
            admin_verifier: auth::fingerprint("admin"),
            models: Arc::new(BTreeMap::from([(
                "claude-fixture".into(),
                Model {
                    window_tokens: Some(200_000),
                    upstream_model: Some("claude-fixture".into()),
                },
            )])),
            mock_adc: Some(Arc::new(
                MockAdc::new("legacy-key".into(), DEFAULT_PROJECT.into(), 600).unwrap(),
            )),
        }
    }
    async fn call(
        router: Router,
        method: &str,
        path: &str,
        key: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let response = router
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("authorization", format!("Bearer {key}"))
                    .header("anthropic-version", "2023-06-01")
                    .header("content-type", "application/json")
                    .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
    async fn token(app: &App, key: &str, project: &str) -> (StatusCode, Value) {
        let body = format!(
            "grant_type=urn:ietf:params:oauth:grant-type:token-exchange&audience={}&scope=https://www.googleapis.com/auth/cloud-platform&requested_token_type=urn:ietf:params:oauth:token-type:access_token&subject_token_type=urn:ietf:params:oauth:token-type:access_token&subject_token={key}",
            crate::mock_adc::audience(project)
        );
        let response = crate::mock_adc::metadata(app.clone())
            .oneshot(
                Request::post("/v1/token")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }
    fn grants(client: &str) -> Value {
        json!({"client_id":client,"models":["claude-fixture"],"backends":["mock"],"catalog_models":true,"vertex_catalog_models":true,"mock_adc":true})
    }
    fn messages() -> Value {
        json!({"model":"claude-fixture","max_tokens":16,"messages":[{"role":"user","content":"private_project_prompt"}]})
    }
    #[tokio::test]
    async fn projects_crud_routing_revocation_and_persistent_telemetry() {
        let dir = tempfile::tempdir().unwrap();
        let app = app(dir.path());
        let admin = crate::admin(app.clone());
        let inference = crate::inference(app.clone());
        assert_eq!(
            call(
                admin.clone(),
                "POST",
                "/api/private/v1/projects",
                "legacy-secret",
                Some(json!({}))
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
        let (status, projects) = call(
            admin.clone(),
            "GET",
            "/api/private/v1/projects",
            "admin",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(projects["items"][0]["project_id"], DEFAULT_PROJECT);
        for id in ["agent-alpha", "agent-beta"] {
            assert_eq!(
                call(
                    admin.clone(),
                    "POST",
                    "/api/private/v1/projects",
                    "admin",
                    Some(json!({"project_id":id,"display_name":id}))
                )
                .await
                .0,
                StatusCode::CREATED
            );
        }
        assert_eq!(
            call(
                admin.clone(),
                "POST",
                "/api/private/v1/projects",
                "admin",
                Some(json!({"project_id":"agent-alpha","display_name":"duplicate"}))
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            call(
                admin.clone(),
                "POST",
                "/api/private/v1/projects",
                "admin",
                Some(
                    json!({"project_id":"unsafe","display_name":"x","upstream_profile":"unknown"})
                )
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        let (_, alpha) = call(
            admin.clone(),
            "POST",
            "/api/private/v1/projects/agent-alpha/credentials",
            "admin",
            Some(grants("alpha")),
        )
        .await;
        let (_, beta) = call(
            admin.clone(),
            "POST",
            "/api/private/v1/projects/agent-beta/credentials",
            "admin",
            Some(grants("beta")),
        )
        .await;
        let a = alpha["api_key"].as_str().unwrap();
        let b = beta["api_key"].as_str().unwrap();
        let aid = alpha["key_id"].as_str().unwrap();
        let bid = beta["key_id"].as_str().unwrap();
        let beta_path = format!("/api/private/v1/projects/agent-beta/credentials/{bid}");
        assert_eq!(
            call(
                admin.clone(),
                "PUT",
                &beta_path,
                "admin",
                Some(json!({"client_id":"beta","backends":["mock"]}))
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            call(
                inference.clone(),
                "POST",
                "/v1/messages",
                b,
                Some(messages())
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(
                admin.clone(),
                "PUT",
                &beta_path,
                "admin",
                Some(grants("beta"))
            )
            .await
            .0,
            StatusCode::OK
        );
        let (_, key_list) = call(
            admin.clone(),
            "GET",
            "/api/private/v1/projects/agent-beta/credentials?limit=1",
            "admin",
            None,
        )
        .await;
        assert_eq!(key_list["items"].as_array().unwrap().len(), 1);
        assert!(key_list["items"][0].get("sha256").is_none());
        let (sa, sb) = tokio::join!(
            call(
                inference.clone(),
                "POST",
                "/v1/messages",
                a,
                Some(messages())
            ),
            call(
                inference.clone(),
                "POST",
                "/v1/messages",
                b,
                Some(messages())
            )
        );
        assert_eq!(sa.0, StatusCode::OK);
        assert_eq!(sb.0, StatusCode::OK);
        let vertex = |id: &str| {
            format!(
                "/v1/projects/{id}/locations/us-east5/publishers/anthropic/models/claude-fixture:rawPredict"
            )
        };
        let vbody = json!({"max_tokens":16,"anthropic_version":"vertex-2023-10-16","messages":[{"role":"user","content":"private_project_prompt"}]});
        assert_eq!(
            call(
                inference.clone(),
                "POST",
                &vertex("agent-beta"),
                a,
                Some(vbody.clone())
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        let (status, t) = token(&app, a, "agent-alpha").await;
        assert_eq!(status, StatusCode::OK);
        let access = t["access_token"].as_str().unwrap();
        assert_eq!(token(&app, a, "agent-beta").await.0, StatusCode::FORBIDDEN);
        assert_eq!(
            token(&app, access, "agent-alpha").await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            call(
                inference.clone(),
                "POST",
                &vertex("agent-alpha"),
                access,
                Some(vbody.clone())
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            call(
                inference.clone(),
                "POST",
                &vertex("agent-beta"),
                access,
                Some(vbody.clone())
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(
                admin.clone(),
                "GET",
                "/api/private/v1/projects",
                access,
                None
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
        let (_, read) = call(
            admin.clone(),
            "GET",
            &format!("/api/private/v1/projects/agent-alpha/credentials/{aid}"),
            "admin",
            None,
        )
        .await;
        assert!(read.get("api_key").is_none() && read.get("sha256").is_none());
        app.store.flush().await;
        let (_, telemetry) = call(
            admin.clone(),
            "GET",
            "/api/private/v1/projects/agent-alpha/telemetry/requests",
            "admin",
            None,
        )
        .await;
        assert_eq!(telemetry["items"].as_array().unwrap().len(), 2);
        assert!(
            telemetry["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|v| v["project_id"] == "agent-alpha")
        );
        let serialized = telemetry.to_string();
        assert!(
            !serialized.contains(a)
                && !serialized.contains(access)
                && !serialized.contains("private_project_prompt")
        );
        let (_, summary) = call(
            admin.clone(),
            "GET",
            "/api/private/v1/telemetry/summary?project_id=agent-alpha",
            "admin",
            None,
        )
        .await;
        assert_eq!(summary["generation_tokens"]["output_tokens"], 6);
        assert_eq!(
            call(
                admin.clone(),
                "GET",
                "/api/private/v1/projects/agent-alpha/telemetry/requests?project_id=agent-beta",
                "admin",
                None
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        let (_, rotated) = call(
            admin.clone(),
            "POST",
            &format!("/api/private/v1/projects/agent-alpha/credentials/{aid}/rotate"),
            "admin",
            None,
        )
        .await;
        let replacement = rotated["api_key"].as_str().unwrap();
        assert_ne!(replacement, a);
        for key in [a, access] {
            assert_eq!(
                call(
                    inference.clone(),
                    "POST",
                    "/v1/messages",
                    key,
                    Some(messages())
                )
                .await
                .0,
                StatusCode::UNAUTHORIZED
            );
        }
        let (_, t) = token(&app, replacement, "agent-alpha").await;
        let replacement_token = t["access_token"].as_str().unwrap();
        let project = json!({"project_id":"agent-alpha","display_name":"disabled","enabled":false});
        assert_eq!(
            call(
                admin.clone(),
                "PUT",
                "/api/private/v1/projects/agent-alpha",
                "admin",
                Some(project)
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            call(
                inference.clone(),
                "POST",
                "/v1/messages",
                replacement,
                Some(messages())
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(call(admin.clone(),"PUT","/api/private/v1/projects/agent-alpha","admin",Some(json!({"project_id":"agent-alpha","display_name":"enabled","upstream_profile":"second-account"}))).await.0,StatusCode::OK);
        assert_eq!(
            call(
                inference.clone(),
                "POST",
                "/v1/messages",
                replacement_token,
                Some(messages())
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            call(
                inference.clone(),
                "POST",
                "/v1/messages",
                replacement,
                Some(messages())
            )
            .await
            .0,
            StatusCode::OK
        );
        app.store.flush().await;
        let reopened = Store::open(&dir.path().join("db.sqlite")).unwrap();
        let mut restored = app.clone();
        restored.store = reopened;
        assert_eq!(
            call(
                crate::inference(restored.clone()),
                "POST",
                "/v1/messages",
                replacement,
                Some(messages())
            )
            .await
            .0,
            StatusCode::OK
        );
        let raw = app
            .store
            .read(|c| {
                Ok(json!(c.query_row(
                    "SELECT group_concat(payload) FROM project_credentials",
                    [],
                    |r| r.get::<_, String>(0)
                )?))
            })
            .await
            .unwrap()
            .to_string();
        assert!(!raw.contains(replacement) && !raw.contains(a));
        assert_eq!(
            call(
                admin.clone(),
                "DELETE",
                "/api/private/v1/projects/agent-alpha",
                "admin",
                None
            )
            .await
            .0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(
                inference.clone(),
                "POST",
                "/v1/messages",
                replacement,
                Some(messages())
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            call(
                admin.clone(),
                "GET",
                "/api/private/v1/projects/agent-alpha",
                "admin",
                None
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        let (_, history) = call(
            admin.clone(),
            "GET",
            "/api/private/v1/projects/agent-alpha/telemetry/requests",
            "admin",
            None,
        )
        .await;
        assert!(!history["items"].as_array().unwrap().is_empty());
        assert_eq!(
            call(
                admin.clone(),
                "DELETE",
                "/api/private/v1/projects/vertex-shim-local",
                "admin",
                None
            )
            .await
            .0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(inference, "GET", "/v1/models", "legacy-secret", None)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        let mut restored = app;
        restored.store = Store::open(&dir.path().join("db.sqlite")).unwrap();
        assert_eq!(
            call(
                crate::admin(restored),
                "GET",
                "/api/private/v1/projects/vertex-shim-local",
                "admin",
                None
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
    }
    #[tokio::test]
    async fn legacy_history_is_migrated_once_and_missing_profiles_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.sqlite");
        let c = Connection::open(&path).unwrap();
        c.execute_batch("CREATE TABLE requests(sequence INTEGER PRIMARY KEY AUTOINCREMENT,request_id TEXT UNIQUE NOT NULL,payload TEXT NOT NULL);CREATE TABLE events(request_id TEXT NOT NULL,sequence INTEGER NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(request_id,sequence));").unwrap();
        c.execute("INSERT INTO requests(request_id,payload) VALUES ('legacy',?1)",[json!({"request_id":"legacy","synthetic":true,"operation":"generation","usage_status":"complete","usage":{"input_tokens":12,"output_tokens":3}}).to_string()]).unwrap();
        drop(c);
        let app = app(dir.path());
        let page = app
            .store
            .read(|c| {
                Filter {
                    project_id: Some(DEFAULT_PROJECT.into()),
                    ..Default::default()
                }
                .list(c)
            })
            .await
            .unwrap();
        assert_eq!(page["items"].as_array().unwrap().len(), 1);
        let admin = crate::admin(app.clone());
        assert_eq!(call(admin.clone(),"POST","/api/private/v1/projects","admin",Some(json!({"project_id":"no-fallback","display_name":"Isolated account","upstream_profile":"second-account"}))).await.0,StatusCode::CREATED);
        let (_, key) = call(
            admin,
            "POST",
            "/api/private/v1/projects/no-fallback/credentials",
            "admin",
            Some(grants("isolated")),
        )
        .await;
        let mut missing = app;
        missing.upstreams = Arc::new(BTreeMap::new());
        assert_eq!(
            call(
                crate::inference(missing.clone()),
                "GET",
                "/v1/models",
                key["api_key"].as_str().unwrap(),
                None
            )
            .await
            .0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        missing.store = Store::open(&path).unwrap();
        assert_eq!(
            missing
                .store
                .read(|c| Filter {
                    project_id: Some(DEFAULT_PROJECT.into()),
                    ..Default::default()
                }
                .list(c))
                .await
                .unwrap()["items"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
}
