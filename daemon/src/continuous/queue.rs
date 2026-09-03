//! Continuous Tasks Phase 4 — the daemon side of named queues
//! (DESIGN_SCRAPER_MIGRATION.md §3, DESIGN_CONTINUOUS_TASKS.md §9).
//!
//! The planning API OWNS the `queue_items` table (sql/012); this module is a
//! thin blocking HTTP client over its `/queues/{queue}/*` endpoints — the
//! daemon never grows a psql client (resolves the doc's §18 open question #2).
//! Callers:
//!
//!   - the scheduler's Consumer due-check polls [`QueueClient::stats`]
//!     (throttled by a per-queue depth cache in `scheduler.rs`);
//!   - `methods::trigger` claims a batch at fire time
//!     ([`QueueClient::claim`]), writes it to `<worktree>/.queue/`, and acks
//!     it consumed after delivery ([`QueueClient::ack`]) — or releases it
//!     back to pending on a spawn failure ([`QueueClient::requeue`]);
//!   - the `enqueue` / `queue.stats` RPC methods route agent/operator calls
//!     through [`QueueClient::enqueue`] / [`QueueClient::stats`].
//!
//! ureq (blocking) + the config-first env-fallback credential chain — both
//! inherited from `crate::planning_client` (see its module docs for why).
//! Errors reuse [`PlanningClientError`] so the ErrorCode mapping
//! (`to_method_err`) stays uniform across every API-talking method.

use serde::Deserialize;
use std::time::Duration;

use crate::planning_client::{resolve_api_token, resolve_api_url, PlanningClientError};

/// One claimed queue item, as returned by `POST /queues/{queue}/claim`.
/// `payload` is the producer's free-form JSON — the daemon never parses it,
/// it lands verbatim in the batch file the fired agent reads.
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
pub struct QueueItem {
    pub id: String,
    pub payload: serde_json::Value,
    #[serde(default)]
    pub dedupe_key: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    pub enqueued_at: String,
}

/// `GET /queues/{queue}` — the Consumer due-check's input.
#[derive(Clone, Debug, Deserialize)]
pub struct QueueStats {
    pub pending: u64,
    pub claimed: u64,
    /// `min(enqueued_at) FILTER (WHERE state = 'pending')`, ISO-8601 as
    /// rendered by `datetime.isoformat()` over a `TIMESTAMPTZ` (sql/012), e.g.
    /// `2026-09-02T14:05:03.123456+00:00`. `None` when the queue holds no
    /// pending items. This is the TRUE age of the oldest waiting item — the
    /// Consumer window arm measures against it rather than against the task's
    /// own `last_fired_at` (see `scheduler::collect_due`).
    #[serde(default)]
    pub oldest_pending_at: Option<String>,
}

impl QueueStats {
    /// [`Self::oldest_pending_at`] as unix seconds, or `None` when absent or
    /// unparseable (an unparseable timestamp degrades the window arm to its
    /// `last_fired_at` fallback rather than wedging the consumer).
    pub fn oldest_pending_unix(&self) -> Option<u64> {
        self.oldest_pending_at
            .as_deref()
            .and_then(parse_iso8601_unix)
    }
}

/// Days from 1970-01-01 to `y-m-d` (proleptic Gregorian). Howard Hinnant's
/// `days_from_civil`, the standard branch-free civil-calendar algorithm.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as i64; // [0, 399]
    let mp = ((m + 9) % 12) as i64; // Mar=0 … Feb=11
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Minimal ISO-8601 / RFC-3339 → unix-seconds parser for the ONE producer we
/// consume: `datetime.isoformat()` on an asyncpg `TIMESTAMPTZ`. Accepts
/// `YYYY-MM-DD[T| ]HH:MM[:SS[.frac]][Z|±HH[:]MM]`; fractional seconds are
/// truncated (second resolution is all the window arm needs) and a missing
/// offset is read as UTC (the column is tz-aware, so this is only a
/// defensive branch). Returns `None` for anything malformed — the caller
/// treats that as "unknown", never as "now".
///
/// Hand-rolled because the daemon deliberately carries no date/time crate
/// (see `daemon/Cargo.toml`); pulling `chrono`/`time` in for one field would
/// add a dependency tree to a binary that has none.
pub fn parse_iso8601_unix(s: &str) -> Option<u64> {
    let s = s.trim();
    let bytes = s.as_bytes();
    if bytes.len() < 16 {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<i64> { s.get(from..to)?.parse::<i64>().ok() };
    if bytes[4] != b'-' || bytes[7] != b'-' || !(bytes[10] == b'T' || bytes[10] == b' ') {
        return None;
    }
    let year = num(0, 4)?;
    let month = num(5, 7)?;
    let day = num(8, 10)?;
    if bytes[13] != b':' {
        return None;
    }
    let hour = num(11, 13)?;
    let minute = num(14, 16)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) {
        return None;
    }
    // Optional `:SS[.frac]`, then an optional zone suffix.
    let mut idx = 16;
    let mut second = 0i64;
    if bytes.get(idx) == Some(&b':') {
        second = num(idx + 1, idx + 3)?;
        if !(0..=60).contains(&second) {
            return None; // 60 = leap second; clamped below.
        }
        second = second.min(59);
        idx += 3;
        if bytes.get(idx) == Some(&b'.') {
            idx += 1;
            while bytes.get(idx).is_some_and(|c| c.is_ascii_digit()) {
                idx += 1; // truncate the fraction
            }
        }
    }
    // Zone: end-of-string / `Z` = UTC, else ±HH:MM or ±HHMM.
    let offset_secs: i64 = match bytes.get(idx) {
        None => 0,
        Some(b'Z') | Some(b'z') if idx + 1 == bytes.len() => 0,
        Some(sign @ (b'+' | b'-')) => {
            let sign = if *sign == b'-' { -1 } else { 1 };
            let rest = s.get(idx + 1..)?;
            let (oh, om) = match rest.len() {
                5 if rest.as_bytes()[2] == b':' => (rest[0..2].to_string(), rest[3..5].to_string()),
                4 => (rest[0..2].to_string(), rest[2..4].to_string()),
                2 => (rest[0..2].to_string(), "0".to_string()),
                _ => return None,
            };
            let oh: i64 = oh.parse().ok()?;
            let om: i64 = om.parse().ok()?;
            if !(0..=23).contains(&oh) || !(0..=59).contains(&om) {
                return None;
            }
            sign * (oh * 3600 + om * 60)
        }
        _ => return None,
    };
    let days = days_from_civil(year, month as u32, day as u32);
    let epoch = days * 86_400 + hour * 3600 + minute * 60 + second - offset_secs;
    u64::try_from(epoch).ok()
}

/// Queue-name allowlist (twin of `task::validate_task_id`, distinct error
/// wording). Validated BEFORE any URL construction so an untrusted queue name
/// can't smuggle path segments into the API request.
pub fn validate_queue_name(queue: &str) -> Result<(), String> {
    if queue.is_empty() {
        return Err("queue name is empty".to_string());
    }
    if queue.len() > 128 {
        return Err(format!("queue name too long: {} > 128", queue.len()));
    }
    for c in queue.chars() {
        if !(c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return Err(format!(
                "disallowed character in queue name {:?}: {:?}",
                queue, c
            ));
        }
    }
    Ok(())
}

/// Blocking client over the planning API's `/queues/*` endpoints. Constructed
/// per call-site from the daemon.toml overrides (config-first, env-fallback —
/// the same chain as `planning_client::propose_task`).
#[derive(Debug)]
pub struct QueueClient {
    api_url: String,
    api_token: String,
}

impl QueueClient {
    /// Resolve credentials or fail with the same `MissingConfig` diagnostics
    /// the planning client raises (operator knows what to set).
    pub fn from_overrides(
        api_url_override: Option<&str>,
        api_token_override: Option<&str>,
    ) -> Result<Self, PlanningClientError> {
        Ok(QueueClient {
            api_url: resolve_api_url(api_url_override)?,
            api_token: resolve_api_token(api_token_override)?,
        })
    }

    fn agent() -> ureq::Agent {
        ureq::Agent::new_with_config(
            ureq::config::Config::builder()
                .timeout_global(Some(Duration::from_secs(10)))
                .http_status_as_error(false)
                .build(),
        )
    }

    fn auth(&self) -> String {
        format!("Bearer {}", self.api_token)
    }

    /// Shared POST-JSON → JSON plumbing. Maps ureq errors onto the planning
    /// client's variants (`StatusCode` → `ApiError`, transport → `Transport`).
    fn post_json(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, PlanningClientError> {
        let endpoint = format!("{}{}", self.api_url, path);
        let response = match Self::agent()
            .post(&endpoint)
            .header("Authorization", &self.auth())
            .send_json(body)
        {
            Ok(r) => r,
            Err(e) => return Err(PlanningClientError::Transport(e.to_string())),
        };
        crate::planning_client::decode_json_response(response, "queue POST response")
    }

    fn get_json(&self, path: &str) -> Result<serde_json::Value, PlanningClientError> {
        let endpoint = format!("{}{}", self.api_url, path);
        let response = match Self::agent()
            .get(&endpoint)
            .header("Authorization", &self.auth())
            .call()
        {
            Ok(r) => r,
            Err(e) => return Err(PlanningClientError::Transport(e.to_string())),
        };
        crate::planning_client::decode_json_response(response, "queue GET response")
    }

    /// `POST /queues/{queue}/items` — returns the API's
    /// `{enqueued, deduped, id, depth}` verbatim (the RPC method forwards it).
    pub fn enqueue(
        &self,
        queue: &str,
        payload: &serde_json::Value,
        dedupe_key: Option<&str>,
        source: Option<&str>,
    ) -> Result<serde_json::Value, PlanningClientError> {
        let mut body = serde_json::json!({ "payload": payload });
        if let Some(k) = dedupe_key {
            body["dedupe_key"] = serde_json::Value::String(k.to_string());
        }
        if let Some(s) = source {
            body["source"] = serde_json::Value::String(s.to_string());
        }
        self.post_json(&format!("/queues/{}/items", queue), &body)
    }

    /// `GET /queues/{queue}` — pending/claimed depth + oldest pending age.
    pub fn stats(&self, queue: &str) -> Result<QueueStats, PlanningClientError> {
        let v = self.get_json(&format!("/queues/{}", queue))?;
        serde_json::from_value(v)
            .map_err(|e| PlanningClientError::Transport(format!("decode stats: {}", e)))
    }

    /// `POST /queues/{queue}/claim` — atomically claim up to `max_items`
    /// oldest pending items (`claimed_by` is the audit label, `<task>#<seq>`).
    pub fn claim(
        &self,
        queue: &str,
        max_items: u32,
        claimed_by: &str,
    ) -> Result<Vec<QueueItem>, PlanningClientError> {
        let body = serde_json::json!({ "max_items": max_items, "claimed_by": claimed_by });
        let v = self.post_json(&format!("/queues/{}/claim", queue), &body)?;
        let items = v.get("items").cloned().unwrap_or(serde_json::json!([]));
        serde_json::from_value(items)
            .map_err(|e| PlanningClientError::Transport(format!("decode claim items: {}", e)))
    }

    /// `POST /queues/{queue}/ack` — claimed → consumed. Returns rows flipped.
    pub fn ack(&self, queue: &str, ids: &[String]) -> Result<u64, PlanningClientError> {
        let body = serde_json::json!({ "ids": ids });
        let v = self.post_json(&format!("/queues/{}/ack", queue), &body)?;
        Ok(v.get("acked").and_then(|a| a.as_u64()).unwrap_or(0))
    }

    /// `POST /queues/{queue}/requeue` — claimed → pending (crash recovery).
    /// ALWAYS id-scoped: the daemon only ever releases the batch IT claimed.
    /// The endpoint's requeue-everything branch needs an explicit
    /// `{"all": true}` (api/main.py) and has no daemon caller by design — a
    /// blanket requeue during an in-flight fire would re-pend another fire's
    /// live batch. An empty slice is a no-op the API answers with `0`.
    pub fn requeue(&self, queue: &str, ids: &[String]) -> Result<u64, PlanningClientError> {
        let body = serde_json::json!({ "ids": ids });
        let v = self.post_json(&format!("/queues/{}/requeue", queue), &body)?;
        Ok(v.get("requeued").and_then(|r| r.as_u64()).unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planning_client::spawn_stub_api_for_test;

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::planning_client::test_env_lock()
    }

    fn clear_env() {
        unsafe {
            std::env::remove_var("CM_API_URL");
            std::env::remove_var("CM_API_TOKEN");
        }
    }

    /// The one producer shape we consume — `datetime.isoformat()` over a
    /// TIMESTAMPTZ — plus the defensive variants (bare `Z`, naive, compact
    /// offset, non-UTC offset, leap second) and rejects. Expected values are
    /// Python `datetime.fromisoformat(...).timestamp()`.
    #[test]
    fn parse_iso8601_unix_covers_api_shapes_and_rejects_garbage() {
        assert_eq!(parse_iso8601_unix("2026-07-04T00:00:00+00:00"), Some(1_783_123_200));
        assert_eq!(
            parse_iso8601_unix("2026-09-02T14:05:03.123456+00:00"),
            Some(1_788_357_903),
            "fractional seconds truncated, not rejected",
        );
        assert_eq!(parse_iso8601_unix("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso8601_unix("2026-09-02 14:05:03+00:00"), Some(1_788_357_903));
        assert_eq!(
            parse_iso8601_unix("2026-03-01T12:00:00-05:00"),
            Some(1_772_384_400),
            "a negative offset shifts FORWARD in UTC",
        );
        assert_eq!(parse_iso8601_unix("2026-03-01T12:00:00-0500"), Some(1_772_384_400));
        assert_eq!(
            parse_iso8601_unix("2000-02-29T23:59:59Z"),
            Some(951_868_799),
            "leap day on a 400-year leap year",
        );
        assert_eq!(
            parse_iso8601_unix("2026-09-02T14:05"),
            Some(1_788_357_900),
            "seconds optional; no zone = UTC",
        );
        for bad in [
            "",
            "not-a-time",
            "2026-09-02",
            "2026-09-02T14:05:03+99:00",
            "2026-13-02T14:05:03Z",
            "2026-09-02T25:05:03Z",
            "2026/09/02T14:05:03Z",
            "1969-12-31T23:59:59Z", // pre-epoch has no u64 representation
        ] {
            assert_eq!(parse_iso8601_unix(bad), None, "expected reject for {:?}", bad);
        }
    }

    #[test]
    fn validate_queue_name_allowlist() {
        assert!(validate_queue_name("scraper-creation-proposals").is_ok());
        assert!(validate_queue_name("q_1").is_ok());
        assert!(validate_queue_name(&"x".repeat(128)).is_ok());
        for bad in ["", "a/b", "a b", "a;b", "../x", &"x".repeat(129) as &str] {
            assert!(validate_queue_name(bad).is_err(), "expected reject for {:?}", bad);
        }
    }

    /// Wire-shape pin for `stats`: GET /queues/{q}, bearer auth, decode.
    #[test]
    fn stats_sends_get_and_decodes() {
        let _g = env_lock();
        clear_env();
        let (port, captured) = spawn_stub_api_for_test(
            200,
            r#"{"queue":"q1","pending":7,"claimed":2,"oldest_pending_at":"2026-07-04T00:00:00+00:00"}"#,
        );
        let client = QueueClient::from_overrides(
            Some(&format!("http://127.0.0.1:{}", port)),
            Some("tok-q"),
        )
        .expect("client");
        let stats = client.stats("q1").expect("stats ok");
        assert_eq!(stats.pending, 7);
        assert_eq!(stats.claimed, 2);
        assert_eq!(
            stats.oldest_pending_at.as_deref(),
            Some("2026-07-04T00:00:00+00:00"),
        );
        assert_eq!(
            stats.oldest_pending_unix(),
            Some(1_783_123_200),
            "decoded to unix seconds for the Consumer window arm",
        );
        let cap = captured.lock().unwrap();
        let (method, path) = cap.method_and_path();
        assert_eq!(method, "GET");
        assert_eq!(path, "/queues/q1");
        assert_eq!(cap.auth_header().as_deref(), Some("Bearer tok-q"));
    }

    /// Wire-shape pin for `claim`: POST body carries max_items + claimed_by;
    /// items decode into `QueueItem`s with free-form payloads.
    #[test]
    fn claim_sends_post_and_decodes_items() {
        let _g = env_lock();
        clear_env();
        let (port, captured) = spawn_stub_api_for_test(
            200,
            r#"{"items":[{"id":"11111111-1111-1111-1111-111111111111","payload":{"url":"https://x"},"dedupe_key":"x.com","source":"aux","enqueued_at":"2026-07-04T00:00:00+00:00"}]}"#,
        );
        let client = QueueClient::from_overrides(
            Some(&format!("http://127.0.0.1:{}", port)),
            Some("tok"),
        )
        .expect("client");
        let items = client.claim("q1", 10, "scraper-creation#4").expect("claim ok");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "11111111-1111-1111-1111-111111111111");
        assert_eq!(items[0].payload["url"], "https://x");
        assert_eq!(items[0].dedupe_key.as_deref(), Some("x.com"));
        let cap = captured.lock().unwrap();
        let (method, path) = cap.method_and_path();
        assert_eq!(method, "POST");
        assert_eq!(path, "/queues/q1/claim");
        let body: serde_json::Value =
            serde_json::from_str(cap.raw.split("\r\n\r\n").nth(1).unwrap_or("{}")).unwrap();
        assert_eq!(body["max_items"], 10);
        assert_eq!(body["claimed_by"], "scraper-creation#4");
    }

    /// `ack` posts the id list and surfaces the acked count.
    #[test]
    fn ack_sends_ids_and_returns_count() {
        let _g = env_lock();
        clear_env();
        let (port, captured) = spawn_stub_api_for_test(200, r#"{"acked":2}"#);
        let client = QueueClient::from_overrides(
            Some(&format!("http://127.0.0.1:{}", port)),
            Some("tok"),
        )
        .expect("client");
        let n = client
            .ack("q1", &["a".to_string(), "b".to_string()])
            .expect("ack ok");
        assert_eq!(n, 2);
        let cap = captured.lock().unwrap();
        let (_, path) = cap.method_and_path();
        assert_eq!(path, "/queues/q1/ack");
        let body: serde_json::Value =
            serde_json::from_str(cap.raw.split("\r\n\r\n").nth(1).unwrap_or("{}")).unwrap();
        assert_eq!(body["ids"], serde_json::json!(["a", "b"]));
    }

    /// `enqueue` forwards payload + optional dedupe_key/source and returns the
    /// API's response verbatim.
    #[test]
    fn enqueue_sends_payload_and_forwards_response() {
        let _g = env_lock();
        clear_env();
        let (port, captured) = spawn_stub_api_for_test(
            200,
            r#"{"enqueued":true,"deduped":false,"id":"abc","depth":3}"#,
        );
        let client = QueueClient::from_overrides(
            Some(&format!("http://127.0.0.1:{}", port)),
            Some("tok"),
        )
        .expect("client");
        let payload = serde_json::json!({"url": "https://y", "market": "M"});
        let resp = client
            .enqueue("q2", &payload, Some("y.com"), Some("session:s1"))
            .expect("enqueue ok");
        assert_eq!(resp["enqueued"], true);
        assert_eq!(resp["depth"], 3);
        let cap = captured.lock().unwrap();
        let (_, path) = cap.method_and_path();
        assert_eq!(path, "/queues/q2/items");
        let body: serde_json::Value =
            serde_json::from_str(cap.raw.split("\r\n\r\n").nth(1).unwrap_or("{}")).unwrap();
        assert_eq!(body["payload"]["url"], "https://y");
        assert_eq!(body["dedupe_key"], "y.com");
        assert_eq!(body["source"], "session:s1");
    }

    /// Non-2xx surfaces as ApiError with the status (4xx → InvalidParams via
    /// the shared `to_method_err`).
    #[test]
    fn api_4xx_maps_to_api_error() {
        let _g = env_lock();
        clear_env();
        let (port, _) = spawn_stub_api_for_test(400, r#"{"detail":"bad queue"}"#);
        let client = QueueClient::from_overrides(
            Some(&format!("http://127.0.0.1:{}", port)),
            Some("tok"),
        )
        .expect("client");
        let err = client.stats("q1").expect_err("must reject");
        match err {
            PlanningClientError::ApiError { status, .. } => assert_eq!(status, 400),
            other => panic!("expected ApiError, got {:?}", other),
        }
    }

    /// Missing creds fail with the planning client's MissingConfig diagnostics.
    #[test]
    fn missing_creds_fail_loudly() {
        let _g = env_lock();
        clear_env();
        let err = QueueClient::from_overrides(None, None).expect_err("must reject");
        assert!(matches!(err, PlanningClientError::MissingConfig("CM_API_URL")));
    }
}
