//! CM maintenance notice: the operator says "a deploy/restart is in
//! progress" so agents don't read a chat-sync drop as their own failure.
//!
//! `daemon.maintenance` (operator-only) sets or clears the notice on one
//! host. It lands in `<cm root>/maintenance.json`, which survives the brain
//! restart it announces. On the messaging hub the same RPC also publishes a
//! replicated `cm.maintenance` event, which every replica reduces into
//! `messages/main/MAINTENANCE.json`: the hub is the source of truth, and a
//! replica that then loses the hub keeps the last notice it saw. The newer of
//! the two records (by `changed_at`) wins, so a local clear beats an older
//! hub set and vice versa.
//!
//! A notice expires on its own [`GRACE_S`] after `expect_until`, so a
//! forgotten clear can't stick. Without a notice, a replica whose hub link is
//! down with a hub-restart-shaped error (or whose messaging is still
//! starting) reports a generic "hub restarting or unreachable" line instead,
//! never claiming a deploy.
//!
//! Agents see the line as a `maintenance` field on chat tool responses and
//! `ping`. Setting a notice also tells live agents that used chat in the last
//! hour, once per notice (the notification id is the notice id; re-setting
//! an active notice extends it and keeps its id).
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// How long past `expect_until` a notice stays before it expires on its own.
pub const GRACE_S: i64 = 15 * 60;
/// `expect_until` when the operator gives none.
pub const DEFAULT_EXPECT_S: i64 = 15 * 60;
/// Longest accepted reason, in characters.
pub const REASON_MAX_CHARS: usize = 120;
/// The replicated messaging event carrying a hub notice.
pub const EVENT_TYPE: &str = "cm.maintenance";
/// Agents that touched chat within this window get the one-time notice.
pub const RECENT_CHAT_S: i64 = 3600;
/// Must appear in the notification text (delivery is confirmed by finding
/// it in the agent's transcript).
pub const MARKER: &str = "[cm-maintenance]";

const SAVED: &str = "sends are saved locally and will replicate automatically, so don't resend. Cross-host reads may be stale.";

pub fn local_path(cm_root: &Path) -> PathBuf {
    cm_root.join("maintenance.json")
}

/// Where a replica projects the hub's notice.
pub fn projection_path(cm_root: &Path) -> PathBuf {
    cm_root.join("messages/main/MAINTENANCE.json")
}

fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn parse(v: &Value) -> Option<DateTime<Utc>> {
    v.as_str()
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc))
}

fn read(path: &Path) -> Value {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or(Value::Null)
}

fn write(path: &Path, record: &Value) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    crate::state::write_json_atomic(path, &record.to_string(), true)
}

/// Whether `record` is a notice still in force at `now`.
pub fn in_force(record: &Value, now: DateTime<Utc>) -> bool {
    record["active"] == true
        && parse(&record["expect_until"])
            .is_some_and(|until| now <= until + chrono::Duration::seconds(GRACE_S))
}

/// The newer of two records by `changed_at` (Null loses).
fn newer(a: Value, b: Value) -> Value {
    match (parse(&a["changed_at"]), parse(&b["changed_at"])) {
        (Some(x), Some(y)) if y > x => b,
        (Some(_), _) => a,
        (None, Some(_)) => b,
        (None, None) => Value::Null,
    }
}

/// The record that decides this host's notice: the newer of the local one
/// and the hub's projection.
pub fn record(cm_root: &Path) -> Value {
    newer(read(&local_path(cm_root)), read(&projection_path(cm_root)))
}

/// The notice in force at `now`, if any.
pub fn current(cm_root: &Path, now: DateTime<Utc>) -> Option<Value> {
    Some(record(cm_root)).filter(|r| in_force(r, now))
}

/// [`current`] at the wall clock (for callers without chrono).
pub fn current_now(cm_root: &Path) -> Option<Value> {
    current(cm_root, Utc::now())
}

/// Validate set params and build the active record. Re-setting while a
/// notice is in force keeps its id (and `started_at` unless given), so
/// agents are told once per maintenance window.
pub fn build_set(p: &Value, existing: &Value, now: DateTime<Utc>) -> Result<Value, String> {
    let reason = p["reason"].as_str().unwrap_or("CM redeploy").trim().to_owned();
    if reason.is_empty()
        || reason.chars().count() > REASON_MAX_CHARS
        || reason.chars().any(char::is_control)
    {
        return Err(format!("reason must be one line of 1–{REASON_MAX_CHARS} characters"));
    }
    let time = |key: &str| -> Result<Option<DateTime<Utc>>, String> {
        match &p[key] {
            Value::Null => Ok(None),
            v => parse(v).map(Some).ok_or_else(|| format!("{key} must be an RFC 3339 time")),
        }
    };
    let ongoing = in_force(existing, now);
    let started_at = time("started_at")?
        .or_else(|| ongoing.then(|| parse(&existing["started_at"])).flatten())
        .unwrap_or(now);
    let expect_until = time("expect_until")?
        .unwrap_or_else(|| now + chrono::Duration::seconds(DEFAULT_EXPECT_S));
    if expect_until <= started_at {
        return Err("expect_until must be after started_at".into());
    }
    let id = match existing["id"].as_str() {
        Some(id) if ongoing => id.to_owned(),
        _ => uuid::Uuid::new_v4().to_string(),
    };
    Ok(json!({
        "id": id,
        "active": true,
        "reason": reason,
        "started_at": rfc3339(started_at),
        "expect_until": rfc3339(expect_until),
        "changed_at": rfc3339(now),
    }))
}

pub fn build_clear(existing: &Value, now: DateTime<Utc>) -> Value {
    json!({"id": existing["id"], "active": false, "changed_at": rfc3339(now)})
}

/// `{action: "get" | "set" | "clear", reason?, started_at?, expect_until?}`
/// on this host's local record. Returns `(response, newly_set)`:
/// `newly_set` is the record when a set opened a new notice id.
pub fn apply_local(cm_root: &Path, p: &Value, now: DateTime<Utc>) -> Result<(Value, Option<Value>), String> {
    let existing = record(cm_root);
    match p["action"].as_str().unwrap_or("get") {
        "get" => Ok((exposure(cm_root, now), None)),
        "set" => {
            let rec = build_set(p, &existing, now)?;
            write(&local_path(cm_root), &rec).map_err(|e| format!("write maintenance notice: {e}"))?;
            let fresh = !(in_force(&existing, now) && existing["id"] == rec["id"]);
            Ok((exposure(cm_root, now), fresh.then_some(rec)))
        }
        "clear" => {
            write(&local_path(cm_root), &build_clear(&existing, now))
                .map_err(|e| format!("write maintenance notice: {e}"))?;
            Ok((exposure(cm_root, now), None))
        }
        _ => Err("action must be get, set or clear".into()),
    }
}

/// [`apply_local`] at the wall clock.
pub fn apply_local_now(cm_root: &Path, p: &Value) -> Result<(Value, Option<Value>), String> {
    apply_local(cm_root, p, Utc::now())
}

/// Reduce a replicated hub notice into the projection.
pub fn project(cm_root: &Path, data: &Value) -> std::io::Result<()> {
    write(&projection_path(cm_root), data)
}

/// The operator-facing state: `{active, notice?, message?}`.
pub fn exposure(cm_root: &Path, now: DateTime<Utc>) -> Value {
    match current(cm_root, now) {
        Some(n) => json!({"active": true, "notice": n, "message": describe(&n, now)}),
        None => json!({"active": false}),
    }
}

fn hhmm(t: Option<DateTime<Utc>>) -> String {
    t.map_or_else(|| "?".into(), |t| t.format("%H:%MZ").to_string())
}

/// The one line agents read for an active notice.
pub fn describe(notice: &Value, now: DateTime<Utc>) -> String {
    let reason = notice["reason"].as_str().unwrap_or("CM redeploy");
    let until = parse(&notice["expect_until"]);
    let over = if until.is_some_and(|u| now > u) { ", running over" } else { "" };
    format!(
        "{reason} in progress since {} (expected until ~{}{over}): {SAVED}",
        hhmm(parse(&notice["started_at"])),
        hhmm(until),
    )
}

/// The generic line when the hub is down or messaging is starting and no
/// notice says why.
pub fn hub_unreachable() -> String {
    format!("CM messaging hub is restarting or unreachable: {SAVED}")
}

/// What `messaging_starting` reads as to an agent.
pub fn messaging_starting() -> String {
    "CM messaging on this host is still starting, likely during a CM restart. Nothing was sent; retry the same request (same request_id) in about 30 seconds.".into()
}

/// Whether a replica's last sync error looks like the hub restarting.
pub fn hub_restart_signature(error: &str) -> bool {
    let e = error.to_ascii_lowercase();
    [
        "eof",
        "failed to fill whole buffer",
        "broken pipe",
        "coordinator_unavailable",
        "connection reset",
        "connection refused",
    ]
        .iter()
        .any(|s| e.contains(s))
}

/// The `maintenance` field for agent-facing responses, or None. `sync` is
/// the messaging sync status object (`{enabled, connected, role, error}`),
/// Null when unknown.
pub fn field(cm_root: &Path, sync: &Value, now: DateTime<Utc>) -> Option<String> {
    if let Some(n) = current(cm_root, now) {
        return Some(describe(&n, now));
    }
    let replica_down = sync["enabled"] == true
        && sync["role"] == "replica"
        && sync["connected"] == false
        && sync["error"].as_str().is_some_and(hub_restart_signature);
    replica_down.then(hub_unreachable)
}

/// The one-time notification text for a new notice.
pub fn announcement(notice: &Value, now: DateTime<Utc>) -> String {
    format!("{MARKER} {} This is informational; continue your task.", describe(notice, now))
}

/// The notification id for a notice: one per notice per session.
pub fn notification_id(notice: &Value) -> String {
    format!("cm-maintenance-{}", notice["id"].as_str().unwrap_or(""))
}

/// Tell each of `targets` once about `notice`. Returns how many were
/// queued (already-queued ones count; publish is idempotent per id).
pub fn announce(cm_root: &Path, notice: &Value, targets: &[String], now: DateTime<Utc>) -> usize {
    let text = announcement(notice, now);
    let id = notification_id(notice);
    targets
        .iter()
        .filter(|uid| match crate::notifications::get(cm_root, uid, &id) {
            Ok(Some(_)) => false,
            _ => match crate::notifications::publish(cm_root, uid, &id, "cm", &text, MARKER) {
                Ok(_) => true,
                Err(e) => {
                    eprintln!("cm maintenance: notify {uid}: {e}");
                    false
                }
            },
        })
        .count()
}

/// This host's own record (not the hub's projection).
pub fn local_record(cm_root: &Path) -> Value {
    read(&local_path(cm_root))
}

type State = std::sync::Arc<std::sync::Mutex<crate::state::DaemonState>>;

/// The `maintenance` field for this daemon right now: the notice in force,
/// else the generic hub line when the replica's hub link is down with a
/// restart-shaped error or local messaging is still starting. Takes the
/// daemon lock, then (without waiting) the store slot, never both at once.
pub fn field_for(state: &State) -> Option<String> {
    let (root, handle, opening) = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        (
            s.messaging_root.clone(),
            s.messaging.clone(),
            s.messaging_startup.load(std::sync::atomic::Ordering::Acquire),
        )
    };
    let now = Utc::now();
    if let Some(n) = current(&root, now) {
        return Some(describe(&n, now));
    }
    // No store yet: only "starting" once the opener runs (a daemon that
    // never starts messaging, e.g. in tests, says nothing).
    let starting = || opening.then(hub_unreachable);
    // Never wait on a busy store for this (ping must stay quick): a held
    // slot just skips the hub line this time.
    let sync = match handle.try_lock() {
        Ok(slot) => match slot.as_ref() {
            Some(store) => store.sync_status(),
            None => return starting(),
        },
        Err(std::sync::TryLockError::Poisoned(p)) => match p.into_inner().as_ref() {
            Some(store) => store.sync_status(),
            None => return starting(),
        },
        Err(std::sync::TryLockError::WouldBlock) => return None,
    };
    field(&root, &sync, now)
}

/// Note that `uid` just used chat.
pub fn stamp_chat(state: &State, uid: &str) {
    let mut s = state.lock().unwrap_or_else(|p| p.into_inner());
    s.last_chat_at.insert(uid.to_owned(), std::time::Instant::now());
}

/// Live agent sessions that used chat within [`RECENT_CHAT_S`].
pub fn announce_targets(state: &State) -> Vec<String> {
    let s = state.lock().unwrap_or_else(|p| p.into_inner());
    let window = std::time::Duration::from_secs(RECENT_CHAT_S as u64);
    let mut out: Vec<String> = s
        .sessions
        .iter()
        .filter(|(uid, sess)| {
            matches!(sess.session_type.as_str(), "claude-code" | "claude" | "codex")
                && s.last_chat_at.get(*uid).is_some_and(|t| t.elapsed() <= window)
        })
        .map(|(uid, _)| uid.clone())
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn a_notice_persists_on_disk_and_reads_back_after_a_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let now = at("2026-10-23T16:26:00Z");
        let (resp, fresh) = apply_local(
            tmp.path(),
            &json!({"action":"set","reason":"CM redeploy","expect_until":"2026-10-23T16:35:00Z"}),
            now,
        )
        .unwrap();
        assert_eq!(resp["active"], true);
        let fresh = fresh.expect("a new notice");
        // A "restart": nothing in memory, the file alone answers.
        let n = current(tmp.path(), at("2026-10-23T16:30:00Z")).expect("still in force");
        assert_eq!(n["id"], fresh["id"]);
        assert_eq!(
            describe(&n, at("2026-10-23T16:30:00Z")),
            "CM redeploy in progress since 16:26Z (expected until ~16:35Z): sends are saved locally and will replicate automatically, so don't resend. Cross-host reads may be stale."
        );
        // Re-setting while active extends it under the same id: no new announcement.
        let (_, again) = apply_local(
            tmp.path(),
            &json!({"action":"set","expect_until":"2026-10-23T16:45:00Z"}),
            at("2026-10-23T16:31:00Z"),
        )
        .unwrap();
        assert!(again.is_none());
        let n = current(tmp.path(), at("2026-10-23T16:31:00Z")).unwrap();
        assert_eq!((n["id"].clone(), n["started_at"].as_str()), (fresh["id"].clone(), Some("2026-10-23T16:26:00Z")));
        // Clear.
        apply_local(tmp.path(), &json!({"action":"clear"}), at("2026-10-23T16:33:00Z")).unwrap();
        assert!(current(tmp.path(), at("2026-10-23T16:33:00Z")).is_none());
        assert_eq!(exposure(tmp.path(), at("2026-10-23T16:33:00Z"))["active"], false);
        // Bad input.
        for bad in [json!({"action":"set","reason":""}), json!({"action":"set","expect_until":"soon"}),
                    json!({"action":"set","started_at":"2026-10-23T17:00:00Z","expect_until":"2026-10-23T16:00:00Z"}),
                    json!({"action":"pause"})] {
            assert!(apply_local(tmp.path(), &bad, now).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_forgotten_notice_expires_after_the_grace_period() {
        let tmp = tempfile::tempdir().unwrap();
        apply_local(tmp.path(), &json!({"action":"set","expect_until":"2026-10-23T16:35:00Z"}), at("2026-10-23T16:26:00Z")).unwrap();
        let over = at("2026-10-23T16:40:00Z");
        assert!(describe(&current(tmp.path(), over).unwrap(), over).contains("running over"));
        assert!(current(tmp.path(), at("2026-10-23T16:50:00Z")).is_some(), "inside the grace period");
        assert!(current(tmp.path(), at("2026-10-23T16:50:01Z")).is_none(), "expired");
        // A later set after expiry opens a new notice (and announces again).
        let (_, fresh) = apply_local(tmp.path(), &json!({"action":"set"}), at("2026-10-23T18:00:00Z")).unwrap();
        assert!(fresh.is_some());
    }

    #[test]
    fn the_newer_of_the_hub_projection_and_the_local_record_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let hub = json!({"id":"h","active":true,"reason":"CM redeploy","started_at":"2026-10-23T16:26:00Z",
                         "expect_until":"2026-10-23T16:35:00Z","changed_at":"2026-10-23T16:26:00Z"});
        project(tmp.path(), &hub).unwrap();
        let now = at("2026-10-23T16:28:00Z");
        assert_eq!(current(tmp.path(), now).unwrap()["id"], "h", "a replica shows the hub's notice");
        // A local clear after it wins; a newer hub set wins back.
        apply_local(tmp.path(), &json!({"action":"clear"}), at("2026-10-23T16:29:00Z")).unwrap();
        assert!(current(tmp.path(), at("2026-10-23T16:29:30Z")).is_none());
        let mut later = hub.clone();
        later["changed_at"] = json!("2026-10-23T16:30:00Z");
        project(tmp.path(), &later).unwrap();
        assert!(current(tmp.path(), at("2026-10-23T16:31:00Z")).is_some());
    }

    #[test]
    fn the_field_shows_a_notice_or_a_generic_hub_line_but_never_claims_a_deploy_without_one() {
        let tmp = tempfile::tempdir().unwrap();
        let now = at("2026-10-23T16:28:00Z");
        let down = |err: &str| json!({"enabled":true,"role":"replica","connected":false,"error":err});
        assert_eq!(field(tmp.path(), &Value::Null, now), None);
        for err in ["transport EOF", "storage_error: failed to fill whole buffer", "storage_error: Broken pipe (os error 32)",
                    "coordinator_unavailable: hub gone"] {
            let f = field(tmp.path(), &down(err), now).unwrap();
            assert!(f.starts_with("CM messaging hub is restarting or unreachable") && !f.contains("redeploy"), "{f}");
        }
        assert_eq!(field(tmp.path(), &down("enrollment revoked"), now), None);
        assert_eq!(field(tmp.path(), &json!({"enabled":true,"role":"replica","connected":true,"error":"eof"}), now), None);
        assert_eq!(field(tmp.path(), &json!({"enabled":true,"role":"coordinator","connected":false,"error":"eof"}), now), None);
        apply_local(tmp.path(), &json!({"action":"set","reason":"CM redeploy"}), now).unwrap();
        assert!(field(tmp.path(), &Value::Null, now).unwrap().starts_with("CM redeploy in progress since 16:28Z"));
    }

    #[test]
    fn each_session_is_told_once_per_notice() {
        let tmp = tempfile::tempdir().unwrap();
        let now = at("2026-10-23T16:28:00Z");
        let (_, n) = apply_local(tmp.path(), &json!({"action":"set"}), now).unwrap();
        let n = n.unwrap();
        let targets = vec!["ts-a".to_string(), "ts-b".to_string()];
        assert_eq!(announce(tmp.path(), &n, &targets, now), 2);
        assert_eq!(announce(tmp.path(), &n, &targets, at("2026-10-23T16:29:00Z")), 0, "already told");
        let queued = crate::notifications::get(tmp.path(), "ts-a", &notification_id(&n)).unwrap().unwrap();
        assert!(queued["text"].as_str().unwrap().starts_with(MARKER));
    }
}
