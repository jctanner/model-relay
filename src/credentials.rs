//! Server-owned credentials. No credential value implements Debug or enters REST.
//! Native JSON schema verified in Claude Code 2.1.280; this is an
//! experimental transport profile, not a claim of supported subscription proxy use.
use crate::ApiError;
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::{
    fs::File,
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};

const MAX_STORE: u64 = 64 * 1024;
const SKEW_MS: i64 = 60_000;

#[derive(Clone)]
pub enum Credentials {
    ManagedToken(PathBuf),
    BearerFile(PathBuf),
    ClaudeOAuth(Arc<OAuth>),
}

fn auth_error(code: &'static str) -> ApiError {
    ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        code,
        "Upstream credential unavailable; check private auth status",
    )
}
fn valid_token(value: &str) -> bool {
    !value.is_empty() && value.len() <= 16 * 1024 && value.bytes().all(|b| b.is_ascii_graphic())
}
fn bounded_file(path: &Path, limit: u64) -> Result<String, ApiError> {
    let mut text = String::new();
    File::open(path)
        .map_err(|_| auth_error("upstream_auth_missing"))?
        .take(limit + 1)
        .read_to_string(&mut text)
        .map_err(|_| auth_error("upstream_auth_invalid"))?;
    if text.len() as u64 > limit {
        return Err(auth_error("upstream_auth_invalid"));
    }
    Ok(text)
}
impl Credentials {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::ManagedToken(_) => "api-token",
            Self::BearerFile(_) => "bearer-file",
            Self::ClaudeOAuth(_) => "claude-oauth",
        }
    }
    pub async fn token(&self) -> Result<String, ApiError> {
        match self {
            Self::ManagedToken(path) => {
                let path = path.clone();
                tokio::task::spawn_blocking(move || {
                    let parent = path.parent().ok_or_else(ApiError::internal)?;
                    let info = std::fs::symlink_metadata(parent)
                        .map_err(|_| auth_error("upstream_auth_missing"))?;
                    private_file(&path, info.uid())?;
                    let value: Value = serde_json::from_str(&bounded_file(&path, MAX_STORE)?)
                        .map_err(|_| auth_error("upstream_auth_invalid"))?;
                    let token = value["token"]
                        .as_str()
                        .filter(|t| valid_token(t))
                        .ok_or_else(|| auth_error("upstream_auth_invalid"))?;
                    Ok(token.to_owned())
                })
                .await
                .map_err(|_| ApiError::internal())?
            }
            Self::BearerFile(path) => {
                let path = path.clone();
                tokio::task::spawn_blocking(move || {
                    let text = bounded_file(&path, 16 * 1024)?;
                    let token = text.trim();
                    if !valid_token(token) {
                        return Err(auth_error("upstream_auth_invalid"));
                    }
                    Ok(token.to_owned())
                })
                .await
                .map_err(|_| ApiError::internal())?
            }
            Self::ClaudeOAuth(oauth) => {
                let oauth = oauth.clone();
                tokio::spawn(async move { oauth.token().await })
                    .await
                    .map_err(|_| ApiError::internal())?
            }
        }
    }
    pub async fn status(&self) -> Value {
        match self {
            Self::BearerFile(_) | Self::ManagedToken(_) => {
                json!({"mode":self.kind(),"state":if self.token().await.is_ok() {"ready"} else {"unavailable"}})
            }
            Self::ClaudeOAuth(oauth) => oauth.status().await,
        }
    }
}

pub struct OAuth {
    directory: PathBuf,
    pub(crate) gate: tokio::sync::Mutex<()>,
    refresh: Option<Refresh>,
    last_error: std::sync::Mutex<Option<&'static str>>,
    retry_after: std::sync::Mutex<Option<std::time::Instant>>,
}
#[derive(Clone)]
pub(crate) struct Refresh {
    client: reqwest::Client,
    endpoint: String,
}
impl Refresh {
    pub(crate) fn new(
        endpoint: String,
        synthetic: bool,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let url = reqwest::Url::parse(&endpoint)?;
        if (url.scheme() != "https" && !(synthetic && url.scheme() == "http"))
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err("Invalid OAuth token endpoint".into());
        }
        Ok(Self {
            endpoint,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
        })
    }
}
pub(crate) fn private_directory(path: &Path) -> Result<(), ApiError> {
    if !path.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .map_err(|_| ApiError::internal())?;
    }
    let info = std::fs::symlink_metadata(path).map_err(|_| ApiError::internal())?;
    if !info.is_dir() || info.mode() & 0o077 != 0 {
        return Err(auth_error("upstream_auth_permissions"));
    }
    Ok(())
}
pub(crate) fn atomic_json(path: &Path, value: &Value) -> Result<(), ApiError> {
    let temporary = path.with_file_name(format!(".credential-{}", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|_| ApiError::internal())?;
        let document = value.to_string();
        if document.len() as u64 > MAX_STORE {
            return Err(auth_error("upstream_auth_invalid"));
        }
        file.write_all(document.as_bytes())
            .map_err(|_| ApiError::internal())?;
        file.sync_all().map_err(|_| ApiError::internal())?;
        std::fs::rename(&temporary, path).map_err(|_| ApiError::internal())?;
        File::open(path.parent().ok_or_else(ApiError::internal)?)
            .and_then(|dir| dir.sync_all())
            .map_err(|_| ApiError::internal())
    })();
    let _ = std::fs::remove_file(temporary);
    result
}
struct Token {
    access: String,
    expires_ms: i64,
}

fn private_file(path: &Path, uid: u32) -> Result<(), ApiError> {
    let info = std::fs::symlink_metadata(path).map_err(|_| auth_error("upstream_auth_missing"))?;
    if !info.is_file() || info.uid() != uid || info.mode() & 0o077 != 0 {
        return Err(auth_error("upstream_auth_permissions"));
    }
    Ok(())
}
fn load(directory: &Path) -> Result<Token, ApiError> {
    let info =
        std::fs::symlink_metadata(directory).map_err(|_| auth_error("upstream_auth_missing"))?;
    if !info.is_dir() || info.mode() & 0o077 != 0 {
        return Err(auth_error("upstream_auth_permissions"));
    }
    let path = directory.join(".credentials.json");
    private_file(&path, info.uid())?;
    let root: Value = serde_json::from_str(&bounded_file(&path, MAX_STORE)?)
        .map_err(|_| auth_error("upstream_auth_invalid"))?;
    parse_root(&root)
}
fn parse_root(root: &Value) -> Result<Token, ApiError> {
    let native = &root["claudeAiOauth"];
    let access = native["accessToken"]
        .as_str()
        .filter(|t| valid_token(t))
        .ok_or_else(|| auth_error("upstream_auth_invalid"))?
        .to_owned();
    let expires_ms = native["expiresAt"]
        .as_i64()
        .filter(|v| *v > 0)
        .ok_or_else(|| auth_error("upstream_auth_invalid"))?;
    let scopes: Vec<String> = native["scopes"]
        .as_array()
        .ok_or_else(|| auth_error("upstream_auth_scope"))?
        .iter()
        .map(|v| {
            v.as_str()
                .filter(|s| {
                    !s.is_empty() && s.len() <= 200 && s.bytes().all(|b| b.is_ascii_graphic())
                })
                .map(str::to_owned)
                .ok_or_else(|| auth_error("upstream_auth_scope"))
        })
        .collect::<Result<_, _>>()?;
    if !scopes.iter().any(|s| s == "user:inference") {
        return Err(auth_error("upstream_auth_scope"));
    }
    Ok(Token { access, expires_ms })
}

impl OAuth {
    pub fn new(directory: PathBuf) -> Result<Arc<Self>, Box<dyn std::error::Error>> {
        let info = std::fs::symlink_metadata(&directory)?;
        if !info.is_dir() || info.mode() & 0o077 != 0 {
            return Err("OAuth mirror directory must have owner-only permissions".into());
        }
        Ok(Arc::new(Self {
            directory,
            gate: tokio::sync::Mutex::new(()),
            refresh: None,
            last_error: std::sync::Mutex::new(None),
            retry_after: std::sync::Mutex::new(None),
        }))
    }
    pub(crate) fn managed(directory: PathBuf, refresh: Refresh) -> Result<Arc<Self>, ApiError> {
        private_directory(&directory)?;
        Ok(Arc::new(Self {
            directory,
            gate: tokio::sync::Mutex::new(()),
            refresh: Some(refresh),
            last_error: std::sync::Mutex::new(None),
            retry_after: std::sync::Mutex::new(None),
        }))
    }
    pub(crate) fn install_locked(&self, source: &Path) -> Result<(), ApiError> {
        let token = load(source)?;
        if token.expires_ms <= chrono::Utc::now().timestamp_millis() + SKEW_MS {
            return Err(auth_error("upstream_auth_expired"));
        }
        let root: Value =
            serde_json::from_str(&bounded_file(&source.join(".credentials.json"), MAX_STORE)?)
                .map_err(|_| auth_error("upstream_auth_invalid"))?;
        if !root["claudeAiOauth"]["refreshToken"]
            .as_str()
            .is_some_and(valid_token)
        {
            return Err(auth_error("upstream_auth_invalid"));
        }
        atomic_json(&self.directory.join(".credentials.json"), &root)?;
        *self.last_error.lock().map_err(|_| ApiError::internal())? = None;
        *self.retry_after.lock().map_err(|_| ApiError::internal())? = None;
        Ok(())
    }
    pub(crate) async fn disconnect(&self) -> Result<(), ApiError> {
        let _guard = self.gate.lock().await;
        self.disconnect_locked()
    }
    pub(crate) fn disconnect_locked(&self) -> Result<(), ApiError> {
        match std::fs::remove_file(self.directory.join(".credentials.json")) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(_) => return Err(ApiError::internal()),
        }
        *self.last_error.lock().map_err(|_| ApiError::internal())? = None;
        Ok(())
    }
    pub(crate) async fn renew(&self, force: bool) -> Result<(), ApiError> {
        let _guard = self.gate.lock().await;
        let token = self.load().await?;
        if !force && token.expires_ms > chrono::Utc::now().timestamp_millis() + 120_000 {
            return Ok(());
        }
        let settings = self
            .refresh
            .as_ref()
            .ok_or_else(|| auth_error("upstream_auth_expired"))?;
        if !force
            && self
                .retry_after
                .lock()
                .map_err(|_| ApiError::internal())?
                .is_some_and(|until| until > std::time::Instant::now())
        {
            return Err(auth_error("upstream_auth_refresh_failed"));
        }
        let result = self.refresh_locked(settings).await;
        *self.last_error.lock().map_err(|_| ApiError::internal())? =
            result.as_ref().err().map(|e| e.1);
        *self.retry_after.lock().map_err(|_| ApiError::internal())? = result
            .as_ref()
            .err()
            .map(|_| std::time::Instant::now() + std::time::Duration::from_secs(60));
        result
    }
    async fn refresh_locked(&self, settings: &Refresh) -> Result<(), ApiError> {
        let path = self.directory.join(".credentials.json");
        let mut root: Value = serde_json::from_str(&bounded_file(&path, MAX_STORE)?)
            .map_err(|_| auth_error("upstream_auth_invalid"))?;
        let native = &root["claudeAiOauth"];
        let refresh = native["refreshToken"]
            .as_str()
            .filter(|s| valid_token(s))
            .ok_or_else(|| auth_error("upstream_auth_invalid"))?;
        let scopes = native["scopes"]
            .as_array()
            .ok_or_else(|| auth_error("upstream_auth_scope"))?
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" ");
        // Claude Code 2.1.293's subscription OAuth refresh transport.
        let mut response = settings.client.post(&settings.endpoint).json(&json!({"grant_type":"refresh_token","refresh_token":refresh,"client_id":"9d1c250a-e61b-44d9-88ed-5944d1962f5e","scope":scopes})).send().await.map_err(|_| auth_error("upstream_auth_refresh_failed"))?;
        if response.status() == reqwest::StatusCode::BAD_REQUEST
            || response.status() == reqwest::StatusCode::UNAUTHORIZED
        {
            return Err(auth_error("upstream_auth_reauthentication_required"));
        }
        if !response.status().is_success() {
            return Err(auth_error("upstream_auth_refresh_failed"));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| auth_error("upstream_auth_refresh_failed"))?
        {
            if bytes.len() + chunk.len() > MAX_STORE as usize {
                return Err(auth_error("upstream_auth_invalid"));
            }
            bytes.extend_from_slice(&chunk);
        }
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| auth_error("upstream_auth_invalid"))?;
        let access = value["access_token"]
            .as_str()
            .filter(|s| valid_token(s))
            .ok_or_else(|| auth_error("upstream_auth_invalid"))?;
        let seconds = value["expires_in"]
            .as_i64()
            .filter(|n| *n > 60 && *n <= 366 * 86400)
            .ok_or_else(|| auth_error("upstream_auth_invalid"))?;
        root["claudeAiOauth"]["accessToken"] = json!(access);
        root["claudeAiOauth"]["expiresAt"] =
            json!(chrono::Utc::now().timestamp_millis() + seconds * 1000);
        if let Some(refresh) = value["refresh_token"].as_str() {
            if !valid_token(refresh) {
                return Err(auth_error("upstream_auth_invalid"));
            }
            root["claudeAiOauth"]["refreshToken"] = json!(refresh);
        }
        if let Some(scopes) = value["scope"].as_str() {
            let scopes: Vec<_> = scopes.split_whitespace().collect();
            if !scopes.contains(&"user:inference") {
                return Err(auth_error("upstream_auth_scope"));
            }
            root["claudeAiOauth"]["scopes"] = json!(scopes);
        }
        parse_root(&root)?;
        atomic_json(&path, &root)?;
        Ok(())
    }
    async fn load(&self) -> Result<Token, ApiError> {
        let directory = self.directory.clone();
        tokio::task::spawn_blocking(move || load(&directory))
            .await
            .map_err(|_| ApiError::internal())?
    }
    async fn token(&self) -> Result<String, ApiError> {
        if self.refresh.is_some() {
            self.renew(false).await?;
        }
        let token = self.load().await?;
        if token.expires_ms
            <= chrono::Utc::now()
                .timestamp_millis()
                .saturating_add(SKEW_MS)
        {
            return Err(auth_error("upstream_auth_expired"));
        }
        Ok(token.access)
    }
    pub(crate) async fn status(&self) -> Value {
        let mut status = match self.load().await {
            Ok(token) => {
                json!({"mode":"claude-oauth","managed":false,"refresh_owner":"host","state":if token.expires_ms > chrono::Utc::now().timestamp_millis().saturating_add(SKEW_MS) { "ready" } else { "needs_login" },"expires_at":chrono::DateTime::from_timestamp_millis(token.expires_ms).map(|t| t.to_rfc3339())})
            }
            Err(_) => {
                json!({"mode":"claude-oauth","managed":false,"refresh_owner":"host","state":"needs_login"})
            }
        };
        if self.refresh.is_some() {
            status["managed"] = json!(true);
            status["refresh_owner"] = json!("relay");
            let error = self.last_error.lock().ok().and_then(|error| *error);
            status["last_error"] = json!(error);
            if error == Some("upstream_auth_reauthentication_required") {
                status["state"] = json!("needs_login");
            }
        }
        status
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn store(dir: &Path, expires: i64) {
        std::fs::write(dir.join(".credentials.json"), json!({"claudeAiOauth":{"accessToken":"synthetic-access","refreshToken":"synthetic-refresh","expiresAt":expires,"scopes":["user:inference"]}}).to_string()).unwrap();
        std::fs::set_permissions(
            dir.join(".credentials.json"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
    #[tokio::test]
    async fn read_only_expiry_permissions_and_atomic_replacement() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        store(dir.path(), chrono::Utc::now().timestamp_millis() + 120_000);
        let oauth = OAuth::new(dir.path().into()).unwrap();
        assert_eq!(oauth.token().await.unwrap(), "synthetic-access");
        store(dir.path(), 1);
        assert_eq!(oauth.token().await.unwrap_err().1, "upstream_auth_expired");
        store(dir.path(), chrono::Utc::now().timestamp_millis() + 120_000);
        std::fs::set_permissions(
            dir.path().join(".credentials.json"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert_eq!(
            oauth.token().await.unwrap_err().1,
            "upstream_auth_permissions"
        );
        let path = dir.path().join(".credentials.json");
        let replacement = dir.path().join("replacement.json");
        std::fs::rename(&path, &replacement).unwrap();
        std::os::unix::fs::symlink(&replacement, &path).unwrap();
        assert_eq!(
            oauth.token().await.unwrap_err().1,
            "upstream_auth_permissions"
        );
    }
    #[tokio::test]
    async fn managed_refresh_serializes_rotations_and_disconnect() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let router=axum::Router::new().route("/token", axum::routing::post(move |axum::Json(body): axum::Json<Value>| { let observed=observed.clone(); async move {
            assert_eq!(body["grant_type"],"refresh_token");
            assert_eq!(body["client_id"],"9d1c250a-e61b-44d9-88ed-5944d1962f5e");
            let index=observed.fetch_add(1,Ordering::SeqCst);
            if index==0 { assert_eq!(body["refresh_token"],"synthetic-refresh"); }
            else { assert_eq!(body["refresh_token"],"rotated-refresh"); }
            axum::Json(json!({"access_token":"rotated-access","refresh_token":"rotated-refresh","expires_in":3600,"scope":"user:inference"}))
        }}));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let oauth =
            OAuth::managed(dir.path().into(), Refresh::new(endpoint, true).unwrap()).unwrap();
        store(dir.path(), 1);
        let (a, b, c) = tokio::join!(oauth.token(), oauth.token(), oauth.token());
        assert_eq!(a.unwrap(), "rotated-access");
        assert!(b.is_ok() && c.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        oauth.renew(true).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let stored = std::fs::read_to_string(dir.path().join(".credentials.json")).unwrap();
        assert!(stored.contains("rotated-refresh"));
        let status = oauth.status().await;
        assert_eq!(status["refresh_owner"], "relay");
        assert!(!status.to_string().contains("rotated-"));
        oauth.disconnect().await.unwrap();
        assert_eq!(oauth.token().await.unwrap_err().1, "upstream_auth_missing");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        server.abort();
    }
    #[tokio::test]
    async fn rejected_refresh_preserves_store_and_reports_reauthentication() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route(
                    "/token",
                    axum::routing::post(|| async { StatusCode::UNAUTHORIZED }),
                ),
            )
            .await
            .unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let oauth =
            OAuth::managed(dir.path().into(), Refresh::new(endpoint, true).unwrap()).unwrap();
        store(dir.path(), 1);
        let original = std::fs::read(dir.path().join(".credentials.json")).unwrap();
        assert_eq!(
            oauth.token().await.unwrap_err().1,
            "upstream_auth_reauthentication_required"
        );
        assert_eq!(
            std::fs::read(dir.path().join(".credentials.json")).unwrap(),
            original
        );
        assert_eq!(oauth.status().await["state"], "needs_login");
        assert_eq!(
            oauth.status().await["last_error"],
            "upstream_auth_reauthentication_required"
        );
        server.abort();
    }
}
