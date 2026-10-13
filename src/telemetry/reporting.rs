//! Compact reporting projection, updated atomically with raw request snapshots.
use super::Filter;
use chrono::Utc;
use rusqlite::{Connection, params_from_iter, types::Value as SqlValue};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

const FIELDS: &str = "request_id,sequence,started_at,run_id,client_id,project_id,key_id,backend,model,operation,usage_status,synthetic,completion,latency_ms,input_tokens,output_tokens,cache_read_input_tokens,cache_creation_input_tokens";
const PROJECTION: &str = "request_id,sequence,julianday(json_extract(payload,'$.started_at')),
    json_extract(payload,'$.run_id'),json_extract(payload,'$.client_id'),
    json_extract(payload,'$.project_id'),json_extract(payload,'$.key_id'),
    json_extract(payload,'$.backend'),json_extract(payload,'$.resolved_model'),
    json_extract(payload,'$.operation'),json_extract(payload,'$.usage_status'),
    json_extract(payload,'$.synthetic'),json_extract(payload,'$.completion'),
    json_extract(payload,'$.latency_ms'),
    COALESCE(json_extract(payload,'$.usage.input_tokens'),0),
    COALESCE(json_extract(payload,'$.usage.output_tokens'),0),
    COALESCE(json_extract(payload,'$.usage.cache_read_input_tokens'),0),
    COALESCE(json_extract(payload,'$.usage.cache_creation_input_tokens'),0)";

pub(super) fn migrate(connection: &mut Connection) -> rusqlite::Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS telemetry_migrations(version INTEGER PRIMARY KEY)",
    )?;
    let ready: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM telemetry_migrations WHERE version=1)",
        [],
        |row| row.get(0),
    )?;
    if ready {
        return migrate_rollups(connection);
    }
    let transaction = connection.transaction()?;
    transaction.execute_batch(
        "CREATE TABLE request_metrics (
        sequence INTEGER PRIMARY KEY, request_id TEXT UNIQUE NOT NULL,
        started_at REAL, run_id TEXT, client_id TEXT, project_id TEXT, key_id TEXT,
        backend TEXT, model TEXT, operation TEXT, usage_status TEXT, synthetic INTEGER,
        completion TEXT, latency_ms INTEGER, input_tokens INTEGER NOT NULL,
        output_tokens INTEGER NOT NULL, cache_read_input_tokens INTEGER NOT NULL,
        cache_creation_input_tokens INTEGER NOT NULL);
        CREATE INDEX metrics_started ON request_metrics(started_at);
        CREATE INDEX metrics_project_started ON request_metrics(project_id,started_at);
        CREATE INDEX metrics_project_key_started ON request_metrics(project_id,key_id,started_at);
        CREATE INDEX metrics_run_started ON request_metrics(run_id,started_at);
        CREATE INDEX metrics_model_started ON request_metrics(model,started_at);",
    )?;
    transaction.execute(
        &format!("INSERT INTO request_metrics({FIELDS}) SELECT {PROJECTION} FROM requests"),
        [],
    )?;
    transaction.execute("INSERT INTO telemetry_migrations(version) VALUES(1)", [])?;
    transaction.commit()?;
    migrate_rollups(connection)
}

const DIMENSIONS: &str = "bucket,run_id,client_id,project_id,key_id,backend,model,operation,usage_status,synthetic,completion";
const MEASURES: &str = "requests,input_tokens,output_tokens,cache_read_input_tokens,cache_creation_input_tokens,latency_sum,latency_count";

fn dimensions(prefix: &str, global: bool) -> String {
    let bucket = if global {
        "NULL".into()
    } else {
        format!("julianday(strftime('%Y-%m-%dT%H:%M:00Z',{prefix}.started_at))")
    };
    let mut result = vec![bucket];
    for field in DIMENSIONS.split(',').skip(1) {
        result.push(
            if global && ["run_id", "client_id", "project_id", "key_id"].contains(&field) {
                "NULL".into()
            } else {
                format!("{prefix}.{field}")
            },
        );
    }
    result.join(",")
}

fn measures(prefix: &str) -> Vec<String> {
    vec![
        "1".into(),
        format!("{prefix}.input_tokens"),
        format!("{prefix}.output_tokens"),
        format!("{prefix}.cache_read_input_tokens"),
        format!("{prefix}.cache_creation_input_tokens"),
        format!("COALESCE({prefix}.latency_ms,0)"),
        format!("{prefix}.latency_ms IS NOT NULL"),
    ]
}

fn migrate_rollups(connection: &mut Connection) -> rusqlite::Result<()> {
    let ready: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM telemetry_migrations WHERE version=2)",
        [],
        |row| row.get(0),
    )?;
    if ready {
        return Ok(());
    }
    let transaction = connection.transaction()?;
    for (table, global) in [("metric_rollups", false), ("metric_global_rollups", true)] {
        transaction.execute_batch(&format!("CREATE TABLE {table}(
            cohort_id TEXT PRIMARY KEY,bucket REAL,run_id TEXT,client_id TEXT,project_id TEXT,key_id TEXT,
            backend TEXT,model TEXT,operation TEXT,usage_status TEXT,synthetic INTEGER,completion TEXT,
            requests INTEGER NOT NULL,input_tokens INTEGER NOT NULL,output_tokens INTEGER NOT NULL,
            cache_read_input_tokens INTEGER NOT NULL,cache_creation_input_tokens INTEGER NOT NULL,
            latency_sum INTEGER NOT NULL,latency_count INTEGER NOT NULL)"))?;
        if !global {
            for (name, fields) in [
                ("time", "bucket"),
                ("project", "project_id,bucket"),
                ("key", "project_id,key_id,bucket"),
                ("run", "run_id,bucket"),
                ("model", "model,bucket"),
            ] {
                transaction
                    .execute_batch(&format!("CREATE INDEX rollups_{name} ON {table}({fields})"))?;
            }
        }
        let dims = dimensions("s", global);
        transaction.execute(
            &format!(
                "INSERT INTO {table}(cohort_id,{DIMENSIONS},{MEASURES})
            SELECT json_array({dims}),{dims},COUNT(*),SUM(s.input_tokens),SUM(s.output_tokens),
            SUM(s.cache_read_input_tokens),SUM(s.cache_creation_input_tokens),
            SUM(COALESCE(s.latency_ms,0)),COUNT(s.latency_ms)
            FROM request_metrics s GROUP BY json_array({dims})"
            ),
            [],
        )?;
        let new_dims = dimensions("NEW", global);
        let old_dims = dimensions("OLD", global);
        let additions = MEASURES
            .split(',')
            .map(|field| format!("{field}={field}+excluded.{field}"))
            .collect::<Vec<_>>()
            .join(",");
        let add = format!(
            "INSERT INTO {table}(cohort_id,{DIMENSIONS},{MEASURES})
            VALUES(json_array({new_dims}),{new_dims},{})
            ON CONFLICT(cohort_id) DO UPDATE SET {additions};",
            measures("NEW").join(",")
        );
        let old_values = measures("OLD");
        let subtract = MEASURES
            .split(',')
            .zip(old_values)
            .map(|(field, value)| format!("{field}={field}-({value})"))
            .collect::<Vec<_>>()
            .join(",");
        // A request's old contribution is removed before its latest snapshot is added.
        transaction.execute_batch(&format!(
            "
            CREATE TRIGGER {table}_insert AFTER INSERT ON request_metrics BEGIN {add} END;
            CREATE TRIGGER {table}_before_update BEFORE UPDATE ON request_metrics BEGIN
                UPDATE {table} SET {subtract} WHERE cohort_id=json_array({old_dims}); END;
            CREATE TRIGGER {table}_after_update AFTER UPDATE ON request_metrics BEGIN {add}
                DELETE FROM {table} WHERE requests=0 AND cohort_id=json_array({old_dims}); END;
            CREATE TRIGGER {table}_delete AFTER DELETE ON request_metrics BEGIN
                UPDATE {table} SET {subtract} WHERE cohort_id=json_array({old_dims});
                DELETE FROM {table} WHERE requests=0 AND cohort_id=json_array({old_dims}); END;
        "
        ))?;
    }
    transaction.execute("INSERT INTO telemetry_migrations(version) VALUES(2)", [])?;
    transaction.commit()
}

pub(super) fn upsert_sql() -> String {
    let updates = FIELDS
        .split(',')
        .filter(|field| *field != "request_id")
        .map(|field| format!("{field}=excluded.{field}"))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "INSERT INTO request_metrics({FIELDS}) SELECT {PROJECTION} FROM requests
        WHERE request_id=?1 ON CONFLICT(request_id) DO UPDATE SET {updates}"
    )
}

// Lists and aggregates share the same parameterized scope, including timezone semantics.
fn scope(filter: &Filter, include_time: bool) -> (String, Vec<SqlValue>) {
    let mut clauses = vec!["1=1".to_string()];
    let mut values = Vec::new();
    for (field, value) in [
        ("run_id", &filter.run_id),
        ("client_id", &filter.client_id),
        ("project_id", &filter.project_id),
        ("key_id", &filter.key_id),
        ("backend", &filter.backend),
        ("model", &filter.model),
        ("operation", &filter.operation),
        ("usage_status", &filter.usage_status),
    ] {
        if let Some(value) = value {
            clauses.push(format!("s.{field} = ?"));
            values.push(SqlValue::Text(value.clone()));
        }
    }
    if let Some(value) = filter.synthetic {
        clauses.push("s.synthetic = ?".into());
        values.push(SqlValue::Integer(i64::from(value)));
    }
    if include_time {
        for (operator, value) in [(">=", &filter.from), ("<=", &filter.to)] {
            if let Some(value) = value {
                clauses.push(format!("s.started_at {operator} julianday(?)"));
                values.push(SqlValue::Text(value.clone()));
            }
        }
    }
    (clauses.join(" AND "), values)
}

pub(super) fn list(filter: &Filter, connection: &Connection) -> rusqlite::Result<Value> {
    let (scope, mut values) = scope(filter, true);
    let descending = filter.order.as_deref() == Some("desc");
    let (operator, order, start) = if descending {
        ("<", "DESC", i64::MAX)
    } else {
        (">", "ASC", 0)
    };
    let limit = filter.limit.unwrap_or(100) as usize;
    values.push(SqlValue::Integer(
        filter.cursor.map(|n| n as i64).unwrap_or(start),
    ));
    values.push(SqlValue::Integer((limit + 1) as i64));
    let sql = format!(
        "SELECT s.sequence,r.payload FROM request_metrics s
        JOIN requests r ON r.request_id=s.request_id WHERE {scope} AND s.sequence {operator} ?
        ORDER BY s.sequence {order} LIMIT ?"
    );
    let mut statement = connection.prepare(&sql)?;
    let mut rows = statement
        .query_map(params_from_iter(values), |row| {
            Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let more = rows.len() > limit;
    rows.truncate(limit);
    let cursor = more
        .then(|| rows.last().map(|(sequence, _)| *sequence))
        .flatten();
    let items = rows
        .into_iter()
        .map(|(_, payload)| serde_json::from_str::<Value>(&payload).expect("stored JSON"))
        .collect::<Vec<_>>();
    Ok(json!({"items":items,"next_cursor":cursor}))
}

// Whole minutes use maintained totals. Only the boundary minutes read individual
// snapshots, so arbitrary inclusive timestamps still match the request table.
fn aggregate_rows(filter: &Filter) -> (String, Vec<SqlValue>) {
    const COLUMNS: &str = "s.model,s.operation,s.usage_status,s.synthetic,s.completion";
    const TOTALS: &str = "s.requests,s.input_tokens,s.output_tokens,s.cache_read_input_tokens,s.cache_creation_input_tokens,s.latency_sum,s.latency_count";
    let (metadata, mut values) = scope(filter, false);
    let global = filter.run_id.is_none()
        && filter.client_id.is_none()
        && filter.project_id.is_none()
        && filter.key_id.is_none()
        && filter.from.is_none()
        && filter.to.is_none();
    let table = if global {
        "metric_global_rollups"
    } else {
        "metric_rollups"
    };
    let mut interior = vec![metadata];
    for (operator, value) in [(">", &filter.from), ("<", &filter.to)] {
        if let Some(value) = value {
            interior.push(format!(
                "s.bucket {operator} julianday(strftime('%Y-%m-%dT%H:%M:00Z',?))"
            ));
            values.push(SqlValue::Text(value.clone()));
        }
    }
    let mut sql = format!(
        "SELECT {COLUMNS},{TOTALS} FROM {table} s WHERE {}",
        interior.join(" AND ")
    );
    if filter.from.is_some() || filter.to.is_some() {
        let (full_scope, mut raw_values) = scope(filter, true);
        let mut edges = Vec::new();
        for value in [&filter.from, &filter.to].into_iter().flatten() {
            edges.push("(s.started_at >= julianday(strftime('%Y-%m-%dT%H:%M:00Z',?)) AND s.started_at < julianday(strftime('%Y-%m-%dT%H:%M:00Z',?),'+1 minute'))");
            raw_values.push(SqlValue::Text(value.clone()));
            raw_values.push(SqlValue::Text(value.clone()));
        }
        sql.push_str(&format!(" UNION ALL SELECT {COLUMNS},1 AS requests,s.input_tokens,s.output_tokens,
            s.cache_read_input_tokens,s.cache_creation_input_tokens,COALESCE(s.latency_ms,0) AS latency_sum,
            s.latency_ms IS NOT NULL AS latency_count FROM request_metrics s
            WHERE {full_scope} AND ({})", edges.join(" OR ")));
        values.extend(raw_values);
    }
    (sql, values)
}

pub(super) fn aggregate(filter: &Filter, connection: &Connection) -> rusqlite::Result<Value> {
    let (selected, values) = aggregate_rows(filter);
    // Both statements use one WAL snapshot so model bars and headline totals agree.
    let transaction = connection.unchecked_transaction()?;
    let sql = format!("SELECT COALESCE(SUM(s.requests),0),COALESCE(SUM(CASE WHEN s.synthetic=1 THEN s.requests ELSE 0 END),0),
        COALESCE(SUM(CASE WHEN s.operation='count_tokens' THEN s.requests ELSE 0 END),0),
        COALESCE(SUM(CASE WHEN s.operation!='count_tokens' AND s.usage_status='complete' THEN s.requests ELSE 0 END),0),
        COALESCE(SUM(CASE WHEN s.operation!='count_tokens' AND s.usage_status='partial' THEN s.requests ELSE 0 END),0),
        COALESCE(SUM(CASE WHEN s.operation!='count_tokens' AND s.usage_status='unavailable' THEN s.requests ELSE 0 END),0),
        COALESCE(SUM(CASE WHEN s.operation!='count_tokens' THEN s.input_tokens ELSE 0 END),0),
        COALESCE(SUM(CASE WHEN s.operation!='count_tokens' THEN s.output_tokens ELSE 0 END),0),
        COALESCE(SUM(CASE WHEN s.operation!='count_tokens' THEN s.cache_read_input_tokens ELSE 0 END),0),
        COALESCE(SUM(CASE WHEN s.operation!='count_tokens' THEN s.cache_creation_input_tokens ELSE 0 END),0),
        COALESCE(SUM(CASE WHEN s.completion='complete' THEN s.requests ELSE 0 END),0),
        COALESCE(SUM(CASE WHEN s.completion IN ('failed','cancelled') THEN s.requests ELSE 0 END),0),
        COALESCE(SUM(CASE WHEN s.completion='in_progress' THEN s.requests ELSE 0 END),0),
        SUM(s.latency_sum)*1.0/NULLIF(SUM(s.latency_count),0)
        FROM ({selected}) s");
    let mut result = transaction.query_row(&sql, params_from_iter(values.iter()), |row| {
        let count: u64 = row.get(0)?;
        let synthetic: u64 = row.get(1)?;
        let source = if count == synthetic { Some(true) } else if synthetic == 0 { Some(false) } else { None };
        Ok(json!({
            "scope":"all matching requests", "request_count":count,
            "synthetic":source, "synthetic_requests":synthetic,"provider_requests":count-synthetic,
            "counting_requests":row.get::<_,u64>(2)?,
            "generation_usage_counts":{"complete":row.get::<_,u64>(3)?,"partial":row.get::<_,u64>(4)?,"unavailable":row.get::<_,u64>(5)?},
            "generation_tokens":{"input_tokens":row.get::<_,u64>(6)?,"output_tokens":row.get::<_,u64>(7)?,"cache_read_input_tokens":row.get::<_,u64>(8)?,"cache_creation_input_tokens":row.get::<_,u64>(9)?},
            "request_health":{"complete":row.get::<_,u64>(10)?,"failed_cancelled":row.get::<_,u64>(11)?,"in_progress":row.get::<_,u64>(12)?,"mean_latency_ms":row.get::<_,Option<f64>>(13)?.map(|n| n.round() as u64)},
        }))
    })?;
    {
        let mut statement = transaction.prepare(&format!("SELECT s.model,
            SUM(s.input_tokens+s.output_tokens+s.cache_read_input_tokens+s.cache_creation_input_tokens),SUM(s.requests)
            FROM ({selected}) s WHERE s.operation!='count_tokens'
            GROUP BY s.model ORDER BY 2 DESC,s.model LIMIT 5"))?;
        result["models"] = json!(statement.query_map(params_from_iter(values.iter()), |row| {
            Ok(json!({"model":row.get::<_,String>(0)?,"tokens":row.get::<_,u64>(1)?,"requests":row.get::<_,u64>(2)?}))
        })?.collect::<rusqlite::Result<Vec<_>>>()?);
    }
    transaction.commit()?;
    result["generated_at"] = json!(Utc::now().to_rfc3339());
    result["cached"] = json!(false);
    result["cache_ttl_ms"] = json!(1000);
    Ok(result)
}

#[derive(Default)]
pub(super) struct Cache(Mutex<HashMap<String, (Instant, Value)>>);

impl Cache {
    pub(super) fn get(&self, key: &str) -> Option<Value> {
        let cache = self.0.lock().expect("aggregate cache lock");
        let (when, value) = cache.get(key)?;
        if when.elapsed() >= Duration::from_secs(1) {
            return None;
        }
        let mut value = value.clone();
        value["cached"] = json!(true);
        Some(value)
    }
    pub(super) fn insert(&self, key: String, value: Value) {
        let mut cache = self.0.lock().expect("aggregate cache lock");
        cache.retain(|_, (when, _)| when.elapsed() < Duration::from_secs(1));
        if cache.len() >= 64
            && let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, (when, _))| *when)
                .map(|(key, _)| key.clone())
        {
            cache.remove(&oldest);
        }
        cache.insert(key, (Instant::now(), value));
    }
}
