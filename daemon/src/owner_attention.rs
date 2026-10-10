//! Owner attention is separate from agent messaging and out-of-band escalation.
//! Persist before broadcasting; manifest.watch replays pending alerts on reconnect.
//!
//! Every request is gated by Owner's availability level
//! (`crate::owner_availability`): an alert whose urgency meets the level's bar
//! is delivered as before; a lower one is HELD in a separate file (old viewers
//! never see it, since `manifest.watch` snapshots only the delivered map) and
//! released when Owner becomes more reachable. Unset delivers everything.
//! `escalate()`/`withdraw()` is the one interface daemon-side sources (the
//! notification stall alarm, the outbox alarm, the work-item board) use.
use crate::control::protocol::{Caller, ErrorCode, Request, Response};
use crate::manifest::ManifestDiff;
use crate::owner_availability::{self as availability, Level, Urgency};
use crate::state::DaemonState;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Alert {
    pub id: String,
    pub session_uid: String,
    pub label: String,
    pub message: String,
    pub task_id: Option<String>,
    pub continuous_task_id: Option<String>,
    /// `fyi` / `decision` / `blocking` / `emergency`; absent on legacy alerts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub urgency: Option<String>,
    /// When the request was held because Owner's level was below its urgency.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_since: Option<String>,
    /// Set when a level change released a held request into delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released_at: Option<String>,
}

/// A held request: the latest per key, plus how many were held in total.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Held {
    pub alert: Alert,
    pub count: u32,
}

const MAX_ENTRIES: usize = 4096;
const MAX_BYTES: usize = 512 * 1024;
const MAX_MESSAGE: usize = 4096;
/// Minimum spacing of `notify_command` pushes per alert key.
const PUSH_INTERVAL_SECS: u64 = 600;

fn path(state: &DaemonState) -> PathBuf {
    state
        .daemon_sessions_path
        .clone()
        .unwrap_or_else(crate::state::default_daemon_sessions_path)
        .with_file_name("owner-attention.json")
}

fn held_path(state: &DaemonState) -> PathBuf {
    path(state).with_file_name("owner-attention-held.json")
}

pub fn snapshot(state: &DaemonState) -> Result<BTreeMap<String, Alert>, String> {
    match std::fs::read(path(state)) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("read Owner alerts: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(format!("read Owner alerts: {e}")),
    }
}

pub fn held_snapshot(state: &DaemonState) -> Result<BTreeMap<String, Held>, String> {
    match std::fs::read(held_path(state)) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("read held Owner requests: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(format!("read held Owner requests: {e}")),
    }
}

fn save(state: &DaemonState, alerts: &BTreeMap<String, Alert>) -> Result<(), String> {
    let json = serde_json::to_string(alerts).unwrap();
    if json.len() > MAX_BYTES {
        return Err("Owner alert queue is full; pending alerts were retained".into());
    }
    crate::state::write_json_atomic(&path(state), &json, true)
        .map_err(|e| format!("persist Owner alerts: {e}"))
}

fn save_held(state: &DaemonState, held: &BTreeMap<String, Held>) -> Result<(), String> {
    let json = serde_json::to_string(held).unwrap();
    if json.len() > MAX_BYTES {
        return Err("Held Owner request store is full; existing requests were retained".into());
    }
    crate::state::write_json_atomic(&held_path(state), &json, true)
        .map_err(|e| format!("persist held Owner requests: {e}"))
}

fn truncate(message: &str) -> String {
    if message.len() <= MAX_MESSAGE {
        return message.into();
    }
    let mut end = MAX_MESSAGE;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message[..end].into()
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn level_now(state: &DaemonState) -> Option<Level> {
    availability::current(&state.messaging_root).0
}

fn broadcast(state: &DaemonState, key: &str, alert: Option<&Alert>) {
    state.manifest_watcher.broadcast(ManifestDiff::Updated {
        uid: key.to_owned(),
        entry: json!({"owner_attention": alert}),
    });
}

/// Out-of-band push (`notify_command`, e.g. Telegram) for an emergency, or —
/// once Owner has set an availability level — for a delivered blocking
/// request when no viewer is connected to show it. While the level is unset
/// nothing but emergencies pushes, so behavior is unchanged until Owner opts
/// in. Rate-limited per key so a misused `emergency` cannot spam the phone.
fn maybe_push(state: &DaemonState, key: &str, alert: &Alert, urgency: Urgency, delivered: bool) {
    let viewer = state.manifest_watcher.subscriber_count() > 0;
    let level_set = level_now(state).is_some();
    if !(urgency == Urgency::Emergency || level_set && delivered && urgency >= Urgency::Blocking && !viewer) {
        return;
    }
    static LAST: std::sync::Mutex<BTreeMap<String, u64>> = std::sync::Mutex::new(BTreeMap::new());
    let now = crate::continuous::task::now_unix();
    {
        let mut last = LAST.lock().unwrap_or_else(|p| p.into_inner());
        if last.get(key).is_some_and(|t| now.saturating_sub(*t) < PUSH_INTERVAL_SECS) {
            eprintln!("cm owner attention: push for {key} rate-limited");
            return;
        }
        last.insert(key.to_owned(), now);
    }
    crate::notify::notify_operator(
        state.config.notify_command.as_deref(),
        "owner-attention",
        &format!("[{}] {}: {}", urgency.as_str(), alert.label, alert.message),
    );
}

/// What happened to one gated request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GateOutcome {
    Delivered { alert_id: String },
    Held { alert_id: String, release_when: Level },
    /// An alert is already pending under this key (an agent's own request is
    /// never displaced); retry after Owner acknowledges it.
    Coalesced { alert_id: String },
}

/// A daemon-side request for Owner attention.
#[derive(Clone, Debug)]
pub struct Escalation {
    /// Who raised it: `stall:<uid>`, `outbox:<uid>`, `board:<board_id>`, ...
    pub source: String,
    /// Map key: the session uid when tied to a session (the TUI attaches the
    /// alert to that row), else the source.
    pub dedupe_key: String,
    pub urgency: Urgency,
    pub summary: String,
    pub session_uid: Option<String>,
    pub task_id: Option<String>,
}

fn session_context(state: &DaemonState, uid: &str) -> (String, Option<String>, Option<String>) {
    if let Some(s) = state.sessions.get(uid) {
        (s.title.clone(), s.task_id.clone(), s.continuous_task_id.clone())
    } else {
        let tui = state.tui_sessions.get(uid);
        (
            tui.and_then(|s| s.label.clone()).unwrap_or_else(|| uid.to_owned()),
            tui.and_then(|s| s.task_id.clone()),
            None,
        )
    }
}

/// Deliver or hold `alert` under `key` against the current level. A daemon
/// escalation (`replace = false`) never displaces a pending alert; an agent's
/// own `notify_user` (`replace = true`) replaces its earlier pending one, as
/// it always has.
fn gate(
    state: &DaemonState,
    key: &str,
    mut alert: Alert,
    urgency: Urgency,
    replace: bool,
) -> Result<GateOutcome, String> {
    let mut alerts = snapshot(state)?;
    if let Some(old) = alerts.get(key).filter(|_| !replace) {
        return Ok(GateOutcome::Coalesced { alert_id: old.id.clone() });
    }
    alert.urgency = Some(urgency.as_str().into());
    if availability::delivers(level_now(state), urgency) {
        if alerts.len() >= MAX_ENTRIES && !alerts.contains_key(key) {
            return Err("Owner alert queue is full; pending alerts were retained".into());
        }
        alerts.insert(key.to_owned(), alert.clone());
        save(state, &alerts)?;
        broadcast(state, key, Some(&alert));
        maybe_push(state, key, &alert, urgency, true);
        return Ok(GateOutcome::Delivered { alert_id: alert.id });
    }
    let mut held = held_snapshot(state)?;
    if let Some(h) = held.get(key).filter(|h| h.alert.message == alert.message) {
        return Ok(GateOutcome::Held {
            alert_id: h.alert.id.clone(),
            release_when: availability::release_level(urgency),
        });
    }
    if held.len() >= MAX_ENTRIES && !held.contains_key(key) {
        return Err("Held Owner request store is full; existing requests were retained".into());
    }
    alert.held_since = Some(now_rfc3339());
    let count = held.get(key).map_or(1, |h| h.count.saturating_add(1));
    // Keep the most urgent pending urgency for the key, so a later fyi never
    // downgrades a held blocking request.
    if let Some(prev) = held.get(key).and_then(|h| h.alert.urgency.as_deref()).and_then(Urgency::parse) {
        if prev > urgency {
            alert.urgency = Some(prev.as_str().into());
        }
    }
    let id = alert.id.clone();
    held.insert(key.to_owned(), Held { alert, count });
    save_held(state, &held)?;
    maybe_push(state, key, &held[key].alert, urgency, false);
    Ok(GateOutcome::Held { alert_id: id, release_when: availability::release_level(urgency) })
}

/// The shared escalation interface (stall alarm, outbox alarm, board).
pub fn escalate(state: &DaemonState, e: Escalation) -> Result<GateOutcome, String> {
    let (label, task_id, continuous_task_id) = match &e.session_uid {
        Some(uid) => session_context(state, uid),
        None => (e.source.clone(), None, None),
    };
    let alert = Alert {
        id: uuid::Uuid::new_v4().to_string(),
        session_uid: e.session_uid.clone().unwrap_or_else(|| e.dedupe_key.clone()),
        label,
        message: truncate(&e.summary),
        task_id: e.task_id.or(task_id),
        continuous_task_id,
        ..Alert::default()
    };
    gate(state, &e.dedupe_key, alert, e.urgency, false)
}

/// Withdraw an escalation once its condition cleared, delivered or held. The
/// ID comparison leaves any newer alert (including an agent's own) in place.
pub fn withdraw(state: &DaemonState, key: &str, alert_id: &str) -> Result<bool, String> {
    let mut alerts = snapshot(state)?;
    if alerts.get(key).is_some_and(|a| a.id == alert_id) {
        alerts.remove(key);
        save(state, &alerts)?;
        broadcast(state, key, None);
        return Ok(true);
    }
    let mut held = held_snapshot(state)?;
    if held.get(key).is_some_and(|h| h.alert.id == alert_id) {
        held.remove(key);
        save_held(state, &held)?;
        return Ok(true);
    }
    Ok(false)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NotifyParams {
    #[serde(default)]
    message: String,
    #[serde(default)]
    urgency: Option<String>,
}

fn availability_json(state: &DaemonState) -> serde_json::Value {
    availability::exposure(&state.messaging_root)
}

// Called under the daemon state lock and the ordinary restart mutation barrier.
pub fn notify(state: &DaemonState, req: &Request) -> Response {
    let Caller::Session(caller) = &req.caller else {
        return Response::err(
            req.id.clone(),
            ErrorCode::Unauthorized,
            "notify_user requires a live Session caller",
        );
    };
    let uid = &caller.session_uid;
    let Some(view) = state.lookup_session_any(uid) else {
        return Response::err(
            req.id.clone(),
            ErrorCode::Unauthorized,
            "notify_user caller is not a live session",
        );
    };
    if state
        .sessions
        .get(uid)
        .is_some_and(|s| s.last_exit.kernel_set())
    {
        return Response::err(
            req.id.clone(),
            ErrorCode::Unauthorized,
            "notify_user caller has exited",
        );
    }
    let params: NotifyParams = match serde_json::from_value(req.params.clone()) {
        Ok(p) => p,
        Err(e) => return Response::err(req.id.clone(), ErrorCode::InvalidParams, e.to_string()),
    };
    if params.message.len() > MAX_MESSAGE {
        return Response::err(
            req.id.clone(),
            ErrorCode::InvalidParams,
            "message must be at most 4096 UTF-8 bytes",
        );
    }
    let urgency = match params.urgency.as_deref().unwrap_or("decision") {
        u => match Urgency::parse(u) {
            Some(u) => u,
            None => {
                return Response::err(
                    req.id.clone(),
                    ErrorCode::InvalidParams,
                    "urgency must be fyi, decision, blocking or emergency",
                )
            }
        },
    };
    let (label, _, continuous_task_id) = session_context(state, uid);
    // Collapse retries of the same pending request. A changed reason re-alerts;
    // after Owner acknowledges, the same reason can raise a new alert.
    let alerts = match snapshot(state) {
        Ok(a) => a,
        Err(e) => return Response::err(req.id.clone(), ErrorCode::Internal, e),
    };
    if let Some(old) = alerts.get(uid).filter(|a| a.message == params.message) {
        return Response::ok(
            req.id.clone(),
            json!({"ok": true, "status": "queued", "delivery": "immediate", "alert_id": old.id,
                   "urgency": old.urgency.clone().unwrap_or_else(|| urgency.as_str().into()),
                   "owner_availability": availability_json(state)}),
        );
    }
    let alert = Alert {
        id: uuid::Uuid::new_v4().to_string(),
        session_uid: uid.clone(),
        label,
        message: params.message,
        task_id: view.task_id,
        continuous_task_id,
        ..Alert::default()
    };
    let outcome = match gate(state, uid, alert, urgency, true) {
        Ok(o) => o,
        Err(e) => return Response::err(req.id.clone(), ErrorCode::Internal, e),
    };
    let result = match outcome {
        GateOutcome::Delivered { alert_id } | GateOutcome::Coalesced { alert_id } => json!({
            "ok": true, "status": "queued", "delivery": "immediate", "alert_id": alert_id,
            "urgency": urgency.as_str(), "owner_availability": availability_json(state),
        }),
        GateOutcome::Held { alert_id, release_when } => json!({
            "ok": true, "status": "queued", "delivery": "held", "alert_id": alert_id,
            "urgency": urgency.as_str(), "release_when": release_when.as_str(),
            "owner_availability": availability_json(state),
            "note": format!("Owner's availability is below this urgency, so the request is held, not lost. Keep working; you will be woken when Owner reaches '{}' or above and it is delivered.", release_when.as_str()),
        }),
    };
    Response::ok(req.id.clone(), result)
}

/// Daemon-originated alert for `uid` (e.g. a stalled native notification
/// path, where the agent itself cannot be relied on to call notify_user).
/// A blocking escalation keyed by the session. Never displaces a pending
/// alert: returns `Ok(None)` while one exists so the caller can retry.
pub fn raise_system(state: &DaemonState, uid: &str, message: &str) -> Result<Option<String>, String> {
    let e = Escalation {
        source: format!("system:{uid}"),
        dedupe_key: uid.to_owned(),
        urgency: Urgency::Blocking,
        summary: message.to_owned(),
        session_uid: Some(uid.to_owned()),
        task_id: None,
    };
    Ok(match escalate(state, e)? {
        GateOutcome::Delivered { alert_id } | GateOutcome::Held { alert_id, .. } => Some(alert_id),
        GateOutcome::Coalesced { .. } => None,
    })
}

/// Withdraw a daemon-originated alert once its condition cleared.
pub fn clear_system(state: &DaemonState, uid: &str, alert_id: &str) -> Result<bool, String> {
    withdraw(state, uid, alert_id)
}

/// Per-key result of a level change: requests released into delivery, and
/// requests still held.
#[derive(Default, Debug)]
pub struct ReleaseOutcome {
    pub released: BTreeMap<String, usize>,
    pub still_held: BTreeMap<String, usize>,
}

/// Move held requests the level now delivers into the delivered map, marked
/// `released_at`. A key with a pending alert gets the held text merged in (a
/// new alert ID, so the viewer notices) up to the 4096-byte limit.
pub fn release_for_level(state: &DaemonState, level: Option<Level>) -> Result<ReleaseOutcome, String> {
    let mut held = held_snapshot(state)?;
    let mut out = ReleaseOutcome::default();
    if held.is_empty() {
        return Ok(out);
    }
    let mut alerts = snapshot(state)?;
    let now = now_rfc3339();
    let mut changed = Vec::new();
    held.retain(|key, h| {
        let urgency = h.alert.urgency.as_deref().and_then(Urgency::parse).unwrap_or(Urgency::Decision);
        if !availability::delivers(level, urgency) {
            out.still_held.insert(h.alert.session_uid.clone(), h.count as usize);
            return true;
        }
        let mut released = h.alert.clone();
        released.released_at = Some(now.clone());
        if h.count > 1 {
            released.message = truncate(&format!("{} ({} requests held; this is the latest)", released.message, h.count));
        }
        let alert = match alerts.get(key) {
            // A newer delivered alert keeps its identity (so its raiser can
            // still withdraw it) and the higher urgency; the released text is
            // appended to it, never written over it.
            Some(pending) => {
                let mut merged = pending.clone();
                merged.message = truncate(&format!("{}\n\n[released] {}", pending.message, released.message));
                merged.released_at = Some(now.clone());
                let rank = |a: &Alert| a.urgency.as_deref().and_then(Urgency::parse).unwrap_or(Urgency::Decision);
                if rank(&released) > rank(pending) {
                    merged.urgency = released.urgency.clone();
                }
                merged
            }
            None => released,
        };
        out.released.insert(alert.session_uid.clone(), h.count as usize);
        alerts.insert(key.clone(), alert);
        changed.push(key.clone());
        false
    });
    if changed.is_empty() {
        return Ok(out);
    }
    save(state, &alerts)?;
    save_held(state, &held)?;
    for key in &changed {
        broadcast(state, key, alerts.get(key));
        let urgency = alerts[key].urgency.as_deref().and_then(Urgency::parse).unwrap_or(Urgency::Decision);
        maybe_push(state, key, &alerts[key], urgency, true);
    }
    Ok(out)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EscalateParams {
    source: String,
    dedupe_key: Option<String>,
    urgency: String,
    summary: String,
    session_uid: Option<String>,
    task_id: Option<String>,
}

/// `owner_attention.escalate` (Operator-only, gated in dispatch): lets an
/// evaluator outside this daemon (the planning API's board, another host)
/// use the same gate.
pub fn escalate_rpc(state: &DaemonState, req: &Request) -> Response {
    let p: EscalateParams = match serde_json::from_value(req.params.clone()) {
        Ok(p) => p,
        Err(e) => return Response::err(req.id.clone(), ErrorCode::InvalidParams, e.to_string()),
    };
    let Some(urgency) = Urgency::parse(&p.urgency) else {
        return Response::err(req.id.clone(), ErrorCode::InvalidParams, "unknown urgency");
    };
    if p.source.is_empty() || p.summary.is_empty() {
        return Response::err(req.id.clone(), ErrorCode::InvalidParams, "source and summary are required");
    }
    let e = Escalation {
        dedupe_key: p.dedupe_key.unwrap_or_else(|| p.source.clone()),
        source: p.source,
        urgency,
        summary: p.summary,
        session_uid: p.session_uid,
        task_id: p.task_id,
    };
    match escalate(state, e) {
        Ok(o) => Response::ok(req.id.clone(), outcome_json(&o)),
        Err(e) => Response::err(req.id.clone(), ErrorCode::Internal, e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WithdrawParams {
    dedupe_key: String,
    alert_id: String,
}

/// `owner_attention.withdraw` (Operator-only, gated in dispatch).
pub fn withdraw_rpc(state: &DaemonState, req: &Request) -> Response {
    let p: WithdrawParams = match serde_json::from_value(req.params.clone()) {
        Ok(p) => p,
        Err(e) => return Response::err(req.id.clone(), ErrorCode::InvalidParams, e.to_string()),
    };
    match withdraw(state, &p.dedupe_key, &p.alert_id) {
        Ok(w) => Response::ok(req.id.clone(), json!({"ok": true, "withdrawn": w})),
        Err(e) => Response::err(req.id.clone(), ErrorCode::Internal, e),
    }
}

fn outcome_json(o: &GateOutcome) -> serde_json::Value {
    match o {
        GateOutcome::Delivered { alert_id } => json!({"delivery":"immediate","alert_id":alert_id}),
        GateOutcome::Held { alert_id, release_when } => {
            json!({"delivery":"held","alert_id":alert_id,"release_when":release_when.as_str()})
        }
        GateOutcome::Coalesced { alert_id } => json!({"delivery":"coalesced","alert_id":alert_id}),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AckParams {
    session_uid: String,
    alert_id: String,
}

/// Operator gate is enforced in dispatch. Compare IDs so a delayed acknowledgement
/// cannot delete a newer request from the same session.
pub fn acknowledge(state: &DaemonState, req: &Request) -> Response {
    let params: AckParams = match serde_json::from_value(req.params.clone()) {
        Ok(p) => p,
        Err(e) => return Response::err(req.id.clone(), ErrorCode::InvalidParams, e.to_string()),
    };
    let mut alerts = match snapshot(state) {
        Ok(a) => a,
        Err(e) => return Response::err(req.id.clone(), ErrorCode::Internal, e),
    };
    let matched = alerts
        .get(&params.session_uid)
        .is_some_and(|a| a.id == params.alert_id);
    if matched {
        alerts.remove(&params.session_uid);
        if let Err(e) = save(state, &alerts) {
            return Response::err(req.id.clone(), ErrorCode::Internal, e);
        }
        state.manifest_watcher.broadcast(ManifestDiff::Updated {
            uid: params.session_uid,
            entry: json!({"owner_attention": null}),
        });
    }
    Response::ok(
        req.id.clone(),
        json!({"ok": true, "acknowledged": matched}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::dispatch::{dispatch_request, DispatchOutcome};
    use crate::session::{DaemonSession, SpawnParams};
    use std::sync::{Arc, Mutex};

    fn fixture() -> (tempfile::TempDir, Arc<Mutex<DaemonState>>) {
        let dir = tempfile::tempdir().unwrap();
        let mut state = DaemonState::new();
        state.daemon_sessions_path = Some(dir.path().join("daemon-sessions.json"));
        state.messaging_root = dir.path().to_path_buf();
        (dir, Arc::new(Mutex::new(state)))
    }
    /// Write the availability projection the way the messaging store does.
    fn set_level(state: &Arc<Mutex<DaemonState>>, level: Option<&str>, event: &str) {
        let root = state.lock().unwrap().messaging_root.clone();
        let path = crate::owner_availability::projection_path(&root);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::json!({"level":level,"event_id":event,"changed_at":"2026-10-06T14:00:00Z"}).to_string()).unwrap();
    }
    fn notify_with(state: &Arc<Mutex<DaemonState>>, uid: &str, message: &str, urgency: &str) -> serde_json::Value {
        let r = dispatch_request(
            state,
            &request("notify_user", Caller::session(uid), serde_json::json!({"message": message, "urgency": urgency})),
        )
        .into_response();
        assert!(r.ok, "{:?}", r.error);
        r.result.unwrap()
    }
    fn held(state: &Arc<Mutex<DaemonState>>) -> BTreeMap<String, Held> {
        held_snapshot(&state.lock().unwrap()).unwrap()
    }
    #[test]
    fn gate_matrix_delivers_or_holds_per_level_and_urgency() {
        let _guard = crate::test_support::env_lock();
        let (_dir, state) = fixture();
        local(&state);
        for (level, delivered) in [
            (None, ["fyi", "decision", "blocking", "emergency"].as_slice()),
            (Some("on-call"), &["fyi", "decision", "blocking", "emergency"]),
            (Some("focused"), &["fyi", "decision", "blocking", "emergency"]),
            (Some("around"), &["decision", "blocking", "emergency"]),
            (Some("away"), &["emergency"]),
        ] {
            set_level(&state, level, "rev");
            for urgency in ["fyi", "decision", "blocking", "emergency"] {
                let p = path(&state.lock().unwrap());
                let _ = std::fs::remove_file(&p);
                let _ = std::fs::remove_file(p.with_file_name("owner-attention-held.json"));
                let r = notify_with(&state, "local", &format!("{level:?} {urgency}"), urgency);
                let expect = if delivered.contains(&urgency) { "immediate" } else { "held" };
                assert_eq!(r["delivery"], expect, "{level:?} {urgency}");
                assert_eq!(r["urgency"], urgency);
                assert_eq!(r["status"], "queued");
                if expect == "held" {
                    assert!(r["release_when"].is_string());
                    assert!(snapshot(&state.lock().unwrap()).unwrap().is_empty(), "held alerts never reach the viewer map");
                    assert_eq!(held(&state)["local"].alert.message, format!("{level:?} {urgency}"));
                }
            }
        }
        // Default urgency is decision; an unknown one is refused.
        set_level(&state, Some("focused"), "rev2");
        let r = dispatch_request(&state, &request("notify_user", Caller::session("local"), serde_json::json!({"message":"x"}))).into_response();
        assert_eq!(r.result.unwrap()["urgency"], "decision");
        let r = dispatch_request(&state, &request("notify_user", Caller::session("local"), serde_json::json!({"message":"x","urgency":"asap"}))).into_response();
        assert_eq!(r.error.unwrap().code, ErrorCode::InvalidParams);
    }
    #[test]
    fn held_requests_coalesce_count_and_release_merged_on_a_level_change() {
        let _guard = crate::test_support::env_lock();
        let (_dir, state) = fixture();
        local(&state);
        set_level(&state, Some("away"), "r1");
        let first = notify_with(&state, "local", "pick a schema", "decision");
        assert_eq!(notify_with(&state, "local", "pick a schema", "decision")["alert_id"], first["alert_id"]);
        notify_with(&state, "local", "also: rename?", "fyi");
        let h = held(&state);
        assert_eq!(h["local"].count, 2);
        assert_eq!(h["local"].alert.urgency.as_deref(), Some("decision"), "a later fyi never downgrades");
        // An emergency still gets through, and stays pending for the merge.
        let emergency = notify_with(&state, "local", "prod down", "emergency");
        // away: decision still held.
        let out = release_for_level(&state.lock().unwrap(), Some(Level::Away)).unwrap();
        assert!(out.released.is_empty());
        assert_eq!(out.still_held["local"], 2);
        // around: released into the delivered map, merged with the pending one.
        let out = release_for_level(&state.lock().unwrap(), Some(Level::Around)).unwrap();
        assert_eq!(out.released["local"], 2);
        let alerts = snapshot(&state.lock().unwrap()).unwrap();
        let a = &alerts["local"];
        assert!(a.released_at.is_some());
        assert_eq!(a.urgency.as_deref(), Some("emergency"), "the newer, higher urgency is kept");
        assert_eq!(serde_json::json!(a.id), emergency["alert_id"], "a merge keeps the pending alert's id");
        assert!(a.message.starts_with("prod down") && a.message.contains("[released] also: rename?"), "{}", a.message);
        assert!(a.message.contains("2 requests held"));
        assert!(held(&state).is_empty());
    }
    #[test]
    fn escalate_holds_coalesces_and_withdraws() {
        let _guard = crate::test_support::env_lock();
        let (_dir, state) = fixture();
        local(&state);
        let esc = |urgency| Escalation {
            source: "board:b1".into(),
            dedupe_key: "board:b1".into(),
            urgency,
            summary: "3 unresolved flags".into(),
            session_uid: None,
            task_id: Some("t".into()),
        };
        set_level(&state, Some("away"), "r1");
        let s = state.lock().unwrap();
        let GateOutcome::Held { alert_id, release_when } = escalate(&s, esc(Urgency::Blocking)).unwrap() else { panic!() };
        assert_eq!(release_when, Level::Around);
        assert!(withdraw(&s, "board:b1", &alert_id).unwrap());
        assert!(held_snapshot(&s).unwrap().is_empty());
        drop(s);
        set_level(&state, None, "r2");
        let s = state.lock().unwrap();
        let GateOutcome::Delivered { alert_id } = escalate(&s, esc(Urgency::Decision)).unwrap() else { panic!() };
        assert_eq!(snapshot(&s).unwrap()["board:b1"].label, "board:b1");
        assert_eq!(escalate(&s, esc(Urgency::Blocking)).unwrap(), GateOutcome::Coalesced { alert_id: alert_id.clone() });
        assert!(!withdraw(&s, "board:b1", "other").unwrap());
        assert!(withdraw(&s, "board:b1", &alert_id).unwrap());
        drop(s);
        // An agent's pending request is never displaced by a daemon escalation.
        notify_with(&state, "local", "mine", "decision");
        let s = state.lock().unwrap();
        assert_eq!(raise_system(&s, "local", "stall").unwrap(), None);
        assert_eq!(snapshot(&s).unwrap()["local"].message, "mine");
    }
    #[test]
    fn emergencies_and_unwatched_blocking_requests_push_out_of_band() {
        let _guard = crate::test_support::env_lock();
        let (dir, state) = fixture();
        local(&state);
        let out = dir.path().join("pushes.txt");
        let script = dir.path().join("notify.sh");
        std::fs::write(&script, format!("#!/bin/sh\necho \"$CM_NOTIFY_TAG|$1\" >> '{}'\n", out.display())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        state.lock().unwrap().config.notify_command = Some(script.display().to_string());
        let pushes = || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            std::fs::read_to_string(&out).unwrap_or_default()
        };
        // Unique session keys: the per-key rate limit is process-wide.
        state.lock().unwrap().tui_sessions.insert("push-a".into(),
            serde_json::from_value(serde_json::json!({"uid":"push-a","label":"Pusher"})).unwrap());
        notify_with(&state, "push-a", "just fyi", "decision");
        assert_eq!(pushes(), "", "decision with no viewer stays in the TUI queue");
        set_level(&state, Some("away"), "r1");
        notify_with(&state, "push-a", "prod down", "emergency");
        let got = pushes();
        assert!(got.contains("owner-attention|[emergency] Pusher: prod down"), "{got}");
        // Rate limited per key.
        notify_with(&state, "push-a", "still down", "emergency");
        assert_eq!(pushes().lines().count(), 1);
        for uid in ["push-b", "push-c"] {
            state.lock().unwrap().tui_sessions.insert(uid.into(),
                serde_json::from_value(serde_json::json!({"uid":uid,"label":"Blocker"})).unwrap());
        }
        // Unset level: no new pushes beyond emergencies (behavior unchanged).
        set_level(&state, None, "r2");
        notify_with(&state, "push-b", "need creds", "blocking");
        assert!(!pushes().contains("need creds"));
        // Once Owner set a level, an unwatched delivered blocking request pushes.
        set_level(&state, Some("on-call"), "r3");
        notify_with(&state, "push-c", "need keys", "blocking");
        assert!(pushes().contains("[blocking] Blocker: need keys"));
    }
    #[test]
    fn level_change_releases_and_wakes_holders_once_per_revision() {
        let _guard = crate::test_support::env_lock();
        let (dir, state) = fixture();
        cloud(&state, false);
        set_level(&state, Some("away"), "r1");
        // First run adopts the current level silently.
        crate::owner_availability::tick(&state);
        let root = dir.path().to_path_buf();
        let wakes = || crate::notifications::snapshot(&root, "cloud").map(|(e, _)| e).unwrap_or_default();
        assert!(wakes().is_empty());
        let r = notify_with(&state, "cloud", "which region?", "decision");
        assert_eq!(r["delivery"], "held");
        set_level(&state, Some("focused"), "r2");
        crate::owner_availability::tick(&state);
        assert_eq!(snapshot(&state.lock().unwrap()).unwrap()["cloud"].message, "which region?");
        let events = wakes();
        assert_eq!(events.len(), 1);
        let text = events[0]["text"].as_str().unwrap();
        assert!(text.contains("away→focused") && text.contains("1 of your requests were released"), "{text}");
        // The marker is in the text, so the consumer can confirm delivery
        // (without it the notice stayed `submitted` and paged Owner).
        let marker = events[0]["marker"].as_str().unwrap();
        assert!(marker.starts_with("[cm-owner-availability ") && text.starts_with(marker), "{text}");
        // Unchanged revision: no second wake.
        crate::owner_availability::tick(&state);
        assert_eq!(wakes().len(), 1);
        // Re-setting the same level is a new revision but no change: no wake.
        set_level(&state, Some("focused"), "r3");
        crate::owner_availability::tick(&state);
        assert_eq!(wakes().len(), 1);
        // A real change with nothing held or released wakes nobody: agents
        // read the level from ping() when they need it.
        set_level(&state, Some("on-call"), "r4");
        crate::owner_availability::tick(&state);
        assert_eq!(wakes().len(), 1);
    }
    fn request(method: &str, caller: Caller, params: serde_json::Value) -> Request {
        Request {
            id: "request".into(),
            method: method.into(),
            caller,
            params,
        }
    }
    fn publish(state: &Arc<Mutex<DaemonState>>, uid: &str, message: &str) -> Response {
        dispatch_request(
            state,
            &request(
                "notify_user",
                Caller::session(uid),
                serde_json::json!({"message": message}),
            ),
        )
        .into_response()
    }
    fn local(state: &Arc<Mutex<DaemonState>>) {
        state.lock().unwrap().tui_sessions.insert(
            "local".into(),
            serde_json::from_value(serde_json::json!({"uid":"local", "label":"Local task"}))
                .unwrap(),
        );
    }
    fn cloud(state: &Arc<Mutex<DaemonState>>, continuous: bool) {
        let mut params = SpawnParams::new("cloud", "Cloud task", "/bin/sleep");
        params.args = vec!["30".into()];
        params.task_id = Some("task".into());
        if continuous {
            params.continuous_task_id = Some("orchestrator".into());
        }
        state
            .lock()
            .unwrap()
            .sessions
            .insert("cloud".into(), DaemonSession::spawn(params).unwrap());
    }
    #[test]
    fn owner_attention_local_cloud_and_continuous_without_global_permissions() {
        let _guard = crate::test_support::env_lock();
        for continuous in [false, true] {
            let (_dir, state) = fixture();
            local(&state);
            cloud(&state, continuous);
            for uid in ["cloud", "local"] {
                assert!(publish(&state, uid, "Ready for review").ok);
            }
            let alerts = snapshot(&state.lock().unwrap()).unwrap();
            assert_eq!(alerts["local"].label, "Local task");
            assert_eq!(alerts["cloud"].task_id.as_deref(), Some("task"));
            assert_eq!(alerts["cloud"].continuous_task_id.is_some(), continuous);
        }
    }
    #[test]
    fn owner_attention_rejects_unknown_exited_operator_and_spoofed_targets() {
        let _guard = crate::test_support::env_lock();
        let (_dir, state) = fixture();
        local(&state);
        cloud(&state, false);
        assert_eq!(
            publish(&state, "unknown", "x").error.unwrap().code,
            ErrorCode::Unauthorized
        );
        state.lock().unwrap().sessions["cloud"]
            .last_exit
            .set_kernel(crate::session::KernelExitStatus {
                code: Some(0),
                signal: None,
            });
        assert_eq!(
            publish(&state, "cloud", "x").error.unwrap().code,
            ErrorCode::Unauthorized
        );
        for caller in [Caller::operator("forged"), Caller::session("local")] {
            let response = dispatch_request(
                &state,
                &request(
                    "notify_user",
                    caller,
                    serde_json::json!({"message":"x","session_uid":"victim"}),
                ),
            )
            .into_response();
            assert!(!response.ok);
        }
        assert!(!publish(&state, "local", &"x".repeat(4097)).ok);
        assert!(snapshot(&state.lock().unwrap()).unwrap().is_empty());
    }
    #[test]
    fn owner_attention_survives_restart_and_watch_reconnect_without_a_viewer() {
        let _guard = crate::test_support::env_lock();
        let _token = crate::control::operator::test_override::set(Some("owner"));
        let (dir, state) = fixture();
        local(&state);
        let response = publish(&state, "local", "Need a decision");
        assert!(response.ok);
        let id = response.result.unwrap()["alert_id"].clone();
        let mut restored = DaemonState::new();
        restored.daemon_sessions_path = Some(dir.path().join("daemon-sessions.json"));
        let restored = Arc::new(Mutex::new(restored));
        for _ in 0..2 {
            let outcome = dispatch_request(
                &restored,
                &request(
                    "manifest.watch",
                    Caller::operator("owner"),
                    serde_json::json!({}),
                ),
            );
            let DispatchOutcome::ManifestWatchStream { handle, .. } = outcome else {
                panic!("watch not established")
            };
            assert_eq!(
                handle.initial_snapshot["owner_attention"]["local"]["id"],
                id
            );
        }
    }
    #[test]
    fn owner_attention_retry_coalesces_and_stale_ack_cannot_clear_newer_alert() {
        let _guard = crate::test_support::env_lock();
        let _token = crate::control::operator::test_override::set(Some("owner"));
        let (_dir, state) = fixture();
        local(&state);
        let first = publish(&state, "local", "first").result.unwrap()["alert_id"].clone();
        assert_eq!(
            publish(&state, "local", "first").result.unwrap()["alert_id"],
            first
        );
        let second = publish(&state, "local", "second").result.unwrap()["alert_id"].clone();
        for caller in [Caller::session("local")] {
            let r = dispatch_request(
                &state,
                &request(
                    "owner_attention.ack",
                    caller,
                    serde_json::json!({"session_uid":"local","alert_id":second}),
                ),
            )
            .into_response();
            assert_eq!(r.error.unwrap().code, ErrorCode::Unauthorized);
        }
        for (id, matched) in [(first, false), (second.clone(), true), (second, false)] {
            let r = dispatch_request(
                &state,
                &request(
                    "owner_attention.ack",
                    Caller::operator("owner"),
                    serde_json::json!({"session_uid":"local","alert_id":id}),
                ),
            )
            .into_response();
            assert_eq!(r.result.unwrap()["acknowledged"], matched);
        }
        assert!(snapshot(&state.lock().unwrap()).unwrap().is_empty());
    }
    #[test]
    fn owner_attention_corrupt_or_unwritable_storage_fails_without_false_success() {
        let (_dir, state) = fixture();
        local(&state);
        let file = path(&state.lock().unwrap());
        std::fs::write(&file, "broken").unwrap();
        assert_eq!(
            publish(&state, "local", "x").error.unwrap().code,
            ErrorCode::Internal
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "broken");
        std::fs::remove_file(&file).unwrap();
        std::fs::create_dir(&file).unwrap();
        assert!(!publish(&state, "local", "x").ok);
    }
}
