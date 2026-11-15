//! Durable periodic wakes for ordinary sessions.
//!
//! Schedules deliberately live beside (rather than inside) continuous-task
//! state.  A schedule targets a session uid, is persisted under `~/.cm`, and
//! delivers through the same durable native notification queue used by
//! `[cm-monitor ...]` wakes.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use uuid::Uuid;

use crate::control::protocol::{Caller, ErrorCode};
use crate::state::DaemonState;

const STORE_VERSION: u32 = 1;
const TICK: Duration = Duration::from_millis(250);
const MAX_HISTORY: usize = 256;

pub type Result<T> = std::result::Result<T, (ErrorCode, String)>;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WakeKind {
    Interval { every_secs: u64 },
    DailyAt { hour: u8, minute: u8 },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WakeRun {
    pub run_id: String,
    pub scheduled_at: f64,
    #[serde(default)]
    pub delivered_at: Option<f64>,
    /// `pending` is a crash-recovery state; public read paths expose it as
    /// `pending` until the scheduler resolves it to delivered/missed.
    pub outcome: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WakeSchedule {
    pub id: String,
    pub session_uid: String,
    pub kind: WakeKind,
    #[serde(default)]
    pub jitter_secs: u64,
    #[serde(default)]
    pub wake_message: Option<String>,
    pub enabled: bool,
    pub created_at: f64,
    pub next_run_at: f64,
    #[serde(default)]
    pub last_run: Option<WakeRun>,
    #[serde(default)]
    pub missed_count: u64,
    #[serde(default = "default_state")]
    pub state: String,
    #[serde(default)]
    pub awaiting_completion: bool,
    #[serde(default)]
    pub runs: Vec<WakeRun>,
}

fn default_state() -> String {
    "active".into()
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct Store {
    version: u32,
    schedules: Vec<WakeSchedule>,
}

fn store_path() -> PathBuf {
    crate::path::dot_cm_dir().join("session-wake-schedules.json")
}

static STORE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
fn lock() -> &'static Mutex<()> {
    STORE_LOCK.get_or_init(|| Mutex::new(()))
}

fn load_store(path: &Path) -> std::io::Result<Store> {
    match fs::read(path) {
        Ok(bytes) => {
            let mut store: Store = serde_json::from_slice(&bytes)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            if store.version == 0 {
                store.version = STORE_VERSION;
            }
            Ok(store)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store {
            version: STORE_VERSION,
            schedules: Vec::new(),
        }),
        Err(e) => Err(e),
    }
}

fn save_store(path: &Path, store: &Store) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(
        &tmp,
        serde_json::to_vec_pretty(store)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?,
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    fs::rename(tmp, path)
}

fn io_err(e: std::io::Error) -> (ErrorCode, String) {
    (
        ErrorCode::Internal,
        format!("session wake schedule storage: {e}"),
    )
}
fn now() -> f64 {
    crate::control::methods::now_unix_f64()
}

fn parse_kind(params: &Value) -> Result<WakeKind> {
    if let Some(n) = params.get("interval_secs").and_then(Value::as_u64) {
        if n == 0 {
            return Err((
                ErrorCode::InvalidParams,
                "interval_secs must be positive".into(),
            ));
        }
        return Ok(WakeKind::Interval { every_secs: n });
    }
    if let Some(raw) = params.get("daily_at").and_then(Value::as_str) {
        let s = raw.trim().trim_end_matches('Z');
        let (h, m) = s
            .split_once(':')
            .ok_or((ErrorCode::InvalidParams, "daily_at must be HH:MMZ".into()))?;
        let hour: u8 = h.parse().map_err(|_| {
            (
                ErrorCode::InvalidParams,
                "daily_at hour must be 0..23".into(),
            )
        })?;
        let minute: u8 = m.parse().map_err(|_| {
            (
                ErrorCode::InvalidParams,
                "daily_at minute must be 0..59".into(),
            )
        })?;
        if hour > 23 || minute > 59 {
            return Err((ErrorCode::InvalidParams, "daily_at must be HH:MMZ".into()));
        }
        return Ok(WakeKind::DailyAt { hour, minute });
    }
    if let Some(kind) = params.get("kind") {
        let parsed: WakeKind = serde_json::from_value(kind.clone())
            .map_err(|e| (ErrorCode::InvalidParams, format!("kind: {e}")))?;
        return Ok(parsed);
    }
    Err((
        ErrorCode::InvalidParams,
        "one of interval_secs, daily_at, or kind is required".into(),
    ))
}

fn jitter(id: &str, at: f64, max: u64) -> f64 {
    if max == 0 {
        return 0.0;
    }
    let mut h = 1469598103934665603u64;
    for b in format!("{id}:{at:.3}").bytes() {
        h = (h ^ b as u64).wrapping_mul(1099511628211);
    }
    (h % (max + 1)) as f64
}

fn next_at(s: &WakeSchedule, from: f64) -> f64 {
    let base = match s.kind {
        WakeKind::Interval { every_secs } => from + every_secs.max(1) as f64,
        WakeKind::DailyAt { hour, minute } => {
            let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(from as i64, 0)
                .unwrap_or_else(chrono::Utc::now);
            let date = dt.date_naive();
            let candidate = date.and_hms_opt(hour as u32, minute as u32, 0).unwrap();
            let candidate =
                chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(candidate, chrono::Utc)
                    .timestamp() as f64;
            if candidate > from {
                candidate
            } else {
                (date + chrono::Days::new(1))
                    .and_hms_opt(hour as u32, minute as u32, 0)
                    .unwrap()
                    .and_utc()
                    .timestamp() as f64
            }
        }
    };
    base + jitter(&s.id, base, s.jitter_secs)
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err((
            ErrorCode::InvalidParams,
            "schedule id must contain only letters, numbers, '-' or '_'".into(),
        ));
    }
    Ok(())
}

fn caller_uid(caller: &Caller) -> Option<&str> {
    caller.session_uid()
}

fn authorized(state: &DaemonState, caller: &Caller, target: &str) -> Result<()> {
    match caller {
        Caller::Operator(_) => Ok(()),
        Caller::Session(c) => {
            if crate::control::auth::check_session_caller(state, &c.session_uid, target).is_allow()
            {
                Ok(())
            } else {
                Err((
                    ErrorCode::Unauthorized,
                    format!(
                        "session wake schedule target '{target}' is outside the caller's scope"
                    ),
                ))
            }
        }
    }
}

fn target_from_params(state: &DaemonState, caller: &Caller, params: &Value) -> Result<String> {
    let target = params
        .get("session_uid")
        .and_then(Value::as_str)
        .or_else(|| caller_uid(caller))
        .ok_or((
            ErrorCode::InvalidParams,
            "session_uid is required for an Operator caller".into(),
        ))?;
    if !state.sessions.contains_key(target) {
        return Err((ErrorCode::NotFound, format!("session '{target}' not found")));
    }
    authorized(state, caller, target)?;
    Ok(target.to_string())
}

fn public_schedule(s: &WakeSchedule) -> Value {
    json!({
        "id": s.id, "session_uid": s.session_uid, "kind": s.kind,
        "jitter_secs": s.jitter_secs, "wake_message": s.wake_message,
        "enabled": s.enabled, "state": s.state, "created_at": s.created_at,
        "next_run": if s.enabled && s.state == "active" { Value::from(s.next_run_at) } else { Value::Null },
        "last_run": s.last_run, "missed_count": s.missed_count,
        "runs": s.runs,
    })
}

pub fn handle(
    state_arc: &Arc<Mutex<DaemonState>>,
    caller: &Caller,
    action: &str,
    params: &Value,
) -> Result<Value> {
    let path = store_path();
    let _guard = lock().lock().unwrap_or_else(|p| p.into_inner());
    let mut store = load_store(&path).map_err(io_err)?;
    match action {
        "create" => {
            let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
            let target = target_from_params(&state, caller, params)?;
            let kind = parse_kind(params)?;
            let jitter_secs = params
                .get("jitter_secs")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .min(86_400);
            let wake_message = params
                .get("wake_message")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            if wake_message.as_ref().is_some_and(|m| m.len() > 4096) {
                return Err((
                    ErrorCode::InvalidParams,
                    "wake_message is limited to 4096 bytes".into(),
                ));
            }
            let id = params
                .get("schedule_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            let id = if id.is_empty() {
                format!("wake-{}", Uuid::new_v4())
            } else {
                id.to_string()
            };
            validate_id(&id)?;
            if store.schedules.iter().any(|s| s.id == id) {
                return Err((
                    ErrorCode::Conflict,
                    format!("schedule '{id}' already exists"),
                ));
            }
            let created = now();
            let mut schedule = WakeSchedule {
                id,
                session_uid: target,
                kind,
                jitter_secs,
                wake_message,
                enabled: true,
                created_at: created,
                next_run_at: created,
                last_run: None,
                missed_count: 0,
                state: "active".into(),
                awaiting_completion: false,
                runs: Vec::new(),
            };
            schedule.next_run_at = next_at(&schedule, created);
            let out = public_schedule(&schedule);
            store.schedules.push(schedule);
            save_store(&path, &store).map_err(io_err)?;
            Ok(json!({"schedule": out}))
        }
        "list" => {
            let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
            let target = params
                .get("session_uid")
                .and_then(Value::as_str)
                .or_else(|| caller_uid(caller));
            let mut rows = Vec::new();
            for schedule in &store.schedules {
                if let Some(target) = target {
                    if schedule.session_uid != target {
                        continue;
                    }
                    authorized(&state, caller, target)?;
                } else if !matches!(caller, Caller::Operator(_)) {
                    continue;
                }
                rows.push(public_schedule(schedule));
            }
            Ok(json!({"schedules": rows, "version": STORE_VERSION}))
        }
        "update" => {
            let id = params
                .get("schedule_id")
                .and_then(Value::as_str)
                .ok_or((ErrorCode::InvalidParams, "schedule_id is required".into()))?;
            let schedule = store
                .schedules
                .iter_mut()
                .find(|s| s.id == id)
                .ok_or((ErrorCode::NotFound, format!("schedule '{id}' not found")))?;
            let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
            authorized(&state, caller, &schedule.session_uid)?;
            if schedule.state == "cancelled" {
                return Err((
                    ErrorCode::Conflict,
                    format!("schedule '{id}' is cancelled; create a new schedule"),
                ));
            }
            if let Some(enabled) = params.get("enabled").and_then(Value::as_bool) {
                schedule.enabled = enabled;
                schedule.state = if enabled {
                    "active".into()
                } else {
                    "paused".into()
                };
            }
            if let Some(j) = params.get("jitter_secs").and_then(Value::as_u64) {
                schedule.jitter_secs = j.min(86_400);
            }
            let cadence_changed = params.get("interval_secs").is_some()
                || params.get("daily_at").is_some()
                || params.get("kind").is_some();
            if cadence_changed {
                schedule.kind = parse_kind(params)?;
                schedule.next_run_at = next_at(schedule, now());
            }
            if let Some(m) = params.get("wake_message") {
                schedule.wake_message = m
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
            }
            let out = public_schedule(schedule);
            save_store(&path, &store).map_err(io_err)?;
            Ok(json!({"schedule": out}))
        }
        "cancel" => {
            let id = params
                .get("schedule_id")
                .and_then(Value::as_str)
                .ok_or((ErrorCode::InvalidParams, "schedule_id is required".into()))?;
            let schedule = store
                .schedules
                .iter_mut()
                .find(|s| s.id == id)
                .ok_or((ErrorCode::NotFound, format!("schedule '{id}' not found")))?;
            let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
            authorized(&state, caller, &schedule.session_uid)?;
            schedule.enabled = false;
            schedule.state = "cancelled".into();
            let out = public_schedule(schedule);
            save_store(&path, &store).map_err(io_err)?;
            Ok(json!({"schedule": out}))
        }
        _ => Err((
            ErrorCode::InvalidParams,
            "action must be create, list, update, or cancel".into(),
        )),
    }
}

pub struct WakeScheduler {
    state: Arc<Mutex<DaemonState>>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    boot_at: f64,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl WakeScheduler {
    pub fn new(state: Arc<Mutex<DaemonState>>) -> Self {
        Self {
            state,
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            boot_at: now(),
            thread: Mutex::new(None),
        }
    }
    pub fn start(&self) -> std::io::Result<()> {
        let mut slot = self.thread.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_some() {
            return Ok(());
        }
        let state = Arc::clone(&self.state);
        let stop = Arc::clone(&self.shutdown);
        let boot_at = self.boot_at;
        *slot = Some(
            thread::Builder::new()
                .name("cm-session-wakes".into())
                .spawn(move || {
                    let mut first_tick = true;
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        Self::tick_at(&state, boot_at, first_tick);
                        first_tick = false;
                        thread::sleep(TICK);
                    }
                })?,
        );
        Ok(())
    }
    pub fn shutdown(&self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self.thread.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = handle.join();
        }
    }
    pub fn tick_once(state_arc: &Arc<Mutex<DaemonState>>) {
        Self::tick_at(state_arc, now(), false);
    }
    fn tick_at(state_arc: &Arc<Mutex<DaemonState>>, boot_at: f64, first_tick: bool) {
        let path = store_path();
        let _guard = lock().lock().unwrap_or_else(|p| p.into_inner());
        let Ok(mut store) = load_store(&path) else {
            return;
        };
        let now = now();
        let mut changed = false;
        for schedule in &mut store.schedules {
            if !schedule.enabled || schedule.state != "active" {
                continue;
            }
            // A pending record is a delivery attempt interrupted by a brain
            // restart. Retry it exactly once; do not manufacture one run per
            // elapsed interval.
            let pending = schedule.runs.iter().position(|r| r.outcome == "pending");
            let run_index = if let Some(i) = pending {
                i
            } else if schedule.next_run_at <= now {
                if first_tick {
                    // A brain can be down for several periods. Record the
                    // additional due slots as missed, but leave one slot for
                    // the single catch-up delivery below.
                    let mut cursor = schedule.next_run_at;
                    let mut missed = 0usize;
                    while missed < MAX_HISTORY {
                        let next = next_at(schedule, cursor);
                        if next > boot_at {
                            break;
                        }
                        let run = WakeRun {
                            run_id: Uuid::new_v4().to_string(),
                            scheduled_at: next,
                            delivered_at: None,
                            outcome: "missed".into(),
                            reason: Some("brain_down".into()),
                        };
                        schedule.runs.push(run.clone());
                        schedule.last_run = Some(run);
                        schedule.missed_count += 1;
                        cursor = next;
                        missed += 1;
                    }
                }
                let run = WakeRun {
                    run_id: Uuid::new_v4().to_string(),
                    scheduled_at: schedule.next_run_at,
                    delivered_at: None,
                    outcome: "pending".into(),
                    reason: None,
                };
                schedule.runs.push(run);
                let i = schedule.runs.len() - 1;
                schedule.next_run_at = next_at(schedule, now);
                i
            } else {
                continue;
            };
            let target = schedule.session_uid.clone();
            let (dead, busy) = {
                let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
                match state.sessions.get(&target) {
                    None => (true, false),
                    Some(session) if session.last_exit.kernel_set() => (true, false),
                    Some(session) => {
                        let idle = session.semantic_idle();
                        let pty_idle =
                            crate::control::methods::session_pty_idle_for_schedule(session);
                        (
                            false,
                            !pty_idle
                                || schedule.awaiting_completion && idle != Some(true)
                                || idle == Some(false),
                        )
                    }
                }
            };
            let run = &mut schedule.runs[run_index];
            if dead {
                run.outcome = "missed".into();
                run.reason = Some("session_dead".into());
                schedule.missed_count += 1;
                schedule.last_run = Some(run.clone());
                changed = true;
            } else if busy {
                run.outcome = "missed".into();
                run.reason = Some("coalesced_busy".into());
                schedule.missed_count += 1;
                schedule.last_run = Some(run.clone());
                changed = true;
            } else {
                let marker = format!("[cm-schedule {} {}]", schedule.id, run.run_id);
                let text = format!(
                    "{} Wake from CM schedule {}. Continue your work; this is an automated notice.",
                    schedule
                        .wake_message
                        .clone()
                        .unwrap_or_else(|| "Scheduled check-in.".into()),
                    marker
                );
                match crate::notifications::publish(
                    &crate::path::dot_cm_dir(),
                    &target,
                    &format!("session-schedule:{}", run.run_id),
                    "session-schedule",
                    &text,
                    &marker,
                ) {
                    Ok(event) => {
                        run.outcome = "delivered".into();
                        run.delivered_at = event["created_at"].as_f64().or(Some(now));
                        run.reason = None;
                        schedule.last_run = Some(run.clone());
                        schedule.awaiting_completion = true;
                        changed = true;
                    }
                    Err(e) => {
                        run.outcome = "missed".into();
                        run.reason = Some(format!("delivery_failed: {e}"));
                        schedule.missed_count += 1;
                        schedule.last_run = Some(run.clone());
                        changed = true;
                    }
                }
            }
            if schedule.awaiting_completion {
                let clear = state_arc
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .sessions
                    .get(&target)
                    .and_then(|s| s.semantic_idle())
                    .is_some_and(|v| v);
                if clear {
                    schedule.awaiting_completion = false;
                    changed = true;
                }
            }
            if schedule.runs.len() > MAX_HISTORY {
                let drop_n = schedule.runs.len() - MAX_HISTORY;
                schedule.runs.drain(0..drop_n);
            }
        }
        if changed {
            let _ = save_store(&path, &store);
        }
    }
}

impl Drop for WakeScheduler {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_interval_and_daily() {
        assert_eq!(
            parse_kind(&json!({"interval_secs": 21600})).unwrap(),
            WakeKind::Interval { every_secs: 21600 }
        );
        assert_eq!(
            parse_kind(&json!({"daily_at": "13:00Z"})).unwrap(),
            WakeKind::DailyAt {
                hour: 13,
                minute: 0
            }
        );
    }
    #[test]
    fn daily_rolls_to_next_day() {
        let s = WakeSchedule {
            id: "x".into(),
            session_uid: "u".into(),
            kind: WakeKind::DailyAt {
                hour: 13,
                minute: 0,
            },
            jitter_secs: 0,
            wake_message: None,
            enabled: true,
            created_at: 0.0,
            next_run_at: 0.0,
            last_run: None,
            missed_count: 0,
            state: "active".into(),
            awaiting_completion: false,
            runs: vec![],
        };
        let at = chrono::DateTime::parse_from_rfc3339("2026-11-15T14:00:00Z")
            .unwrap()
            .timestamp() as f64;
        assert!(next_at(&s, at) > at);
    }
}
