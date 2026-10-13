//! Owner attention is separate from agent messaging and out-of-band escalation.
//! Persist before broadcasting; manifest.watch replays pending alerts on reconnect.
//!
//! Every request is delivered regardless of Owner's informational availability
//! level. The legacy held queue is drained after upgrade.
//! `escalate()`/`withdraw()` is the one interface daemon-side sources (the
//! notification stall alarm, the outbox alarm, the work-item board) use.
use crate::control::protocol::{Caller, ErrorCode, Request, Response};
use crate::manifest::ManifestDiff;
use crate::owner_availability::{self as availability, Urgency};
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
    /// Set when a legacy held request was released into delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released_at: Option<String>,
    /// Original request time; absent for alerts created before this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

/// A legacy held request: the latest per key, plus the original count.
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

pub(crate) fn path(state: &DaemonState) -> PathBuf {
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
    snapshot_at(&path(state))
}

pub(crate) fn snapshot_at(path: &std::path::Path) -> Result<BTreeMap<String, Alert>, String> {
    match std::fs::read(path) {
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

fn broadcast(state: &DaemonState, key: &str, alert: Option<&Alert>) {
    if let Some(sync) = &state.messaging_sync {
        sync.signal_owner_alerts();
    }
    // This boundary includes newly delivered and legacy released alerts.
    // Web Push is a separate subscriber:
    // it must not count as an attached viewer or change Telegram's policy.
    if let Some(alert) = alert {
        if let Err(e) = state.owner_notifications.publish(&state.messaging_root,
            format!("cm-alert:{}", alert.id), &alert.label, &alert.message,
            format!("/#alerts&alert={}", crate::owner_notifications::url_component(&alert.id))) {
            eprintln!("cm Owner notification checkpoint failed: {e}");
        }
    }
    state.manifest_watcher.broadcast(ManifestDiff::Updated {
        uid: key.to_owned(),
        entry: json!({"owner_attention": alert}),
    });
}

/// Out-of-band push (`notify_command`, e.g. Telegram): every emergency, and
/// blocking requests when no viewer is connected. Availability is informational.
/// Rate-limited per key so repeated requests cannot spam the phone.
fn maybe_push(state: &DaemonState, key: &str, alert: &Alert, urgency: Urgency) {
    let viewer = state.manifest_watcher.subscriber_count() > 0;
    if !(urgency == Urgency::Emergency || urgency >= Urgency::Blocking && !viewer) {
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

/// What happened to one request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GateOutcome {
    Delivered { alert_id: String },
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

/// Deliver `alert` under `key`, at every availability level. A daemon
/// escalation (`replace = false`) never displaces a pending alert; an agent's
/// own `notify_user` (`replace = true`) replaces its earlier pending one, as
/// it always has.
fn deliver(
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
    alert.label = truncate(&alert.label);
    alert.created_at.get_or_insert_with(now_rfc3339);
    if alerts.len() >= MAX_ENTRIES && !alerts.contains_key(key) {
        return Err("Owner alert queue is full; pending alerts were retained".into());
    }
    alerts.insert(key.to_owned(), alert.clone());
    save(state, &alerts)?;
    broadcast(state, key, Some(&alert));
    maybe_push(state, key, &alert, urgency);
    Ok(GateOutcome::Delivered { alert_id: alert.id })
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
    deliver(state, &e.dedupe_key, alert, e.urgency, false)
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
    let outcome = match deliver(state, uid, alert, urgency, true) {
        Ok(o) => o,
        Err(e) => return Response::err(req.id.clone(), ErrorCode::Internal, e),
    };
    let result = match outcome {
        GateOutcome::Delivered { alert_id } | GateOutcome::Coalesced { alert_id } => json!({
            "ok": true, "status": "queued", "delivery": "immediate", "alert_id": alert_id,
            "urgency": urgency.as_str(), "owner_availability": availability_json(state),
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
        GateOutcome::Delivered { alert_id } => Some(alert_id),
        GateOutcome::Coalesced { .. } => None,
    })
}

/// Withdraw a daemon-originated alert once its condition cleared.
pub fn clear_system(state: &DaemonState, uid: &str, alert_id: &str) -> Result<bool, String> {
    withdraw(state, uid, alert_id)
}

/// Per-session counts of legacy requests released into delivery.
#[derive(Default, Debug)]
pub struct ReleaseOutcome {
    pub released: BTreeMap<String, usize>,
}

/// Move all legacy held requests into the delivered map, marked `released_at`.
/// A key with a pending alert keeps its ID and gets the held text merged in,
/// up to the 4096-byte limit.
pub fn release_held(state: &DaemonState) -> Result<ReleaseOutcome, String> {
    let mut held = held_snapshot(state)?;
    let mut out = ReleaseOutcome::default();
    if held.is_empty() {
        return Ok(out);
    }
    let mut alerts = snapshot(state)?;
    let now = now_rfc3339();
    let mut changed = Vec::new();
    held.retain(|key, h| {
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
        maybe_push(state, key, &alerts[key], urgency);
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
/// use the same delivery path.
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
    match acknowledge_ids(state, &[(params.session_uid, params.alert_id)]) {
        Ok(count) => Response::ok(req.id.clone(), json!({"ok":true,"acknowledged":count>0})),
        Err(e) => Response::err(req.id.clone(), ErrorCode::Internal, e),
    }
}

/// Batch a peer's exact-ID acknowledgements into one durable queue rewrite.
/// Only delivered alerts are considered; held requests cannot be cleared.
pub(crate) fn acknowledge_ids(state: &DaemonState, ids: &[(String, String)]) -> Result<usize, String> {
    let mut alerts = snapshot(state)?;
    let mut removed = Vec::new();
    for (key, id) in ids {
        if alerts.get(key).is_some_and(|a| &a.id == id) {
            alerts.remove(key);
            removed.push(key);
        }
    }
    if !removed.is_empty() {
        save(state, &alerts)?;
        for key in &removed { broadcast(state, key, None); }
    }
    Ok(removed.len())
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
        std::fs::write(dir.path().join("daemon-id"), uuid::Uuid::new_v4().to_string()).unwrap();
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
    // Model the exact on-disk format written by pre-upgrade daemons.
    fn seed_held(state: &Arc<Mutex<DaemonState>>, uid: &str, message: &str, urgency: &str, count: u32) -> String {
        let s = state.lock().unwrap();
        let id = format!("legacy-{uid}");
        let mut old = held_snapshot(&s).unwrap();
        old.insert(uid.into(), Held { alert: Alert {
            id: id.clone(), session_uid: uid.into(), label: uid.into(),
            message: message.into(), urgency: Some(urgency.into()),
            held_since: Some("2026-10-12T12:00:00Z".into()),
            created_at: Some("2026-10-12T12:00:00Z".into()),
            ..Alert::default()
        }, count });
        save_held(&s, &old).unwrap();
        id
    }
    #[test]
    fn web_feed_receives_alerts_while_away_without_replaying_seen_alerts() {
        let _guard = crate::test_support::env_lock();
        let (dir,state) = fixture(); local(&state);
        let feed = state.lock().unwrap().owner_notifications.clone();
        set_level(&state,Some("away"),"away");
        notify_with(&state,"local","please decide","decision");
        feed.flush();
        let page = feed.next(dir.path(),&json!({"wait_ms":0})).unwrap();
        assert_eq!(page["events"].as_array().unwrap().len(),1);
        let listed = crate::owner_alert_relay::list(&state,&request("owner_attention.list",Caller::operator("test"),json!({}))).result.unwrap();
        let alert = &listed["items"][0];
        let stale = crate::owner_alert_relay::acknowledge(&state,&request("owner_attention.ack",Caller::operator("test"),json!({"session_uid":alert["key"],"alert_id":"stale"})));
        assert_eq!(stale.result.unwrap()["acknowledged"],false);
        crate::owner_alert_relay::acknowledge(&state,&request("owner_attention.ack",Caller::operator("test"),json!({"session_uid":alert["key"],"alert_id":alert["id"]})));
        feed.flush();
        assert!(feed.next(dir.path(),&json!({"wait_ms":0,"cursor":page["cursor"]})).unwrap()["events"].as_array().unwrap().is_empty());
        for method in ["owner_attention.list","owner.notifications.next"] {
            let response = dispatch_request(&state,&request(method,Caller::session("local"),json!({"wait_ms":0}))).into_response();
            assert_eq!(response.error.unwrap().code,ErrorCode::Unauthorized);
            assert!(!crate::control::dispatch::method_counts_as_restart_mutation(method));
        }
    }
    #[test]
    fn every_level_delivers_every_urgency_to_viewers_and_web_without_holding() {
        let _guard = crate::test_support::env_lock();
        let (dir, state) = fixture();
        local(&state);
        let (watcher, _subscription) = state.lock().unwrap().manifest_watcher.subscribe();
        let feed = state.lock().unwrap().owner_notifications.clone();
        for level in [None, Some("on-call"), Some("focused"), Some("around"), Some("away")] {
            set_level(&state, level, "rev");
            for urgency in ["fyi", "decision", "blocking", "emergency"] {
                let message = format!("{level:?} {urgency}");
                let r = notify_with(&state, "local", &message, urgency);
                assert_eq!(r["delivery"], "immediate", "{level:?} {urgency}");
                assert_eq!(r["urgency"], urgency);
                assert_eq!(r["status"], "queued");
                assert_eq!(r["owner_availability"]["level"], json!(level));
                assert!(r.get("release_when").is_none());
                assert_eq!(snapshot(&state.lock().unwrap()).unwrap()["local"].message, message);
                assert!(held(&state).is_empty());
                assert!(watcher.try_recv().is_ok(), "viewer receives {message}");
            }
        }
        feed.flush();
        assert_eq!(feed.next(dir.path(), &json!({"wait_ms":0})).unwrap()["events"].as_array().unwrap().len(), 20);
        // Default urgency is decision; an unknown one is refused.
        set_level(&state, Some("focused"), "rev2");
        let r = dispatch_request(&state, &request("notify_user", Caller::session("local"), serde_json::json!({"message":"x"}))).into_response();
        assert_eq!(r.result.unwrap()["urgency"], "decision");
        let r = dispatch_request(&state, &request("notify_user", Caller::session("local"), serde_json::json!({"message":"x","urgency":"asap"}))).into_response();
        assert_eq!(r.error.unwrap().code, ErrorCode::InvalidParams);
    }
    #[test]
    fn legacy_held_requests_release_while_away_and_preserve_pending_identity_and_urgency() {
        let _guard = crate::test_support::env_lock();
        let (_dir, state) = fixture();
        local(&state);
        set_level(&state, Some("away"), "r1");
        seed_held(&state, "local", "also: rename?", "decision", 2);
        let emergency = notify_with(&state, "local", "prod down", "emergency");
        let out = release_held(&state.lock().unwrap()).unwrap();
        assert_eq!(out.released["local"], 2);
        let alerts = snapshot(&state.lock().unwrap()).unwrap();
        let a = &alerts["local"];
        assert!(a.released_at.is_some());
        assert_eq!(a.urgency.as_deref(), Some("emergency"));
        assert_eq!(json!(a.id), emergency["alert_id"], "a merge keeps the pending alert's id");
        assert!(a.message.starts_with("prod down") && a.message.contains("[released] also: rename?"), "{}", a.message);
        assert!(a.message.contains("2 requests held"));
        assert!(held(&state).is_empty());
        assert!(release_held(&state.lock().unwrap()).unwrap().released.is_empty());
        assert_eq!(snapshot(&state.lock().unwrap()).unwrap(), alerts, "retries do not append again");
    }
    #[test]
    fn escalate_delivers_coalesces_and_withdraws_while_away() {
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
        // Unset and away have the same no-viewer blocking fallback as on-call.
        set_level(&state, None, "r2");
        notify_with(&state, "push-b", "need creds", "blocking");
        assert!(pushes().contains("need creds"));
        set_level(&state, Some("away"), "r3");
        notify_with(&state, "push-c", "need keys", "blocking");
        assert!(pushes().contains("[blocking] Blocker: need keys"));
        let (_receiver, _subscription) = state.lock().unwrap().manifest_watcher.subscribe();
        state.lock().unwrap().tui_sessions.insert("push-viewer".into(),
            serde_json::from_value(json!({"uid":"push-viewer","label":"Watched"})).unwrap());
        notify_with(&state, "push-viewer", "viewer handles blocking", "blocking");
        assert!(!pushes().contains("viewer handles blocking"));
        notify_with(&state, "push-viewer", "emergency despite viewer", "emergency");
        assert!(pushes().contains("emergency despite viewer"));

    }
    #[test]
    fn legacy_release_keeps_requests_when_the_delivered_store_is_unavailable() {
        let _guard = crate::test_support::env_lock();
        let (_dir, state) = fixture();
        set_level(&state, Some("away"), "same-revision");
        for urgency in ["fyi", "decision", "blocking", "emergency"] {
            seed_held(&state, urgency, &format!("old {urgency}"), urgency, 1);
        }
        let s = state.lock().unwrap();
        let before = std::fs::read(held_path(&s)).unwrap();
        std::fs::create_dir(path(&s)).unwrap();
        assert!(release_held(&s).is_err());
        assert_eq!(std::fs::read(held_path(&s)).unwrap(), before);
        std::fs::remove_dir(path(&s)).unwrap();
        let released = release_held(&s).unwrap();
        assert_eq!(released.released.len(), 4);
        assert!(held_snapshot(&s).unwrap().is_empty());
        for (key, alert) in snapshot(&s).unwrap() {
            assert_eq!(alert.urgency.as_deref(), Some(key.as_str()));
            assert!(alert.released_at.is_some());
        }
    }
    #[test]
    fn upgrade_releases_held_on_an_unchanged_revision_once_and_level_changes_never_wake() {
        let _guard = crate::test_support::env_lock();
        // Both a previous daemon's applied checkpoint and a missing checkpoint
        // must drain the old file; neither requires a new availability event.
        for prior_checkpoint in [false, true] {
            let (dir, state) = fixture();
            cloud(&state, false);
            set_level(&state, Some("away"), "r1");
            if prior_checkpoint {
                std::fs::write(dir.path().join("owner-availability-applied.json"),
                    json!({"level":"away","event_id":"r1"}).to_string()).unwrap();
            }
            let id = seed_held(&state, "cloud", "which region?", "decision", 1);
            let root = dir.path().to_path_buf();
            let wakes = || crate::notifications::snapshot(&root, "cloud").map(|(e, _)| e).unwrap_or_default();
            let feed = state.lock().unwrap().owner_notifications.clone();
            crate::owner_availability::tick(&state);
            let alerts = snapshot(&state.lock().unwrap()).unwrap();
            assert_eq!(alerts["cloud"].id, id);
            assert_eq!(alerts["cloud"].message, "which region?");
            assert_eq!(alerts["cloud"].created_at.as_deref(), Some("2026-10-12T12:00:00Z"));
            assert!(alerts["cloud"].released_at.is_some());
            assert!(held(&state).is_empty());
            feed.flush();
            assert_eq!(feed.next(dir.path(), &json!({"wait_ms":0})).unwrap()["events"].as_array().unwrap().len(), 1);
            let events = wakes();
            assert_eq!(events.len(), 1);
            let text = events[0]["text"].as_str().unwrap();
            assert!(text.contains("1 of your previously held requests were released"), "{text}");
            let marker = events[0]["marker"].as_str().unwrap();
            assert_eq!(marker, "[cm-owner-alerts-unfiltered]");
            assert!(text.starts_with(marker));
            crate::owner_availability::tick(&state);
            set_level(&state, Some("focused"), "r2");
            crate::owner_availability::tick(&state);
            assert_eq!(wakes().len(), 1);
            assert_eq!(snapshot(&state.lock().unwrap()).unwrap(), alerts);
            let applied: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.path().join("owner-availability-applied.json")).unwrap()).unwrap();
            assert_eq!(applied["unfiltered_notifications"], true);
        }
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
