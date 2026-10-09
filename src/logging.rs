//! Allowlisted JSON lines for container logs. Never serialize request bodies,
//! headers, provider errors, credential objects, or the full telemetry record.
use crate::telemetry::Record;
use serde_json::{Value, json};
use std::io::{Write, stdout};

pub fn emit(level: &'static str, event: &'static str, fields: Value) {
    let mut line = fields.as_object().cloned().unwrap_or_default();
    line.insert("timestamp".into(), chrono::Utc::now().to_rfc3339().into());
    line.insert("level".into(), level.into());
    line.insert("event".into(), event.into());
    line.insert("version".into(), env!("CARGO_PKG_VERSION").into());
    line.insert(
        "build_id".into(),
        option_env!("SHIM_BUILD_ID").unwrap_or("unknown").into(),
    );
    // Hold the lock for the whole line so concurrent requests cannot interleave.
    // A closed log sink must not crash inference or replace its result.
    let _ = writeln!(stdout().lock(), "{}", Value::Object(line));
}

fn completion_fields(record: &Record) -> Value {
    let mut usage = json!({});
    for field in [
        "input_tokens",
        "output_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
    ] {
        if let Some(value) = record.usage.get(field).and_then(Value::as_u64) {
            usage[field] = value.into();
        }
    }
    for field in ["ephemeral_5m_input_tokens", "ephemeral_1h_input_tokens"] {
        if let Some(value) = record
            .usage
            .get("cache_creation")
            .and_then(|cache| cache.get(field))
            .and_then(Value::as_u64)
        {
            if usage.get("cache_creation").is_none() {
                usage["cache_creation"] = json!({});
            }
            usage["cache_creation"][field] = value.into();
        }
    }
    json!({
        "request_id": record.request_id,
        "attempt_id": record.attempt_id,
        "backend": record.backend,
        "client_api": record.client_api,
        "upstream_auth": record.upstream_auth,
        "synthetic": record.synthetic,
        "operation": record.operation,
        "model": record.requested_model,
        "upstream_model": record.resolved_model,
        "http_status": record.http_status,
        "completion": record.completion,
        "latency_ms": record.latency_ms,
        "first_event_ms": record.first_event_ms,
        "usage_status": record.usage_status,
        "usage": usage,
        "context": {
            "window_tokens": record.context.get("window_tokens").and_then(Value::as_u64),
            "total_input_tokens": record.context.get("total_input_tokens").and_then(Value::as_u64),
        },
        "error_type": record.error_type,
        "observation_error": record.observation_error,
        "stop_reason": record.stop_reason,
        "upstream_request_id": record.upstream_request_id,
        "upstream_message_id": record.upstream_message_id,
    })
}

pub(crate) fn completed(record: &Record) {
    let level = match record.completion.as_str() {
        "failed" => "error",
        "cancelled" => "warn",
        _ if record.observation_error.is_some() => "warn",
        _ => "info",
    };
    emit(level, "request_completed", completion_fields(record));
    if record.completion == "failed" && record.backend == "anthropic" {
        emit(
            "error",
            "upstream_failure",
            json!({
                "request_id": record.request_id,
                "attempt_id": record.attempt_id,
                "http_status": record.http_status,
                "error_type": record.error_type,
                "observation_error": record.observation_error,
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Identity;

    #[test]
    fn completion_logs_exclude_content_and_untrusted_usage_fields() {
        let secret = "private_log_marker";
        let mut record = Record::new(
            "model",
            "generation",
            Identity {
                upstream_profile: "default".into(),
                project_id: crate::projects::DEFAULT_PROJECT.into(),
                client_id: secret.into(),
                key_id: secret.into(),
                scheme: "bearer",
            },
            &json!({"messages":[{"content":secret}],"tools":[{"description":secret}]}),
            100,
            Some(secret.into()),
            Some(200_000),
        );
        record.merge_usage(&json!({"input_tokens":12,"output_tokens":3}));
        record.usage["content"] = json!(secret);
        record.usage["cache_creation"] = json!({"ephemeral_5m_input_tokens":7,"secret":secret});
        record.context["window_source"] = json!(secret);
        record.context["total_input_tokens"] = json!(secret);
        let fields = completion_fields(&record);
        assert!(!fields.to_string().contains(secret));
        assert_eq!(fields["usage"]["input_tokens"], 12);
        assert_eq!(fields["usage"]["output_tokens"], 3);
        assert_eq!(
            fields["usage"]["cache_creation"]["ephemeral_5m_input_tokens"],
            7
        );
        assert_eq!(fields["context"]["window_tokens"], 200_000);
        assert!(fields["context"]["total_input_tokens"].is_null());
        record.usage["input_tokens"] = json!(secret);
        assert!(
            completion_fields(&record)["usage"]
                .get("input_tokens")
                .is_none()
        );
    }
}
