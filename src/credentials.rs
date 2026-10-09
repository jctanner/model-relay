//! Server-owned credentials. No credential value implements Debug or enters REST.
//! Native JSON schema verified in Claude Code 2.1.280; this is an
//! experimental transport profile, not a claim of supported subscription proxy use.
use crate::ApiError;
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::{
    fs::File,
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::Arc,
};

const MAX_STORE: u64 = 64 * 1024;
const SKEW_MS: i64 = 60_000;

#[derive(Clone)]
pub enum Credentials {
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
            Self::BearerFile(_) => "bearer-file",
            Self::ClaudeOAuth(_) => "claude-oauth",
        }
    }
    pub async fn token(&self) -> Result<String, ApiError> {
        match self {
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
            Self::BearerFile(_) => {
                json!({"mode":"bearer-file","state":if self.token().await.is_ok() {"ready"} else {"unavailable"}})
            }
            Self::ClaudeOAuth(oauth) => oauth.status().await,
        }
    }
}

pub struct OAuth {
    directory: PathBuf,
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
        Ok(Arc::new(Self { directory }))
    }
    async fn load(&self) -> Result<Token, ApiError> {
        let directory = self.directory.clone();
        tokio::task::spawn_blocking(move || load(&directory))
            .await
            .map_err(|_| ApiError::internal())?
    }
    async fn token(&self) -> Result<String, ApiError> {
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
    async fn status(&self) -> Value {
        match self.load().await {
            Ok(token) => {
                json!({"mode":"claude-oauth","managed":false,"refresh_owner":"host","state":if token.expires_ms > chrono::Utc::now().timestamp_millis().saturating_add(SKEW_MS) { "ready" } else { "needs_login" },"expires_at":chrono::DateTime::from_timestamp_millis(token.expires_ms).map(|t| t.to_rfc3339())})
            }
            Err(_) => {
                json!({"mode":"claude-oauth","managed":false,"refresh_owner":"host","state":"needs_login"})
            }
        }
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
}
