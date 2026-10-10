use crate::{ApiError, auth::Identity};
use chrono::Utc;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};
use tokio::sync::{Semaphore, mpsc, oneshot};

#[derive(Clone, Serialize, Deserialize)]
pub struct Record {
    pub build_id: String,
    pub schema_version: u8,
    pub request_id: String,
    pub attempt_id: String,
    pub run_id: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub backend: String,
    #[serde(default)]
    pub client_api: String,
    #[serde(default)]
    pub upstream_auth: String,
    #[serde(default)]
    pub upstream_profile: String,
    pub synthetic: bool,
    pub operation: String,
    pub requested_model: String,
    pub resolved_model: String,
    pub client_id: String,
    #[serde(default)]
    pub project_id: Option<String>,
    pub key_id: String,
    pub auth_scheme: String,
    pub requested_max_tokens: Option<u64>,
    pub request_bytes: usize,
    pub message_count: usize,
    pub tool_count: usize,
    pub usage: Value,
    pub usage_source: String,
    pub usage_status: String,
    pub completion: String,
    pub http_status: u16,
    pub latency_ms: Option<u64>,
    #[serde(default)]
    pub first_event_ms: Option<u64>,
    #[serde(default)]
    pub error_type: Option<String>,
    #[serde(default)]
    pub observation_error: Option<String>,
    #[serde(default)]
    pub upstream_request_id: Option<String>,
    #[serde(default)]
    pub upstream_message_id: Option<String>,
    #[serde(default)]
    pub stop_reason: Option<String>,
    pub context: Value,
}

impl Record {
    pub fn new(
        model: &str,
        operation: &str,
        identity: Identity,
        body: &Value,
        bytes: usize,
        run_id: Option<String>,
        window: Option<u64>,
    ) -> Self {
        Self {
            build_id: option_env!("SHIM_BUILD_ID").unwrap_or("unknown").into(),
            schema_version: 1,
            request_id: uuid::Uuid::new_v4().to_string(),
            attempt_id: uuid::Uuid::new_v4().to_string(),
            run_id,
            started_at: Utc::now().to_rfc3339(),
            finished_at: None,
            backend: "mock".into(),
            client_api: "vertex".into(),
            upstream_auth: "none".into(),
            upstream_profile: identity.upstream_profile,
            synthetic: true,
            operation: operation.into(),
            requested_model: model.into(),
            resolved_model: model.into(),
            client_id: identity.client_id,
            project_id: Some(identity.project_id),
            key_id: identity.key_id,
            auth_scheme: identity.scheme.into(),
            requested_max_tokens: body.get("max_tokens").and_then(Value::as_u64),
            request_bytes: bytes,
            message_count: body
                .get("messages")
                .and_then(Value::as_array)
                .map_or(0, Vec::len),
            tool_count: body
                .get("tools")
                .and_then(Value::as_array)
                .map_or(0, Vec::len),
            usage: json!({}),
            usage_source: "synthetic fixture".into(),
            usage_status: "unavailable".into(),
            completion: "in_progress".into(),
            http_status: 200,
            latency_ms: None,
            first_event_ms: None,
            error_type: None,
            observation_error: None,
            upstream_request_id: None,
            upstream_message_id: None,
            stop_reason: None,
            context: json!({"window_tokens":window,"window_source":window.map(|_| "mock deployment override"),"total_input_tokens":null,"input_utilization_ratio":null}),
        }
    }
    pub fn merge_usage(&mut self, usage: &Value) {
        // Only numeric usage fields and numeric nested cache breakdowns are
        // stored. Arbitrary strings/provider content cannot enter telemetry.
        for field in [
            "input_tokens",
            "output_tokens",
            "cache_creation_input_tokens",
            "cache_read_input_tokens",
        ] {
            if let Some(value) = usage.get(field).and_then(Value::as_u64) {
                self.usage[field] = value.into();
            }
        }
        if let Some(cache) = usage.get("cache_creation") {
            let mut safe = json!({});
            for field in ["ephemeral_5m_input_tokens", "ephemeral_1h_input_tokens"] {
                if let Some(value) = cache.get(field).and_then(Value::as_u64) {
                    safe[field] = value.into();
                }
            }
            self.usage["cache_creation"] = safe;
        }
        self.usage_status = "partial".into();
        let counts: Option<Vec<u64>> = [
            "input_tokens",
            "cache_creation_input_tokens",
            "cache_read_input_tokens",
        ]
        .iter()
        .map(|field| self.usage.get(*field).and_then(Value::as_u64))
        .collect();
        let total = counts.and_then(|v| v.into_iter().try_fold(0u64, u64::checked_add));
        self.context["total_input_tokens"] = json!(total);
        self.context["input_utilization_ratio"] = json!(
            total
                .zip(self.context["window_tokens"].as_u64())
                .and_then(|(t, w)| (w > 0).then_some(t as f64 / w as f64))
        );
    }
}

enum Write {
    Request(Box<Record>),
    Event {
        request_id: String,
        sequence: u32,
        usage: Value,
        source: &'static str,
    },
    Flush(oneshot::Sender<()>),
}

// Queued writes above which streamed usage events are dropped. Request records are kept up to
// the hard limit, so a burst loses intermediate usage snapshots before it loses any request.
const EVENT_QUEUE_LIMIT: u64 = 1024;
const REQUEST_QUEUE_LIMIT: u64 = 65536;
// Writes taken from the queue per transaction.
const WRITE_BATCH: usize = 512;

#[derive(Default)]
struct Health {
    pending: AtomicU64,
    dropped_requests: AtomicU64,
    dropped_events: AtomicU64,
    errors: AtomicU64,
    persisted_at_ms: AtomicU64,
    dropped_at_ms: AtomicU64,
    error_at_ms: AtomicU64,
}

fn now_ms() -> u64 {
    Utc::now().timestamp_millis() as u64
}

// Writes one batch in a single transaction. A request's record is rewritten at every usage
// snapshot, so only its latest copy in the batch is written; events are keyed by
// (request_id, sequence) and are coalesced the same way.
fn write_batch(connection: &mut Connection, batch: Vec<Write>, health: &Health) {
    let count = batch
        .iter()
        .filter(|w| !matches!(w, Write::Flush(_)))
        .count() as u64;
    let mut requests: Vec<(String, String)> = Vec::new();
    let mut request_index = std::collections::HashMap::new();
    let mut events: Vec<(String, u32, String)> = Vec::new();
    let mut event_index = std::collections::HashMap::new();
    let mut flushes = Vec::new();
    for write in batch {
        match write {
            Write::Request(record) => {
                let payload = serde_json::to_string(&record).expect("serializable record");
                match request_index.get(&record.request_id) {
                    Some(&i) => requests[i] = (record.request_id, payload),
                    None => {
                        request_index.insert(record.request_id.clone(), requests.len());
                        requests.push((record.request_id, payload));
                    }
                }
            }
            Write::Event {
                request_id,
                sequence,
                usage,
                source,
            } => {
                let payload =
                    json!({"sequence": sequence, "usage": usage, "source": source}).to_string();
                let key = (request_id.clone(), sequence);
                match event_index.get(&key) {
                    Some(&i) => events[i] = (request_id, sequence, payload),
                    None => {
                        event_index.insert(key, events.len());
                        events.push((request_id, sequence, payload));
                    }
                }
            }
            Write::Flush(reply) => flushes.push(reply),
        }
    }
    let result = (|| -> rusqlite::Result<()> {
        let transaction = connection.transaction()?;
        {
            let mut insert = transaction.prepare_cached("INSERT INTO requests(request_id,payload) VALUES (?1,?2) ON CONFLICT(request_id) DO UPDATE SET payload=excluded.payload")?;
            for (id, payload) in &requests {
                insert.execute(params![id, payload])?;
            }
            let mut event = transaction.prepare_cached(
                "INSERT OR REPLACE INTO events(request_id,sequence,payload) VALUES (?1,?2,?3)",
            )?;
            for (id, sequence, payload) in &events {
                event.execute(params![id, sequence, payload])?;
            }
        }
        transaction.commit()
    })();
    if requests.len() + events.len() > 0 {
        if result.is_err() {
            health.errors.fetch_add(1, Ordering::Relaxed);
            health.error_at_ms.store(now_ms(), Ordering::Relaxed);
        } else {
            health.persisted_at_ms.store(now_ms(), Ordering::Relaxed);
        }
    }
    health.pending.fetch_sub(count, Ordering::Relaxed);
    for reply in flushes {
        let _ = reply.send(());
    }
}

#[derive(Clone)]
pub struct Store {
    path: Arc<PathBuf>,
    sender: mpsc::UnboundedSender<Write>,
    health: Arc<Health>,
    readers: Arc<Semaphore>,
    controls: Arc<Semaphore>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut connection = Connection::open(path)?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
            CREATE TABLE IF NOT EXISTS requests (sequence INTEGER PRIMARY KEY AUTOINCREMENT, request_id TEXT UNIQUE NOT NULL, payload TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS events (request_id TEXT NOT NULL, sequence INTEGER NOT NULL, payload TEXT NOT NULL, PRIMARY KEY(request_id, sequence));")?;
        crate::projects::migrate(&connection)?;
        // Unbounded so that request records are never refused; enqueue() bounds it by count.
        let (sender, mut receiver) = mpsc::unbounded_channel::<Write>();
        let health = Arc::new(Health::default());
        let worker_health = health.clone();
        thread::Builder::new()
            .name("telemetry-writer".into())
            .spawn(move || {
                while let Some(first) = receiver.blocking_recv() {
                    let mut batch = vec![first];
                    while batch.len() < WRITE_BATCH {
                        match receiver.try_recv() {
                            Ok(write) => batch.push(write),
                            Err(_) => break,
                        }
                    }
                    write_batch(&mut connection, batch, &worker_health);
                }
            })?;
        Ok(Self {
            path: Arc::new(path.into()),
            sender,
            health,
            readers: Arc::new(Semaphore::new(4)),
            controls: Arc::new(Semaphore::new(4)),
        })
    }
    fn enqueue(&self, write: Write) {
        let (limit, dropped) = match write {
            Write::Event { .. } => (EVENT_QUEUE_LIMIT, &self.health.dropped_events),
            _ => (REQUEST_QUEUE_LIMIT, &self.health.dropped_requests),
        };
        if self.health.pending.fetch_add(1, Ordering::Relaxed) >= limit
            || self.sender.send(write).is_err()
        {
            self.health.pending.fetch_sub(1, Ordering::Relaxed);
            dropped.fetch_add(1, Ordering::Relaxed);
            self.health.dropped_at_ms.store(now_ms(), Ordering::Relaxed);
        }
    }
    pub fn request(&self, record: &Record) {
        self.enqueue(Write::Request(Box::new(record.clone())));
    }
    pub fn event(&self, record: &Record, sequence: u32, source: &'static str) {
        self.enqueue(Write::Event {
            request_id: record.request_id.clone(),
            sequence,
            usage: record.usage.clone(),
            source,
        });
    }
    pub fn health(&self) -> Value {
        let h = &self.health;
        let (requests, events) = (
            h.dropped_requests.load(Ordering::Relaxed),
            h.dropped_events.load(Ordering::Relaxed),
        );
        let at = |v: &AtomicU64| Some(v.load(Ordering::Relaxed)).filter(|&ms| ms > 0);
        json!({"pending_writes": h.pending.load(Ordering::Relaxed), "dropped_records": requests + events, "dropped_requests": requests, "dropped_events": events, "storage_errors": h.errors.load(Ordering::Relaxed), "last_persisted_at_unix_ms": h.persisted_at_ms.load(Ordering::Relaxed), "last_dropped_at_unix_ms": at(&h.dropped_at_ms), "last_storage_error_at_unix_ms": at(&h.error_at_ms)})
    }
    pub async fn flush(&self) {
        let (tx, rx) = oneshot::channel();
        if self.sender.send(Write::Flush(tx)).is_ok() {
            let _ = rx.await;
        }
    }
    pub async fn read<F>(&self, query: F) -> Result<Value, ApiError>
    where
        F: FnOnce(&Connection) -> rusqlite::Result<Value> + Send + 'static,
    {
        let permit = self
            .readers
            .clone()
            .try_acquire_owned()
            .map_err(|_| ApiError::busy())?;
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let connection = Connection::open_with_flags(
                path.as_ref(),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?;
            connection.busy_timeout(std::time::Duration::from_millis(500))?;
            let started = std::time::Instant::now();
            connection.progress_handler(
                1000,
                Some(move || started.elapsed() > std::time::Duration::from_millis(250)),
            );
            query(&connection)
        })
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(|_| ApiError::internal())
    }
    pub async fn control<T, F>(&self, write: bool, query: F) -> Result<T, ApiError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T, ApiError> + Send + 'static,
    {
        let permit = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            self.controls.clone().acquire_owned(),
        )
        .await
        .map_err(|_| ApiError::busy())?
        .map_err(|_| ApiError::internal())?;
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut connection = Connection::open_with_flags(
                path.as_ref(),
                if write {
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                } else {
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                },
            )
            .map_err(|_| ApiError::internal())?;
            connection
                .busy_timeout(std::time::Duration::from_millis(500))
                .map_err(|_| ApiError::internal())?;
            let started = std::time::Instant::now();
            connection.progress_handler(
                1000,
                Some(move || started.elapsed() > std::time::Duration::from_millis(250)),
            );
            // A transaction makes project state, credentials, and revocation one snapshot.
            let transaction = connection
                .transaction_with_behavior(if write {
                    rusqlite::TransactionBehavior::Immediate
                } else {
                    rusqlite::TransactionBehavior::Deferred
                })
                .map_err(|_| ApiError::busy())?;
            let value = query(&transaction)?;
            transaction.commit().map_err(|_| ApiError::internal())?;
            Ok(value)
        })
        .await
        .map_err(|_| ApiError::internal())?
    }
}

#[derive(Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    pub cursor: Option<u64>,
    pub limit: Option<u16>,
    pub run_id: Option<String>,
    pub client_id: Option<String>,
    pub project_id: Option<String>,
    pub key_id: Option<String>,
    pub backend: Option<String>,
    pub model: Option<String>,
    pub operation: Option<String>,
    pub usage_status: Option<String>,
    pub synthetic: Option<bool>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub format: Option<String>,
    // "asc" (default): oldest first, the cursor moves forward. "desc": newest first.
    pub order: Option<String>,
}

impl Filter {
    pub fn validate(&self) -> Result<(), ApiError> {
        if self.limit.is_some_and(|n| n == 0 || n > 500)
            || self.cursor.is_some_and(|n| n > i64::MAX as u64)
        {
            return Err(ApiError::bad("Invalid pagination"));
        }
        for value in [
            &self.run_id,
            &self.client_id,
            &self.project_id,
            &self.key_id,
            &self.backend,
            &self.model,
        ] {
            if value.as_ref().is_some_and(|s| s.len() > 200) {
                return Err(ApiError::bad("Filter too long"));
            }
        }
        if self.operation.as_ref().is_some_and(|s| {
            !["generation", "stream_generation", "count_tokens"].contains(&s.as_str())
        }) || self
            .usage_status
            .as_ref()
            .is_some_and(|s| !["complete", "partial", "unavailable"].contains(&s.as_str()))
        {
            return Err(ApiError::bad("Invalid filter value"));
        }
        let parse = |v: &Option<String>| -> Result<Option<chrono::DateTime<chrono::FixedOffset>>, ApiError> { v.as_ref().map(|s| chrono::DateTime::parse_from_rfc3339(s).map_err(|_| ApiError::bad("Expected RFC3339 timestamp"))).transpose() };
        let (from, to) = (parse(&self.from)?, parse(&self.to)?);
        if from.zip(to).is_some_and(|(a, b)| a > b) {
            return Err(ApiError::bad("Invalid time range"));
        }
        if self
            .format
            .as_ref()
            .is_some_and(|s| s != "json" && s != "jsonl")
        {
            return Err(ApiError::bad("Invalid export format"));
        }
        if self
            .order
            .as_ref()
            .is_some_and(|s| s != "asc" && s != "desc")
        {
            return Err(ApiError::bad("Invalid order"));
        }
        Ok(())
    }
    pub fn list(&self, connection: &Connection) -> rusqlite::Result<Value> {
        let newest_first = self.order.as_deref() == Some("desc");
        let text = "SELECT sequence,payload FROM requests WHERE sequence > ?1
            AND (?2 IS NULL OR json_extract(payload,'$.run_id') = ?2)
            AND (?3 IS NULL OR json_extract(payload,'$.client_id') = ?3)
            AND (?4 IS NULL OR json_extract(payload,'$.key_id') = ?4)
            AND (?5 IS NULL OR json_extract(payload,'$.backend') = ?5)
            AND (?6 IS NULL OR json_extract(payload,'$.resolved_model') = ?6)
            AND (?7 IS NULL OR json_extract(payload,'$.operation') = ?7)
            AND (?8 IS NULL OR json_extract(payload,'$.usage_status') = ?8)
            AND (?9 IS NULL OR json_extract(payload,'$.synthetic') = ?9)
            AND (?10 IS NULL OR julianday(json_extract(payload,'$.started_at')) >= julianday(?10))
            AND (?11 IS NULL OR julianday(json_extract(payload,'$.started_at')) <= julianday(?11))
            AND (?13 IS NULL OR json_extract(payload,'$.project_id') = ?13)
            ORDER BY sequence LIMIT ?12";
        let text = if self.project_id.is_some() {
            text.replace(
                "(?13 IS NULL OR json_extract(payload,'$.project_id') = ?13)",
                "json_extract(payload,'$.project_id') = ?13",
            )
        } else {
            text.into()
        };
        let text = if newest_first {
            text.replace("sequence > ?1", "sequence < ?1")
                .replace("ORDER BY sequence", "ORDER BY sequence DESC")
        } else {
            text
        };
        let mut query = connection.prepare(&text)?;
        let limit = self.limit.unwrap_or(100) as usize;
        let rows = query.query_map(
            params![
                self.cursor
                    .map(|c| c as i64)
                    .unwrap_or(if newest_first { i64::MAX } else { 0 }),
                self.run_id,
                self.client_id,
                self.key_id,
                self.backend,
                self.model,
                self.operation,
                self.usage_status,
                self.synthetic,
                self.from,
                self.to,
                (limit + 1) as i64,
                self.project_id
            ],
            |row| Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?)),
        )?;
        let mut values = Vec::new();
        for row in rows {
            values.push(row?);
        }
        let has_more = values.len() > limit;
        values.truncate(limit);
        let cursor = if has_more {
            values.last().map(|(seq, _)| *seq)
        } else {
            None
        };
        let items: Vec<Value> = values
            .into_iter()
            .map(|(_, value)| serde_json::from_str(&value).expect("valid stored JSON"))
            .collect();
        Ok(json!({"items":items,"next_cursor":cursor}))
    }
}

pub fn summary(page: Value) -> Value {
    let mut totals = json!({"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0});
    let mut counts = json!({"complete":0,"partial":0,"unavailable":0});
    let mut counting = 0u64;
    let mut synthetic_requests = 0u64;
    let mut provider_requests = 0u64;
    for item in page["items"].as_array().unwrap() {
        if item["synthetic"] == true {
            synthetic_requests += 1;
        } else {
            provider_requests += 1;
        }
        if item["operation"] == "count_tokens" {
            counting += 1;
            continue;
        }
        if let Some(status) = item["usage_status"].as_str() {
            counts[status] = json!(counts[status].as_u64().unwrap_or(0) + 1);
        }
        for field in [
            "input_tokens",
            "output_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
        ] {
            totals[field] = json!(
                totals[field]
                    .as_u64()
                    .unwrap()
                    .saturating_add(item["usage"][field].as_u64().unwrap_or(0))
            );
        }
    }
    let synthetic = match (synthetic_requests > 0, provider_requests > 0) {
        (_, false) => Some(true),
        (false, true) => Some(false),
        (true, true) => None,
    };
    json!({"synthetic":synthetic,"synthetic_requests":synthetic_requests,"provider_requests":provider_requests,"generation_usage_counts":counts,"generation_tokens":totals,"counting_requests":counting,"next_cursor":page["next_cursor"],"scope":"bounded page; continue with next_cursor"})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mixed_summary_labels_and_counting_exclusion() {
        let result = summary(json!({"items":[
            {"synthetic":true,"operation":"generation","usage_status":"complete","usage":{"output_tokens":3}},
            {"synthetic":false,"operation":"generation","usage_status":"partial","usage":{"output_tokens":2}},
            {"synthetic":false,"operation":"count_tokens","usage":{"input_tokens":100}}
        ],"next_cursor":42}));
        assert!(result["synthetic"].is_null());
        assert_eq!(result["synthetic_requests"], 1);
        assert_eq!(result["provider_requests"], 2);
        assert_eq!(result["generation_tokens"]["output_tokens"], 5);
        assert_eq!(result["generation_tokens"]["input_tokens"], 0);
        assert_eq!(result["next_cursor"], 42);
    }
    #[tokio::test]
    async fn persistence_cumulative_usage_and_privacy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("telemetry.sqlite");
        let store = Store::open(&path).unwrap();
        let mut record = Record::new(
            "model",
            "stream_generation",
            Identity {
                upstream_profile: "default".into(),
                project_id: crate::projects::DEFAULT_PROJECT.into(),
                client_id: "c".into(),
                key_id: "k".into(),
                scheme: "bearer",
            },
            &json!({"messages":[{"content":"private prompt"}],"max_tokens":100}),
            50,
            None,
            None,
        );
        record.merge_usage(&json!({"input_tokens":12,"cache_creation_input_tokens":5,"cache_read_input_tokens":8,"output_tokens":1,"secret":"private response"}));
        record.merge_usage(&json!({"output_tokens":3}));
        assert_eq!(record.context["total_input_tokens"], 25);
        assert_eq!(record.usage["output_tokens"], 3);
        assert!(record.context["window_tokens"].is_null());
        store.request(&record);
        store.flush().await;
        let page = store
            .read(|conn| Filter::default().list(conn))
            .await
            .unwrap();
        let serialized = page.to_string();
        assert!(!serialized.contains("private prompt") && !serialized.contains("private response"));
        let reopened = Store::open(&path).unwrap();
        assert_eq!(
            reopened
                .read(|conn| Filter::default().list(conn))
                .await
                .unwrap()["items"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(
            Filter {
                limit: Some(501),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }
    fn sample(id: &str) -> Record {
        let mut record = Record::new(
            "model",
            "stream_generation",
            Identity {
                upstream_profile: "default".into(),
                project_id: crate::projects::DEFAULT_PROJECT.into(),
                client_id: "c".into(),
                key_id: "k".into(),
                scheme: "bearer",
            },
            &json!({"messages":[],"max_tokens":10}),
            50,
            None,
            None,
        );
        record.request_id = id.into();
        record
    }
    #[tokio::test]
    async fn newest_first_pages_walk_backwards() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("t.sqlite")).unwrap();
        for i in 0..5 {
            store.request(&sample(&format!("r{i}")));
        }
        store.flush().await;
        let ids = |page: &Value| -> Vec<String> {
            page["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v["request_id"].as_str().unwrap().to_string())
                .collect()
        };
        let desc = |cursor| Filter {
            order: Some("desc".into()),
            limit: Some(2),
            cursor,
            ..Default::default()
        };
        let first = store.read(move |c| desc(None).list(c)).await.unwrap();
        assert_eq!(ids(&first), ["r4", "r3"]);
        let cursor = first["next_cursor"].as_u64();
        let second = store.read(move |c| desc(cursor).list(c)).await.unwrap();
        assert_eq!(ids(&second), ["r2", "r1"]);
        let asc = store
            .read(|c| {
                Filter {
                    limit: Some(2),
                    ..Default::default()
                }
                .list(c)
            })
            .await
            .unwrap();
        assert_eq!(ids(&asc), ["r0", "r1"]);
        assert!(
            Filter {
                order: Some("newest".into()),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }
    #[tokio::test]
    async fn bursts_drop_events_before_requests() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("t.sqlite")).unwrap();
        let record = sample("burst");
        // Far more writes than the event limit, enqueued faster than any writer drains them.
        for i in 0..3000u32 {
            store.event(&record, i, "SSE usage snapshot");
            store.request(&record);
        }
        store.flush().await;
        let health = store.health();
        assert_eq!(health["dropped_requests"], 0);
        assert_eq!(health["pending_writes"], 0);
        assert_eq!(health["storage_errors"], 0);
        let rows = store.read(|c| Filter::default().list(c)).await.unwrap();
        assert_eq!(rows["items"].as_array().unwrap().len(), 1);
        if health["dropped_events"] != 0 {
            assert!(health["last_dropped_at_unix_ms"].as_u64().is_some());
        }
    }
}
