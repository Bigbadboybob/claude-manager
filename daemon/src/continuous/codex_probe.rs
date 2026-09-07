//! Codex rollout observations grounded in the pinned 0.153.4 fixtures.
//! Errors must come from runtime error objects, never assistant prose.
use super::probe::{TailProbe, TailShape};
use serde_json::Value;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

#[derive(Debug, PartialEq, Eq)]
pub enum ErrorKind {
    AuthExpired,
    UsageLimited,
    ModelUnavailable,
    PoolUnavailable,
    Other,
}

pub fn classify_error(error: &Value) -> ErrorKind {
    let code = error
        .get("codex_error_info")
        .and_then(Value::as_str)
        .unwrap_or("");
    let message = error.get("message").and_then(Value::as_str).unwrap_or("");
    match code {
        "unauthorized" => ErrorKind::AuthExpired,
        "usage_limit_exceeded" => ErrorKind::UsageLimited,
        "model_not_found" => ErrorKind::ModelUnavailable,
        _ if message.starts_with("unexpected status 503 Service Unavailable: No available accounts") => ErrorKind::PoolUnavailable,
        _ if message.starts_with("unexpected status 401 Unauthorized:") => ErrorKind::AuthExpired,
        _ if message.starts_with("unexpected status 404 Not Found:")
            && message.to_ascii_lowercase().contains("model") =>
        {
            ErrorKind::ModelUnavailable
        }
        _ => ErrorKind::Other,
    }
}

/// Newest substantive event after this run's delivery. Unknown/partial JSON
/// cannot be skipped to discover an older completion behind active work.
pub fn probe(path: &Path, after: f64) -> Option<TailProbe> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(256 * 1024)))
        .ok()?;
    let mut bytes = Vec::new();
    file.take(256 * 1024 + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() > 256 * 1024 {
        return None;
    }
    let text = std::str::from_utf8(&bytes).ok()?;
    for line in text
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(200)
    {
        let value: Value = serde_json::from_str(line).ok()?;
        let top = value.get("type")?.as_str()?;
        let payload = value.get("payload")?;
        let kind = payload.get("type").and_then(Value::as_str).unwrap_or("");
        if matches!(
            (top, kind),
            ("event_msg", "token_count") | ("session_meta", _)
        ) {
            continue;
        }
        let stamp = value.get("timestamp")?.as_str()?;
        if !stamp.ends_with('Z') {
            return None;
        }
        let observed_at = crate::workflow::history::iso8601_to_ms(stamp)? as f64 / 1000.0;
        if !after.is_finite() || observed_at < after {
            return None;
        }
        let mut result = TailProbe {
            shape: TailShape::MidTurn,
            auth_error: None,
            usage_limit: None,
            pool_unavailable: None,
        };
        match (top, kind) {
            ("event_msg", "task_complete") => {
                if let Some(error) = payload.get("error").filter(|e| !e.is_null()) {
                    match classify_error(error) {
                        ErrorKind::AuthExpired => {
                            result.auth_error = Some(
                                "Codex runtime rejected authentication (401/unauthorized).".into(),
                            )
                        }
                        ErrorKind::UsageLimited => {
                            result.usage_limit =
                                Some("Codex runtime reported usage_limit_exceeded.".into())
                        }
                        ErrorKind::PoolUnavailable => {
                            result.pool_unavailable = Some("Codex pool cannot serve this request. Check pool capacity and continuation ownership; work reconciliation is required before recovery.".into());
                        }
                        // A configuration/transport/unknown error is an
                        // unresolved turn, not proof of abandoned work.
                        _ => return None,
                    }
                }
                result.shape = TailShape::TurnComplete;
            }
            ("event_msg", "user_message") => result.shape = TailShape::AwaitingResponse,
            ("event_msg", "item_completed") | ("event_msg", "item_started") => {
                match payload.pointer("/item/type").and_then(Value::as_str)? {
                    "UserMessage" => result.shape = TailShape::AwaitingResponse,
                    "AgentMessage" | "Reasoning" | "CommandExecution" | "McpToolCall" => {}
                    _ => return None,
                }
            }
            (
                "event_msg",
                "task_started" | "agent_message" | "agent_reasoning" | "exec_command_begin"
                | "exec_command_end",
            ) => {}
            (
                "response_item",
                "function_call"
                | "function_call_output"
                | "custom_tool_call"
                | "custom_tool_call_output"
                | "reasoning",
            ) => {}
            ("response_item", "message") => match payload.get("role").and_then(Value::as_str)? {
                "user" => result.shape = TailShape::AwaitingResponse,
                "assistant" => {}
                _ => return None,
            },
            // Compaction/rotation bookkeeping without a post-delivery turn
            // is unknown. A new tool kind is never a fabricated completion.
            _ => return None,
        }
        return Some(result);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    const SUCCESS: &str = include_str!("../../tests/fixtures/codex-0.153.4/success.jsonl");
    const AUTH: &str = include_str!("../../tests/fixtures/codex-0.153.4/invalid_api_key.jsonl");
    const USAGE: &str =
        include_str!("../../tests/fixtures/codex-0.153.4/usage_limit_reached.jsonl");
    const UNKNOWN: &str = include_str!("../../tests/fixtures/codex-0.153.4/model_not_found.jsonl");

    #[test]
    fn real_pool_failure_is_distinct_from_account_auth_or_quota() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let fixture = include_str!("../../tests/fixtures/codex-0.153.4/pool_unavailable.jsonl");
        std::fs::write(&path, fixture).unwrap();
        let observed = probe(&path, 0.0).unwrap();
        assert_eq!(observed.shape, TailShape::TurnComplete);
        assert!(observed.pool_unavailable.is_some());
        assert!(observed.auth_error.is_none() && observed.usage_limit.is_none());
        let user = serde_json::json!({"timestamp":"2026-09-08T00:00:00.000Z","type":"event_msg",
            "payload":{"type":"user_message","message":"unexpected status 503 Service Unavailable: No available accounts"}});
        std::fs::write(&path, format!("{fixture}\n{user}\n")).unwrap();
        let newer = probe(&path, 0.0).unwrap();
        assert_eq!(newer.shape, TailShape::AwaitingResponse);
        assert!(newer.pool_unavailable.is_none());
    }

    #[test]
    fn codex_probe_classifies_real_pinned_cli_success_and_error_shapes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        for (fixture, auth, usage) in [
            (SUCCESS, false, false),
            (AUTH, true, false),
            (USAGE, false, true),
        ] {
            std::fs::write(&path, fixture).unwrap();
            let result = probe(&path, 0.0).unwrap();
            assert_eq!(result.shape, TailShape::TurnComplete);
            assert_eq!(result.auth_error.is_some(), auth);
            assert_eq!(result.usage_limit.is_some(), usage);
            assert!(
                probe(&path, 2_000_000_000.0).is_none(),
                "an old turn cannot close a newer delivery"
            );
        }
        std::fs::write(&path, UNKNOWN).unwrap();
        assert!(probe(&path, 0.0).is_none());
    }

    #[test]
    fn codex_probe_new_input_tools_compaction_and_partial_records_hide_old_completion() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        for (payload, shape) in [
            (
                serde_json::json!({"type":"item_completed","item":{"type":"UserMessage"}}),
                Some(TailShape::AwaitingResponse),
            ),
            (
                serde_json::json!({"type":"item_started","item":{"type":"CommandExecution"}}),
                Some(TailShape::MidTurn),
            ),
            (
                serde_json::json!({"type":"agent_message","message":"You've hit your usage limit. unexpected status 401 Unauthorized:"}),
                Some(TailShape::MidTurn),
            ),
            (serde_json::json!({"type":"compacted"}), None),
            (serde_json::json!({"type":"future_event"}), None),
        ] {
            let event = serde_json::json!({"timestamp":"2026-09-07T01:00:00.000Z","type":"event_msg","payload":payload});
            std::fs::write(&path, format!("{SUCCESS}\n{event}\n")).unwrap();
            let result = probe(&path, 0.0);
            assert_eq!(result.as_ref().map(|p| p.shape), shape);
            assert!(result.is_none_or(|p| p.auth_error.is_none() && p.usage_limit.is_none()));
        }
        std::fs::write(&path, format!("{SUCCESS}\n{{partial")).unwrap();
        assert!(probe(&path, 0.0).is_none());
    }
}
