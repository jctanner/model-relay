//! Managed upstream accounts. Native login output and credentials never enter logs.
use crate::{
    ApiError, App,
    backend::{Backend, Upstream},
    credentials::{OAuth, Refresh, private_directory},
    telemetry::Store,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::File,
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::{Mutex, Notify, Semaphore, mpsc},
};

pub const CLAUDE_VERSION: &str = "2.1.293";

fn anthropic() -> String {
    "anthropic".into()
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountInfo {
    #[serde(default = "anthropic")]
    provider: String,
    account_id: String,
    display_name: String,
}
struct Account {
    info: RwLock<AccountInfo>,
    directory: PathBuf,
    oauth: Arc<OAuth>,
    backend: Backend,
    generation: AtomicU64,
    flow: Mutex<Option<Arc<Flow>>>,
}
#[derive(Clone, Serialize)]
struct FlowView {
    flow_id: String,
    state: &'static str,
    expires_at: String,
    authorization_url: Option<String>,
    error: Option<&'static str>,
}
impl FlowView {
    fn active(&self) -> bool {
        matches!(self.state, "starting" | "awaiting_code" | "exchanging")
    }
}
enum Control {
    Code(String),
    Cancel,
}
struct Flow {
    view: Mutex<FlowView>,
    control: mpsc::Sender<Control>,
    done: Notify,
    cancelled: AtomicBool,
}

pub struct Manager {
    store: Store,
    root: PathBuf,
    binary: PathBuf,
    refresh: Refresh,
    base_url: String,
    synthetic: bool,
    accounts: RwLock<BTreeMap<String, Arc<Account>>>,
    operations: Mutex<()>,
    login_slots: Arc<Semaphore>,
    login_lifetime: Duration,
    helper_available: bool,
    // One process owns credentials on this volume, including across rolling updates.
    _owner: File,
}
fn missing() -> ApiError {
    ApiError(
        StatusCode::NOT_FOUND,
        "not_found_error",
        "Unknown upstream account or login flow",
    )
}
fn conflict(message: &'static str) -> ApiError {
    ApiError(StatusCode::CONFLICT, "conflict_error", message)
}
fn sql<T>(result: rusqlite::Result<T>) -> Result<T, ApiError> {
    result.map_err(|_| ApiError::internal())
}

impl Manager {
    pub async fn open(
        store: Store,
        root: PathBuf,
        binary: PathBuf,
        base_url: String,
        token_url: String,
        synthetic: bool,
    ) -> Result<Arc<Self>, Box<dyn std::error::Error>> {
        if !root.is_absolute() || !binary.is_absolute() {
            return Err("Managed account paths must be absolute".into());
        }
        private_directory(&root).map_err(|_| "Invalid managed credential directory")?;
        let owner = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(root.join(".owner.lock"))?;
        owner
            .try_lock()
            .map_err(|_| "Another relay owns the managed credential directory")?;
        let refresh = Refresh::new(token_url, synthetic)?;
        let version = tokio::time::timeout(
            Duration::from_secs(5),
            Command::new(&binary)
                .arg("--version")
                .env_clear()
                .env("PATH", "/usr/local/bin:/usr/bin:/bin")
                .env("HOME", &root)
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await;
        let helper_available = version.ok().and_then(Result::ok).is_some_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).trim()
                    == format!("{CLAUDE_VERSION} (Claude Code)")
        });
        let manager = Arc::new(Self {
            store,
            root,
            binary,
            refresh,
            base_url,
            synthetic,
            accounts: RwLock::new(BTreeMap::new()),
            operations: Mutex::new(()),
            login_slots: Arc::new(Semaphore::new(4)),
            login_lifetime: Duration::from_secs(600),
            helper_available,
            _owner: owner,
        });
        let records: Vec<AccountInfo> = manager
            .store
            .control(false, |c| {
                let mut statement =
                    sql(c.prepare("SELECT payload FROM upstream_accounts ORDER BY account_id"))?;
                let rows = sql(statement.query_map([], |r| r.get::<_, String>(0)))?;
                rows.map(|row| serde_json::from_str(&sql(row)?).map_err(|_| ApiError::internal()))
                    .collect()
            })
            .await
            .map_err(|_| "Unable to load upstream accounts")?;
        for record in records {
            if !crate::projects::valid_id(&record.account_id) || record.account_id == "default" {
                return Err("Invalid stored account ID".into());
            }
            let account = manager
                .build(record)
                .map_err(|_| "Unable to initialize upstream account")?;
            // Login processes do not survive a proxy restart. Remove their temporary stores.
            let staging = account.directory.join("logins");
            if staging.exists() {
                std::fs::remove_dir_all(&staging)?;
            }
            manager
                .accounts
                .write()
                .map_err(|_| "Account lock unavailable")?
                .insert(
                    account
                        .info
                        .read()
                        .map_err(|_| "Account lock unavailable")?
                        .account_id
                        .clone(),
                    account.clone(),
                );
        }
        Ok(manager)
    }
    fn build(&self, info: AccountInfo) -> Result<Arc<Account>, ApiError> {
        let directory = self.root.join(&info.account_id);
        private_directory(&directory)?;
        let oauth = OAuth::managed(directory.join("credentials"), self.refresh.clone())?;
        let backend = if info.provider == "enmaas" {
            let mut upstream = Upstream::new(
                "https://api.enmaas.devshift.net",
                PathBuf::new(),
                false,
                None,
                crate::backend::upstream_timeout().map_err(|_| ApiError::internal())?,
            )
            .map_err(|_| ApiError::internal())?
            .with_enmaas();
            upstream.credentials =
                crate::credentials::Credentials::ManagedToken(directory.join("api-token.json"));
            Backend::Anthropic(upstream)
        } else if info.provider == "vertex" {
            Backend::Anthropic(
                Upstream::new(
                    "https://aiplatform.googleapis.com",
                    PathBuf::new(),
                    false,
                    None,
                    crate::backend::upstream_timeout().map_err(|_| ApiError::internal())?,
                )
                .map_err(|_| ApiError::internal())?
                .with_vertex(Arc::new(crate::vertex::Vertex::new(directory.clone())?)),
            )
        } else {
            Backend::Anthropic(
                Upstream::new(
                    &self.base_url,
                    PathBuf::new(),
                    self.synthetic,
                    None,
                    crate::backend::upstream_timeout().map_err(|_| ApiError::internal())?,
                )
                .map_err(|_| ApiError::internal())?
                .with_oauth(oauth.clone()),
            )
        };
        Ok(Arc::new(Account {
            info: RwLock::new(info),
            directory,
            oauth,
            backend,
            generation: AtomicU64::new(0),
            flow: Mutex::new(None),
        }))
    }
    fn account(&self, id: &str) -> Result<Arc<Account>, ApiError> {
        self.accounts
            .read()
            .map_err(|_| ApiError::internal())?
            .get(id)
            .cloned()
            .ok_or_else(missing)
    }
    pub fn contains_profile(&self, id: &str) -> bool {
        self.contains(id)
    }
    pub(crate) fn contains(&self, id: &str) -> bool {
        self.accounts
            .read()
            .is_ok_and(|accounts| accounts.contains_key(id))
    }
    pub(crate) fn backend(&self, id: &str) -> Option<Backend> {
        self.account(id).ok().map(|account| account.backend.clone())
    }
    pub(crate) fn profiles(&self) -> Vec<Value> {
        self.accounts.read().map(|accounts| accounts.iter().map(|(id, account)| json!({"profile_id":id,"backend":"anthropic","provider":account.info.read().map(|i| i.provider.clone()).unwrap_or_default(),"auth_mode":match &account.backend { Backend::Anthropic(u) => u.credentials.kind(), Backend::Mock => "none" },"synthetic":account.backend.synthetic(),"managed":true})).collect()).unwrap_or_default()
    }
    fn snapshot(&self) -> Result<Vec<Arc<Account>>, ApiError> {
        Ok(self
            .accounts
            .read()
            .map_err(|_| ApiError::internal())?
            .values()
            .cloned()
            .collect())
    }
    async fn status(account: &Account) -> Result<Value, ApiError> {
        let info = account
            .info
            .read()
            .map_err(|_| ApiError::internal())?
            .clone();
        let auth = match &account.backend {
            Backend::Anthropic(u) => u.credentials.status().await,
            Backend::Mock => Value::Null,
        };
        let flow = account.flow.lock().await.clone();
        let login = match flow {
            Some(flow) => {
                let view = flow.view.lock().await;
                json!({"flow_id":view.flow_id,"state":view.state,"expires_at":view.expires_at})
            }
            None => Value::Null,
        };
        Ok(
            json!({"account_id":info.account_id,"profile_id":info.account_id,"display_name":info.display_name,"provider":info.provider,"managed":true,"upstream_auth":auth,"login":login}),
        )
    }
    async fn flow(&self, account: &str, id: &str) -> Result<Arc<Flow>, ApiError> {
        let account = self.account(account)?;
        let flow = account.flow.lock().await.clone().ok_or_else(missing)?;
        if flow.view.lock().await.flow_id != id {
            return Err(missing());
        }
        Ok(flow)
    }
    async fn begin(self: &Arc<Self>, id: &str) -> Result<Value, ApiError> {
        if self
            .account(id)?
            .info
            .read()
            .map_err(|_| ApiError::internal())?
            .provider
            != "anthropic"
        {
            return Err(ApiError::bad(
                "This account uses an API token, not browser login",
            ));
        }
        if !self.helper_available {
            return Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "login_helper_unavailable",
                "Claude login service is unavailable",
            ));
        }
        let account = self.account(id)?;
        let mut current = account.flow.lock().await;
        if let Some(flow) = current.as_ref()
            && flow.view.lock().await.active()
        {
            return Err(conflict("An account login is already in progress"));
        }
        let permit = self
            .login_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| ApiError::busy())?;
        let id = uuid::Uuid::new_v4().to_string();
        let home = account.directory.join("logins").join(&id);
        private_directory(&home)?;
        let config = home.join(".claude");
        private_directory(&config)?;
        let generation = account.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let mut command = Command::new(&self.binary);
        command
            .args(["auth", "login", "--claudeai"])
            .env_clear()
            .env("HOME", &home)
            .env("CLAUDE_CONFIG_DIR", &config)
            .env("CLAUDE_SECURESTORAGE_CONFIG_DIR", &config)
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("BROWSER", "/bin/false")
            .env("DISABLE_AUTOUPDATER", "1")
            .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
            .env("DISABLE_TELEMETRY", "1")
            .env("DISABLE_ERROR_REPORTING", "1")
            .current_dir(&home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(_) => {
                let _ = std::fs::remove_dir_all(home);
                return Err(ApiError(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "login_helper_unavailable",
                    "Could not start Claude login",
                ));
            }
        };
        let (sender, receiver) = mpsc::channel(2);
        let expires = chrono::Utc::now()
            + chrono::Duration::from_std(self.login_lifetime).map_err(|_| ApiError::internal())?;
        let flow = Arc::new(Flow {
            view: Mutex::new(FlowView {
                flow_id: id,
                state: "starting",
                expires_at: expires.to_rfc3339(),
                authorization_url: None,
                error: None,
            }),
            control: sender,
            done: Notify::new(),
            cancelled: AtomicBool::new(false),
        });
        let value = json!(*flow.view.lock().await);
        *current = Some(flow.clone());
        drop(current);
        let duration = self.login_lifetime;
        tokio::spawn(async move {
            let _permit = permit;
            run_login(
                child, receiver, &flow, &account, generation, &config, duration,
            )
            .await;
            let _ = std::fs::remove_dir_all(home);
            flow.done.notify_one();
        });
        Ok(value)
    }
    async fn disconnect(&self, id: &str) -> Result<(), ApiError> {
        let account = self.account(id)?;
        account.generation.fetch_add(1, Ordering::SeqCst);
        if let Some(flow) = account.flow.lock().await.clone() {
            let _ = flow.control.try_send(Control::Cancel);
        }
        remove_api_token(&account)?;
        if let Backend::Anthropic(u) = &account.backend
            && let crate::credentials::Credentials::Vertex(vertex) = &u.credentials
        {
            vertex.disconnect().await?;
        }
        account.oauth.disconnect().await
    }
    pub async fn background(self: Arc<Self>, mut stop: tokio::sync::watch::Receiver<bool>) {
        loop {
            if *stop.borrow() {
                break;
            }
            tokio::select! { _ = stop.changed() => break, _ = tokio::time::sleep(Duration::from_secs(30)) => {} }
            if let Ok(accounts) = self.snapshot() {
                for account in accounts {
                    if *stop.borrow() {
                        break;
                    }
                    if matches!(&account.backend, Backend::Anthropic(u) if u.provider == "anthropic")
                    {
                        let _ = account.oauth.renew(false).await;
                    }
                }
            }
        }
        if let Ok(accounts) = self.snapshot() {
            for account in accounts {
                if let Some(flow) = account.flow.lock().await.clone()
                    && flow.view.lock().await.active()
                {
                    account.generation.fetch_add(1, Ordering::SeqCst);
                    let _ = flow.control.try_send(Control::Cancel);
                    let _ =
                        tokio::time::timeout(Duration::from_secs(5), flow.done.notified()).await;
                }
            }
        }
    }
}

// Keep only a small buffer, never expose subprocess output, and accept provider URLs only.
fn authorization_url(output: &str) -> Option<String> {
    for start in output.match_indices("https://").map(|(start, _)| start) {
        let suffix = &output[start..];
        let Some(end) = suffix.find(|c: char| c.is_whitespace() || c.is_control()) else {
            continue;
        };
        let candidate = &suffix[..end];
        let Ok(url) = reqwest::Url::parse(candidate) else {
            continue;
        };
        if !matches!(
            (url.host_str()?, url.path()),
            ("claude.com", "/cai/oauth/authorize") | ("claude.ai", "/oauth/authorize")
        ) || !url.username().is_empty()
            || url.password().is_some()
            || url.port().is_some()
            || url.fragment().is_some()
        {
            continue;
        }
        let fields: BTreeMap<_, _> = url.query_pairs().collect();
        if fields
            .get("response_type")
            .is_some_and(|value| value == "code")
            && fields.get("state").is_some_and(|value| !value.is_empty())
            && fields
                .get("code_challenge_method")
                .is_some_and(|value| value == "S256")
            && fields
                .get("code_challenge")
                .is_some_and(|value| !value.is_empty())
        {
            return Some(candidate.to_owned());
        }
    }
    None
}
async fn read_output<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    sender: mpsc::Sender<Vec<u8>>,
) {
    let mut bytes = [0u8; 4096];
    loop {
        match reader.read(&mut bytes).await {
            Ok(0) | Err(_) => break,
            Ok(size) => {
                if sender.send(bytes[..size].to_vec()).await.is_err() {
                    break;
                }
            }
        }
    }
}
async fn run_login(
    mut child: tokio::process::Child,
    mut controls: mpsc::Receiver<Control>,
    flow: &Flow,
    account: &Account,
    generation: u64,
    config: &std::path::Path,
    lifetime: Duration,
) {
    let mut input = child.stdin.take();
    let (output, mut chunks) = mpsc::channel(8);
    let stdout = tokio::spawn(read_output(child.stdout.take().unwrap(), output.clone()));
    let stderr = tokio::spawn(read_output(child.stderr.take().unwrap(), output));
    let deadline = tokio::time::sleep(lifetime);
    tokio::pin!(deadline);
    let mut buffer = String::new();
    let mut output_open = true;
    let (state, error) = loop {
        tokio::select! {
            _ = &mut deadline => { let _ = child.kill().await; break ("expired", Some("Login expired; start a new connection")); }
            command = controls.recv() => match command {
                Some(Control::Cancel) | None => { let _ = child.kill().await; break ("cancelled", None); }
                Some(Control::Code(code)) => {
                    if let Some(input) = input.as_mut() && input.write_all(format!("{code}\n").as_bytes()).await.is_err() { let _ = child.kill().await; break ("failed", Some("Could not submit authorization code")); }
                }
            },
            chunk = chunks.recv(), if output_open => if let Some(chunk) = chunk {
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                if let Some(url) = authorization_url(&buffer) { let mut view = flow.view.lock().await; if view.state == "starting" { view.authorization_url = Some(url); view.state = "awaiting_code"; } }
                if buffer.len() > 32 * 1024 { let mut cut = buffer.len() - 16 * 1024; while !buffer.is_char_boundary(cut) { cut += 1; } buffer.drain(..cut); }
            } else { output_open = false; },
            result = child.wait() => {
                if !result.is_ok_and(|status| status.success()) { break ("failed", Some("Claude login failed; start a new connection")); }
                let _guard = account.oauth.gate.lock().await;
                if flow.cancelled.load(Ordering::SeqCst) || account.generation.load(Ordering::SeqCst) != generation { break ("cancelled", None); }
                if account.oauth.install_locked(config).is_err() { break ("failed", Some("Login completed without usable subscription credentials")); }
                break ("connected", None);
            }
        }
    };
    stdout.abort();
    stderr.abort();
    let mut view = flow.view.lock().await;
    view.state = state;
    view.error = error;
    view.authorization_url = None;
}

pub fn routes() -> Router<App> {
    Router::new()
        .route(
            "/api/private/v1/upstream-accounts/{account}/token",
            axum::routing::put(set_token),
        )
        .route(
            "/api/private/v1/upstream-accounts/{account}/vertex",
            axum::routing::put(set_vertex),
        )
        .route("/api/private/v1/upstream-accounts", get(list).post(create))
        .route(
            "/api/private/v1/upstream-accounts/{account}",
            get(detail).put(update).delete(remove),
        )
        .route(
            "/api/private/v1/upstream-accounts/{account}/login",
            post(login),
        )
        .route(
            "/api/private/v1/upstream-accounts/{account}/login/{flow}",
            get(login_status).delete(cancel),
        )
        .route(
            "/api/private/v1/upstream-accounts/{account}/login/{flow}/code",
            post(code),
        )
        .route(
            "/api/private/v1/upstream-accounts/{account}/disconnect",
            post(disconnect),
        )
        .route(
            "/api/private/v1/upstream-accounts/{account}/refresh",
            post(refresh),
        )
}
fn manager(app: &App) -> Result<Arc<Manager>, ApiError> {
    app.accounts.clone().ok_or(ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "account_management_unavailable",
        "Upstream account management is unavailable",
    ))
}
async fn list(State(app): State<App>) -> Result<Json<Value>, ApiError> {
    let mut items = Vec::new();
    let mut external = vec![("default", &app.backend)];
    external.extend(app.upstreams.iter().map(|(id, b)| (id.as_str(), b)));
    for (id, backend) in external {
        if let Backend::Anthropic(upstream) = backend {
            items.push(json!({"account_id":id,"profile_id":id,"display_name":if id=="default" {"Default upstream account"} else {id},"provider":"anthropic","managed":false,"upstream_auth":upstream.credentials.status().await}));
        }
    }
    if let Some(manager) = &app.accounts {
        for account in manager.snapshot()? {
            items.push(Manager::status(&account).await?);
        }
    }
    Ok(Json(
        json!({"items":items,"management_enabled":app.accounts.is_some(),"login_available":app.accounts.as_ref().is_some_and(|manager| manager.helper_available),"claude_version":CLAUDE_VERSION}),
    ))
}
async fn create(
    State(app): State<App>,
    Json(info): Json<AccountInfo>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if !["anthropic", "enmaas", "vertex"].contains(&info.provider.as_str())
        || !crate::projects::valid_id(&info.account_id)
        || info.account_id == "default"
        || info.display_name.is_empty()
        || info.display_name.len() > 200
        || info.display_name.chars().any(char::is_control)
    {
        return Err(ApiError::bad("Invalid upstream account fields"));
    }
    let manager = manager(&app)?;
    let _guard = manager.operations.lock().await;
    if manager.snapshot()?.len() >= 100 {
        return Err(ApiError::bad("Too many upstream accounts"));
    }
    if app.upstreams.contains_key(&info.account_id) || manager.contains(&info.account_id) {
        return Err(conflict("Upstream account ID already exists"));
    }
    let account = manager.build(info.clone())?;
    manager
        .store
        .control(true, move |c| {
            if info.provider == "enmaas" {
                crate::model_policy::save(
                    c,
                    &info.account_id,
                    &crate::model_policy::Inventory::enmaas(),
                )?;
            } else if info.provider == "vertex" {
                crate::model_policy::save(
                    c,
                    &info.account_id,
                    &crate::model_policy::Inventory::vertex(),
                )?;
            }
            sql(c.execute(
                "INSERT INTO upstream_accounts(account_id,payload) VALUES (?1,?2)",
                rusqlite::params![info.account_id, json!(info).to_string()],
            ))?;
            Ok(())
        })
        .await?;
    let id = account
        .info
        .read()
        .map_err(|_| ApiError::internal())?
        .account_id
        .clone();
    manager
        .accounts
        .write()
        .map_err(|_| ApiError::internal())?
        .insert(id, account.clone());
    Ok((StatusCode::CREATED, Json(Manager::status(&account).await?)))
}
async fn detail(State(app): State<App>, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    let account = manager(&app)?.account(&id)?;
    Ok(Json(Manager::status(&account).await?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Update {
    display_name: String,
}
async fn update(
    State(app): State<App>,
    Path(id): Path<String>,
    Json(input): Json<Update>,
) -> Result<Json<Value>, ApiError> {
    if input.display_name.is_empty()
        || input.display_name.len() > 200
        || input.display_name.chars().any(char::is_control)
    {
        return Err(ApiError::bad("Invalid account display name"));
    }
    let manager = manager(&app)?;
    let _guard = manager.operations.lock().await;
    let account = manager.account(&id)?;
    let info = AccountInfo {
        provider: account
            .info
            .read()
            .map_err(|_| ApiError::internal())?
            .provider
            .clone(),
        account_id: id.clone(),
        display_name: input.display_name,
    };
    let document = json!(info).to_string();
    manager
        .store
        .control(true, move |c| {
            sql(c.execute(
                "UPDATE upstream_accounts SET payload=?2 WHERE account_id=?1",
                rusqlite::params![id, document],
            ))?;
            Ok(())
        })
        .await?;
    *account.info.write().map_err(|_| ApiError::internal())? = info;
    Ok(Json(Manager::status(&account).await?))
}
async fn remove(State(app): State<App>, Path(id): Path<String>) -> Result<StatusCode, ApiError> {
    let manager = manager(&app)?;
    let _guard = manager.operations.lock().await;
    let account = manager.account(&id)?;
    let _credentials_guard = account.oauth.gate.lock().await;
    let removed = account.clone();
    let account_id = id.clone();
    manager.store.control(true, move |c| {
        let referenced: bool = sql(c.query_row("SELECT EXISTS(SELECT 1 FROM projects WHERE deleted=0 AND ((json_type(payload,'$.routing') IS NULL OR json_type(payload,'$.routing')='null') AND json_extract(payload,'$.upstream_profile')=?1 OR json_extract(payload,'$.routing.default_upstream')=?1 OR EXISTS(SELECT 1 FROM json_each(payload,'$.routing.model_overrides') WHERE value=?1)))", [&account_id], |r|r.get(0)))?;
        if referenced { return Err(conflict("Reassign the account's projects before deleting it")); }
        removed.generation.fetch_add(1, Ordering::SeqCst);
        removed.oauth.disconnect_locked()?;
        remove_api_token(&removed)?;
        sql(c.execute("DELETE FROM model_inventories WHERE profile_id=?1", [&account_id]))?;
        sql(c.execute("DELETE FROM upstream_accounts WHERE account_id=?1", [&account_id]))?; Ok(())
    }).await?;
    if let Some(flow) = account.flow.lock().await.clone() {
        let _ = flow.control.try_send(Control::Cancel);
    }
    manager
        .accounts
        .write()
        .map_err(|_| ApiError::internal())?
        .remove(&id);
    if let Backend::Anthropic(u) = &account.backend
        && let crate::credentials::Credentials::Vertex(vertex) = &u.credentials
    {
        vertex.disconnect().await?;
    }
    // Keep the directory until any cancelled subprocess has exited; its credential is removed.
    Ok(StatusCode::NO_CONTENT)
}
async fn login(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let manager = manager(&app)?;
    let _guard = manager.operations.lock().await;
    Ok((StatusCode::ACCEPTED, Json(manager.begin(&id).await?)))
}
async fn login_status(
    State(app): State<App>,
    Path((id, flow)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let flow = manager(&app)?.flow(&id, &flow).await?;
    Ok(Json(json!(*flow.view.lock().await)))
}
async fn cancel(
    State(app): State<App>,
    Path((id, flow)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let manager = manager(&app)?;
    let flow = manager.flow(&id, &flow).await?;
    let account = manager.account(&id)?;
    let _guard = account.oauth.gate.lock().await;
    if flow.view.lock().await.active() {
        flow.cancelled.store(true, Ordering::SeqCst);
        // Cancellation is idempotent. A full queue already contains cancellation;
        // never await channel capacity while holding the credential publication lock.
        let _ = flow.control.try_send(Control::Cancel);
    }
    Ok(StatusCode::NO_CONTENT)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Code {
    code: String,
}
async fn code(
    State(app): State<App>,
    Path((id, flow)): Path<(String, String)>,
    Json(input): Json<Code>,
) -> Result<StatusCode, ApiError> {
    let code = input.code.trim();
    if code.is_empty() || code.len() > 4096 || !code.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(ApiError::bad("Invalid authorization code"));
    }
    let flow = manager(&app)?.flow(&id, &flow).await?;
    let mut view = flow.view.lock().await;
    if view.state != "awaiting_code" {
        return Err(conflict("Login is not waiting for a code"));
    }
    if let Some((_, state)) = code.split_once('#') {
        let url = reqwest::Url::parse(
            view.authorization_url
                .as_deref()
                .ok_or_else(ApiError::internal)?,
        )
        .map_err(|_| ApiError::internal())?;
        if !url
            .query_pairs()
            .any(|(key, value)| key == "state" && value == state)
        {
            return Err(ApiError::bad("Authorization code belongs to another login"));
        }
    }
    flow.control
        .try_send(Control::Code(code.to_owned()))
        .map_err(|_| conflict("Login already ended"))?;
    view.state = "exchanging";
    view.authorization_url = None;
    Ok(StatusCode::ACCEPTED)
}
async fn disconnect(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let manager = manager(&app)?;
    let _guard = manager.operations.lock().await;
    manager.disconnect(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn refresh(State(app): State<App>, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    let account = manager(&app)?.account(&id)?;
    if let Backend::Anthropic(u) = &account.backend
        && let crate::credentials::Credentials::Vertex(vertex) = &u.credentials
    {
        vertex.renew().await?;
        return Ok(Json(Manager::status(&account).await?));
    }
    if matches!(&account.backend, Backend::Anthropic(u) if u.provider == "enmaas") {
        return Err(ApiError::bad("Rotate API tokens using the token endpoint"));
    }
    account.oauth.renew(true).await?;
    Ok(Json(Manager::status(&account).await?))
}

fn remove_api_token(account: &Account) -> Result<(), ApiError> {
    for file in ["api-token.json", "vertex-credentials.json"] {
        match std::fs::remove_file(account.directory.join(file)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(ApiError::internal()),
        }
    }
    File::open(&account.directory)
        .and_then(|dir| dir.sync_all())
        .map_err(|_| ApiError::internal())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenInput {
    token: String,
}
async fn set_vertex(
    State(app): State<App>,
    Path(id): Path<String>,
    Json(input): Json<crate::vertex::Input>,
) -> Result<Json<Value>, ApiError> {
    let manager = manager(&app)?;
    let _guard = manager.operations.lock().await;
    let account = manager.account(&id)?;
    let Backend::Anthropic(upstream) = &account.backend else {
        return Err(ApiError::bad("This account is not a Vertex account"));
    };
    let crate::credentials::Credentials::Vertex(vertex) = &upstream.credentials else {
        return Err(ApiError::bad("This account is not a Vertex account"));
    };
    vertex.configure(input).await?;
    Ok(Json(Manager::status(&account).await?))
}
async fn set_token(
    State(app): State<App>,
    Path(id): Path<String>,
    Json(input): Json<TokenInput>,
) -> Result<Json<Value>, ApiError> {
    if input.token.is_empty()
        || input.token.len() > 16 * 1024
        || !input.token.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err(ApiError::bad("Invalid API token"));
    }
    let manager = manager(&app)?;
    let _guard = manager.operations.lock().await;
    let account = manager.account(&id)?;
    if account
        .info
        .read()
        .map_err(|_| ApiError::internal())?
        .provider
        != "enmaas"
    {
        return Err(ApiError::bad("This account uses browser login"));
    }
    crate::credentials::atomic_json(
        &account.directory.join("api-token.json"),
        &json!({"token":input.token}),
    )?;
    if let Backend::Anthropic(u) = &account.backend {
        u.clear_catalog().await;
    }
    Ok(Json(Manager::status(&account).await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use std::os::unix::fs::PermissionsExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn vertex_account_api_keeps_json_private_and_preserves_it_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let app = setup(dir.path(), Duration::from_secs(10)).await;
        let router = crate::admin(app.clone());
        let path = "/api/private/v1/upstream-accounts/vertex-test";
        let created = call(&router, "POST", "/api/private/v1/upstream-accounts", "admin",
            Some(json!({"account_id":"vertex-test","display_name":"Vertex fixture","provider":"vertex"}))).await;
        assert_eq!(created.0, StatusCode::CREATED);
        assert_eq!(created.1["upstream_auth"]["configured"], false);
        let input = json!({"project_id":"central-test-project","region":"global",
            "service_account_json":crate::vertex::tests::service_account().to_string()});
        assert_eq!(
            call(
                &router,
                "PUT",
                &format!("{path}/vertex"),
                "wrong-key",
                Some(input.clone())
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
        let saved = call(
            &router,
            "PUT",
            &format!("{path}/vertex"),
            "admin",
            Some(input),
        )
        .await;
        assert_eq!(saved.0, StatusCode::OK);
        assert_eq!(
            saved.1["upstream_auth"]["project_id"],
            "central-test-project"
        );
        assert_eq!(saved.1["upstream_auth"]["state"], "ready");
        for endpoint in [path, "/api/private/v1/upstream-accounts"] {
            let response = call(&router, "GET", endpoint, "admin", None).await;
            let text = response.1.to_string();
            assert!(!text.contains("PRIVATE KEY"));
            assert!(!text.contains("client_email"));
            assert!(!text.contains("service_account_json"));
        }
        assert_eq!(
            call(&router, "POST", &format!("{path}/login"), "admin", None)
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            call(
                &router,
                "PUT",
                &format!("{path}/token"),
                "admin",
                Some(json!({"token":"wrong-provider"}))
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        let inventory = call(
            &router,
            "GET",
            "/api/private/v1/upstream-profiles/vertex-test/models",
            "admin",
            None,
        )
        .await
        .1;
        assert_eq!(inventory["models"].as_array().unwrap().len(), 3);
        let document = app
            .store
            .control(false, |c| {
                sql(c.query_row(
                    "SELECT payload FROM upstream_accounts WHERE account_id='vertex-test'",
                    [],
                    |r| r.get::<_, String>(0),
                ))
            })
            .await
            .unwrap();
        assert!(!document.contains("PRIVATE KEY"));
        assert!(!document.contains("service_account"));
        let credential_file = dir
            .path()
            .join("accounts/vertex-test/vertex-credentials.json");
        assert_eq!(
            std::fs::metadata(&credential_file)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(router);
        drop(app);
        let reopened = setup(dir.path(), Duration::from_secs(10)).await;
        let router = crate::admin(reopened);
        let status = call(&router, "GET", path, "admin", None).await.1;
        assert_eq!(status["upstream_auth"]["state"], "ready");
        assert_eq!(
            status["upstream_auth"]["project_id"],
            "central-test-project"
        );
        let edit = call(
            &router,
            "PUT",
            &format!("{path}/vertex"),
            "admin",
            Some(json!({"project_id":"other-test-project","region":"us-east5"})),
        )
        .await;
        assert_eq!(edit.0, StatusCode::OK);
        assert_eq!(edit.1["upstream_auth"]["region"], "us-east5");
        assert_eq!(
            call(&router, "DELETE", path, "admin", None).await.0,
            StatusCode::NO_CONTENT
        );
        assert!(!credential_file.exists());
    }

    #[tokio::test]
    async fn enmaas_tokens_are_private_rotatable_persistent_and_glm_only() {
        let dir = tempfile::tempdir().unwrap();
        let app = setup(dir.path(), Duration::from_secs(10)).await;
        let router = crate::admin(app.clone());
        let path = "/api/private/v1/upstream-accounts/enmaas-test";
        let created = call(
            &router,
            "POST",
            "/api/private/v1/upstream-accounts",
            "admin",
            Some(json!({"account_id":"enmaas-test","display_name":"EnMaaS","provider":"enmaas"})),
        )
        .await;
        assert_eq!(created.0, StatusCode::CREATED);
        assert_eq!(created.1["provider"], "enmaas");
        assert_eq!(
            call(&router, "POST", &format!("{path}/login"), "admin", None)
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        for secret in ["test-private-token-a", "test-private-token-b"] {
            let result = call(
                &router,
                "PUT",
                &format!("{path}/token"),
                "admin",
                Some(json!({"token":secret})),
            )
            .await;
            assert_eq!(result.0, StatusCode::OK);
            assert_eq!(result.1["upstream_auth"]["state"], "ready");
            assert!(!result.1.to_string().contains(secret));
            let Backend::Anthropic(upstream) = app
                .accounts
                .as_ref()
                .unwrap()
                .backend("enmaas-test")
                .unwrap()
            else {
                panic!()
            };
            assert_eq!(upstream.credentials.token().await.unwrap(), secret);
        }
        let token_file = dir.path().join("accounts/enmaas-test/api-token.json");
        assert_eq!(
            std::fs::metadata(&token_file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let invpath = "/api/private/v1/upstream-profiles/enmaas-test/models";
        let mut inventory = call(&router, "GET", invpath, "admin", None).await.1;
        assert_eq!(inventory["models"][0]["id"], crate::model_policy::GLM);
        assert_eq!(inventory["models"][0]["enabled"], true);
        inventory["models"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id":"claude-opus-4-6","enabled":true,"manual":true}));
        assert_eq!(
            call(&router, "PUT", invpath, "admin", Some(inventory))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            call(
                &router,
                "PUT",
                path,
                "admin",
                Some(json!({"display_name":"Renamed"}))
            )
            .await
            .1["provider"],
            "enmaas"
        );
        drop(router);
        drop(app);
        let reopened = setup(dir.path(), Duration::from_secs(10)).await;
        let router = crate::admin(reopened.clone());
        let detail = call(&router, "GET", path, "admin", None).await.1;
        assert_eq!(detail["provider"], "enmaas");
        assert_eq!(detail["upstream_auth"]["state"], "ready");
        assert_eq!(
            call(
                &router,
                "POST",
                &format!("{path}/disconnect"),
                "admin",
                None
            )
            .await
            .0,
            StatusCode::NO_CONTENT
        );
        assert!(!token_file.exists());
        assert_eq!(
            call(&router, "DELETE", path, "admin", None).await.0,
            StatusCode::NO_CONTENT
        );
        assert!(!reopened.accounts.as_ref().unwrap().contains("enmaas-test"));
    }

    async fn setup(dir: &std::path::Path, lifetime: Duration) -> App {
        let binary = dir.join("claude");
        std::fs::write(&binary, include_str!("fixtures/fake-claude-login.sh")).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        let store = Store::open(&dir.join("relay.sqlite")).unwrap();
        let mut manager = Manager::open(
            store.clone(),
            dir.join("accounts"),
            binary,
            "http://127.0.0.1:1".into(),
            "http://127.0.0.1:1/token".into(),
            true,
        )
        .await
        .unwrap();
        Arc::get_mut(&mut manager).unwrap().login_lifetime = lifetime;
        let registry = dir.join("keys.json");
        std::fs::write(&registry, "{\"keys\":[]}").unwrap();
        App {
            routing: None,
            inventory: None,
            project_policy: Default::default(),
            accounts: Some(manager),
            backend: Backend::Mock,
            upstreams: Arc::new(BTreeMap::new()),
            store,
            registry,
            admin_verifier: crate::auth::fingerprint("admin"),
            models: Arc::new(BTreeMap::new()),
            mock_adc: None,
        }
    }
    async fn call(
        router: &Router,
        method: &str,
        path: &str,
        key: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("authorization", format!("Bearer {key}"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        if status.is_success() {
            assert_eq!(response.headers()["cache-control"], "no-store");
        }
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
    async fn await_state(router: &Router, path: &str, expected: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let (status, value) = call(router, "GET", path, "admin", None).await;
                assert_eq!(status, StatusCode::OK);
                if value["state"] == expected {
                    return value;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    }
    const ROOT: &str = "/api/private/v1/upstream-accounts";
    #[tokio::test]
    async fn managed_account_streaming_uses_configured_timeout() {
        // Isolate environment overrides from the parallel test suite, without
        // mutating process-global environment variables in an async runtime.
        let Ok(seconds) = std::env::var("SHIM_MANAGED_TIMEOUT_TEST") else {
            for seconds in ["1", "3"] {
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "accounts::tests::managed_account_streaming_uses_configured_timeout",
                        "--nocapture",
                    ])
                    .env("SHIM_MANAGED_TIMEOUT_TEST", seconds)
                    .env("SHIM_UPSTREAM_TIMEOUT_SECONDS", seconds)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "managed timeout {seconds}s failed: {} {}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr),
                );
            }
            return;
        };
        use futures_util::StreamExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let fixture = Router::new().route(
            "/v1/messages",
            post(|Json(body): Json<Value>| async move {
                let start = futures_util::stream::once(async {
                    Ok::<_, std::io::Error>(
                        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_fixture\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
                    )
                });
                let finish = futures_util::stream::once(async move {
                    if body.pointer("/metadata/user_id").and_then(Value::as_str)
                        == Some("broken")
                    {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        Err(std::io::Error::other("synthetic broken stream"))
                    } else {
                        tokio::time::sleep(Duration::from_millis(1500)).await;
                        Ok("event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n")
                    }
                });
                (
                    [("content-type", "text/event-stream")],
                    Body::from_stream(start.chain(finish)),
                )
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, fixture).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let mut app = setup(dir.path(), Duration::from_secs(600)).await;
        let manager = Arc::get_mut(app.accounts.as_mut().unwrap()).unwrap();
        manager.base_url = format!("http://{address}");
        let account = manager
            .build(AccountInfo {
                provider: anthropic(),
                account_id: "timeout-fixture".into(),
                display_name: "Timeout fixture".into(),
            })
            .unwrap();
        let Backend::Anthropic(mut upstream) = account.backend.clone() else {
            panic!("managed account must use Anthropic backend");
        };
        // Only authentication is replaced; exercise the managed constructor's
        // actual HTTP client and the production forwarding/telemetry path.
        let token = dir.path().join("token");
        std::fs::write(&token, "synthetic-fixture-token").unwrap();
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
        upstream.credentials = crate::credentials::Credentials::BearerFile(token);
        for mode in ["slow", "broken"] {
            let body = json!({"messages":[{"role":"user","content":"synthetic"}],
                "max_tokens":10,"metadata":{"user_id":mode}});
            let record = crate::telemetry::Record::new(
                "fixture-model",
                "stream_generation",
                crate::auth::Identity {
                    upstream_profile: "timeout-fixture".into(),
                    project_id: "fixture".into(),
                    client_id: "fixture".into(),
                    key_id: "fixture".into(),
                    scheme: "bearer",
                },
                &body,
                0,
                Some(mode.into()),
                None,
            );
            let response = crate::backend::forward(
                &upstream,
                crate::Lifecycle::new(app.store.clone(), record),
                body,
                false,
                true,
                None,
            )
            .await
            .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let result =
                tokio::time::timeout(Duration::from_secs(5), response.into_body().collect())
                    .await
                    .unwrap();
            assert_eq!(result.is_ok(), mode == "slow" && seconds == "3");
            app.store.flush().await;
            let records = app
                .store
                .read(|conn| crate::telemetry::Filter::default().list(conn))
                .await
                .unwrap();
            let row = records["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["run_id"] == mode)
                .unwrap();
            if mode == "broken" {
                assert_eq!(row["error_type"], "upstream_stream_error");
                assert_eq!(row["completion"], "failed");
            } else if seconds == "1" {
                assert_eq!(row["error_type"], "timeout_error");
                assert_eq!(row["completion"], "failed");
                assert_eq!(row["usage_status"], "partial");
            } else {
                assert_eq!(row["completion"], "complete");
                assert_eq!(row["usage_status"], "complete");
                assert_eq!(row["usage"]["output_tokens"], 2);
            }
        }
        server.abort();
    }
    #[tokio::test]
    async fn login_isolation_persistence_routing_and_disconnect() {
        let dir = tempfile::tempdir().unwrap();
        let app = setup(dir.path(), Duration::from_secs(600)).await;
        let router = crate::admin(app.clone());
        assert_eq!(
            call(&router, "GET", ROOT, "wrong", None).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            call(&crate::inference(app.clone()), "GET", ROOT, "admin", None)
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(
                &router,
                "POST",
                ROOT,
                "admin",
                Some(json!({"account_id":"../escape","display_name":"Unsafe"}))
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        for id in ["team-a", "team-b"] {
            assert_eq!(
                call(
                    &router,
                    "POST",
                    ROOT,
                    "admin",
                    Some(json!({"account_id":id,"display_name":id}))
                )
                .await
                .0,
                StatusCode::CREATED
            );
        }
        assert_eq!(
            call(
                &router,
                "POST",
                ROOT,
                "admin",
                Some(json!({"account_id":"team-a","display_name":"Duplicate"}))
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        let (_, a) = call(
            &router,
            "POST",
            &format!("{ROOT}/team-a/login"),
            "admin",
            None,
        )
        .await;
        let path = format!("{ROOT}/team-a/login/{}", a["flow_id"].as_str().unwrap());
        let view = await_state(&router, &path, "awaiting_code").await;
        assert!(
            view["authorization_url"]
                .as_str()
                .unwrap()
                .starts_with("https://claude.com/")
        );
        assert_eq!(
            call(
                &router,
                "POST",
                &format!("{ROOT}/team-a/login"),
                "admin",
                None
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            call(
                &router,
                "GET",
                &path.replace("team-a", "team-b"),
                "admin",
                None
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(
                &router,
                "POST",
                &(path.clone() + "/code"),
                "admin",
                Some(json!({"code":"fixture-code#other-state"}))
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        let (_, list) = call(&router, "GET", ROOT, "admin", None).await;
        assert!(!list.to_string().contains("authorization_url"));
        assert_eq!(
            call(
                &router,
                "POST",
                &(path.clone() + "/code"),
                "admin",
                Some(json!({"code":format!("fixture-code#{}",a["flow_id"].as_str().unwrap())}))
            )
            .await
            .0,
            StatusCode::ACCEPTED
        );
        let connected = await_state(&router, &path, "connected").await;
        assert!(connected["authorization_url"].is_null());
        let manager = app.accounts.as_ref().unwrap();
        let account = manager.account("team-a").unwrap();
        assert_eq!(account.oauth.status().await["state"], "ready");
        assert_eq!(
            crate::credentials::Credentials::ClaudeOAuth(account.oauth.clone())
                .token()
                .await
                .unwrap(),
            "fixture-access"
        );
        assert_eq!(
            manager.account("team-b").unwrap().oauth.status().await["state"],
            "needs_login"
        );
        let (_, profiles) = call(
            &router,
            "GET",
            "/api/private/v1/upstream-profiles",
            "admin",
            None,
        )
        .await;
        assert!(
            profiles["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["profile_id"] == "team-a")
        );
        assert_eq!(call(&router,"POST","/api/private/v1/projects","admin",Some(json!({"project_id":"agent-a","display_name":"Agent A","upstream_profile":"team-a"}))).await.0,StatusCode::CREATED);
        let (_, auth) = call(
            &router,
            "GET",
            "/api/private/v1/projects/agent-a/auth/status",
            "admin",
            None,
        )
        .await;
        assert_eq!(auth["upstream_auth"]["state"], "ready");
        assert_eq!(auth["upstream_auth"]["refresh_owner"], "relay");
        assert_eq!(
            call(&router, "DELETE", &format!("{ROOT}/team-a"), "admin", None)
                .await
                .0,
            StatusCode::CONFLICT
        );
        let serialized = call(&router, "GET", ROOT, "admin", None)
            .await
            .1
            .to_string();
        for secret in ["fixture-code", "fixture-access", "fixture-refresh"] {
            assert!(!serialized.contains(secret));
        }
        // Process ownership prevents refresh races across replicas on the same volume.
        assert!(
            Manager::open(
                app.store.clone(),
                dir.path().join("accounts"),
                dir.path().join("claude"),
                "http://127.0.0.1:1".into(),
                "http://127.0.0.1:1/token".into(),
                true
            )
            .await
            .is_err()
        );
        drop(router);
        drop(app);
        let app = setup(dir.path(), Duration::from_secs(600)).await;
        let router = crate::admin(app.clone());
        assert_eq!(
            call(&router, "GET", &format!("{ROOT}/team-a"), "admin", None)
                .await
                .1["upstream_auth"]["state"],
            "ready"
        );
        assert_eq!(
            call(
                &router,
                "POST",
                &format!("{ROOT}/team-a/disconnect"),
                "admin",
                None
            )
            .await
            .0,
            StatusCode::NO_CONTENT
        );
        assert!(
            !dir.path()
                .join("accounts/team-a/credentials/.credentials.json")
                .exists()
        );
        assert_eq!(
            call(
                &router,
                "GET",
                "/api/private/v1/projects/agent-a/auth/status",
                "admin",
                None
            )
            .await
            .1["upstream_auth"]["state"],
            "needs_login"
        );
        assert_eq!(
            call(
                &router,
                "DELETE",
                "/api/private/v1/projects/agent-a",
                "admin",
                None
            )
            .await
            .0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(&router, "DELETE", &format!("{ROOT}/team-a"), "admin", None)
                .await
                .0,
            StatusCode::NO_CONTENT
        );
        assert!(!app.accounts.as_ref().unwrap().contains("team-a"));
    }
    #[tokio::test]
    async fn cancelled_and_expired_logins_never_publish_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let app = setup(dir.path(), Duration::from_millis(500)).await;
        let router = crate::admin(app.clone());
        call(
            &router,
            "POST",
            ROOT,
            "admin",
            Some(json!({"account_id":"cancel-test","display_name":"Cancellation"})),
        )
        .await;
        for expected in ["cancelled", "expired"] {
            let (_, flow) = call(
                &router,
                "POST",
                &format!("{ROOT}/cancel-test/login"),
                "admin",
                None,
            )
            .await;
            let path = format!(
                "{ROOT}/cancel-test/login/{}",
                flow["flow_id"].as_str().unwrap()
            );
            await_state(&router, &path, "awaiting_code").await;
            if expected == "cancelled" {
                let (first, second, third) = tokio::join!(
                    call(&router, "DELETE", &path, "admin", None),
                    call(&router, "DELETE", &path, "admin", None),
                    call(&router, "DELETE", &path, "admin", None)
                );
                for result in [first, second, third] {
                    assert_eq!(result.0, StatusCode::NO_CONTENT);
                }
            }
            let view = await_state(&router, &path, expected).await;
            assert!(view["authorization_url"].is_null());
            assert_eq!(
                call(
                    &router,
                    "POST",
                    &(path + "/code"),
                    "admin",
                    Some(json!({"code":"fixture-code"}))
                )
                .await
                .0,
                StatusCode::CONFLICT
            );
            assert!(
                !dir.path()
                    .join("accounts/cancel-test/credentials/.credentials.json")
                    .exists()
            );
        }
        // Reconnect can proceed after cancellation/expiry and temporary CLI state is removed.
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            std::fs::read_dir(dir.path().join("accounts/cancel-test/logins"))
                .unwrap()
                .count(),
            0
        );
    }
    #[tokio::test]
    async fn project_assignment_and_account_deletion_are_consistent() {
        let dir = tempfile::tempdir().unwrap();
        let router = crate::admin(setup(dir.path(), Duration::from_secs(600)).await);
        for index in 0..20 {
            let id = format!("race-{index}");
            assert_eq!(
                call(
                    &router,
                    "POST",
                    ROOT,
                    "admin",
                    Some(json!({"account_id":id,"display_name":"Concurrent lifecycle"}))
                )
                .await
                .0,
                StatusCode::CREATED
            );
            let account_path = format!("{ROOT}/{id}");
            let project_path = format!("/api/private/v1/projects/{id}");
            let (created, deleted) = tokio::join!(
                call(
                    &router,
                    "POST",
                    "/api/private/v1/projects",
                    "admin",
                    Some(if index % 2 == 0 {
                        json!({"project_id":id,"display_name":"Concurrent assignment","upstream_profile":id})
                    } else {
                        json!({"project_id":id,"display_name":"Concurrent route","routing":{"default_upstream":"default","model_overrides":{"rits/zai-org/glm-5-3":id}}})
                    })
                ),
                call(&router, "DELETE", &account_path, "admin", None)
            );
            if created.0 == StatusCode::CREATED {
                assert_eq!(deleted.0, StatusCode::CONFLICT);
                assert_eq!(
                    call(&router, "DELETE", &project_path, "admin", None)
                        .await
                        .0,
                    StatusCode::NO_CONTENT
                );
                assert_eq!(
                    call(&router, "DELETE", &account_path, "admin", None)
                        .await
                        .0,
                    StatusCode::NO_CONTENT
                );
            } else {
                assert_eq!(created.0, StatusCode::BAD_REQUEST);
                assert_eq!(deleted.0, StatusCode::NO_CONTENT);
                assert_eq!(
                    call(&router, "GET", &project_path, "admin", None).await.0,
                    StatusCode::NOT_FOUND
                );
            }
        }
    }
    #[test]
    fn authorization_links_require_provider_host_and_pkce() {
        let valid = "https://claude.com/cai/oauth/authorize?response_type=code&state=x&code_challenge=y&code_challenge_method=S256";
        assert!(authorization_url(&format!("Visit {valid}\n")).is_some());
        for invalid in [
            valid.replace("claude.com", "evil.example"),
            valid.replace("https://", "https://user@"),
            valid.replace("S256", "plain"),
            format!("{valid}#fragment"),
        ] {
            assert!(authorization_url(&(invalid + "\n")).is_none());
        }
    }
}
