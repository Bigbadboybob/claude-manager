//! Durable scheduled wakes for ordinary sessions.
//!
//! The schedule store is daemon-owned and kept in memory for the life of the
//! brain. Changes are atomically persisted, and delivery uses the same native
//! notification queue as monitor wakes.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use uuid::Uuid;

use crate::control::protocol::{Caller, ErrorCode};
use crate::state::DaemonState;

const STORE_VERSION: u32 = 1;
const TICK: Duration = Duration::from_secs(2);
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
    pub outcome: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub coalesced_into: Option<String>,
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
    pub awaiting_run_id: Option<String>,
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

struct RuntimeStore {
    path: PathBuf,
    store: Mutex<Store>,
}
static RUNTIME: OnceLock<Arc<RuntimeStore>> = OnceLock::new();

fn store_path() -> PathBuf {
    crate::path::dot_cm_dir().join("session-wake-schedules.json")
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

fn runtime() -> std::io::Result<Arc<RuntimeStore>> {
    if let Some(runtime) = RUNTIME.get() {
        return Ok(Arc::clone(runtime));
    }
    let path = store_path();
    let candidate = Arc::new(RuntimeStore {
        path: path.clone(),
        store: Mutex::new(load_store(&path)?),
    });
    if RUNTIME.set(Arc::clone(&candidate)).is_ok() {
        Ok(candidate)
    } else {
        Ok(Arc::clone(RUNTIME.get().expect("wake runtime initialized")))
    }
}

fn persist(runtime: &RuntimeStore, store: &Store) -> Result<()> {
    save_store(&runtime.path, store).map_err(|e| {
        (
            ErrorCode::Internal,
            format!("session wake schedule storage: {e}"),
        )
    })
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
        let (h, m) = raw
            .trim()
            .trim_end_matches('Z')
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
        return serde_json::from_value(kind.clone())
            .map_err(|e| (ErrorCode::InvalidParams, format!("kind: {e}")));
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

fn cadence_secs(s: &WakeSchedule) -> f64 {
    match s.kind {
        WakeKind::Interval { every_secs } => every_secs.max(1) as f64,
        WakeKind::DailyAt { .. } => 86_400.0,
    }
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
        Caller::Session(c)
            if crate::control::auth::check_session_caller(state, &c.session_uid, target)
                .is_allow() =>
        {
            Ok(())
        }
        Caller::Session(_) => Err((
            ErrorCode::Unauthorized,
            format!("session wake schedule target '{target}' is outside the caller's scope"),
        )),
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
    json!({"id":s.id,"session_uid":s.session_uid,"kind":s.kind,"jitter_secs":s.jitter_secs,"wake_message":s.wake_message,"enabled":s.enabled,"state":s.state,"created_at":s.created_at,"next_run":if s.enabled && s.state=="active" {Value::from(s.next_run_at)} else {Value::Null},"last_run":s.last_run,"missed_count":s.missed_count,"runs":s.runs})
}

/// Read-only live projection used by the org/team marker. It deliberately
/// reads the already-loaded runtime store; the org sidebar must not introduce
/// a disk poll or copy schedule state into the org record.
pub fn live_summary(session_uid: &str) -> Value {
    let Ok(runtime) = runtime() else { return json!({"next_run":null,"last_run":null,"missed_count":0,"health":"blocked"}); };
    let store = runtime.store.lock().unwrap_or_else(|p| p.into_inner());
    let rows: Vec<&WakeSchedule> = store.schedules.iter().filter(|s| s.enabled && s.state == "active" && s.session_uid == session_uid).collect();
    let next = rows.iter().filter_map(|s| (s.next_run_at > 0.0).then_some(s.next_run_at)).min_by(f64::total_cmp);
    let last = rows.iter().filter_map(|s| s.last_run.as_ref()).max_by(|a,b| a.scheduled_at.total_cmp(&b.scheduled_at));
    let missed: u64 = rows.iter().map(|s| s.missed_count).sum();
    let cadence = rows.iter().map(|s| cadence_secs(s)).min_by(f64::total_cmp);
    let schedule_kind = rows.first().map(|s| match s.kind { WakeKind::Interval { .. } => "interval", WakeKind::DailyAt { .. } => "daily_at" });
    let health = if rows.is_empty() { "blocked" } else if missed > 0 { "missed" } else if next.is_some_and(|n| n <= now()) { "late" } else { "ok" };
    json!({"next_run":next,"last_run":last,"missed_count":missed,"health":health,"cadence_secs":cadence,"schedule_kind":schedule_kind})
}

pub fn handle(
    state_arc: &Arc<Mutex<DaemonState>>,
    caller: &Caller,
    action: &str,
    params: &Value,
) -> Result<Value> {
    let runtime = runtime().map_err(|e| {
        (
            ErrorCode::Internal,
            format!("session wake schedule storage: {e}"),
        )
    })?;
    let mut store = runtime.store.lock().unwrap_or_else(|p| p.into_inner());
    match action {
        "create" => {
            let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
            let target = target_from_params(&state, caller, params)?;
            let kind = parse_kind(params)?;
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
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("wake-{}", Uuid::new_v4()));
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
                jitter_secs: params
                    .get("jitter_secs")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .min(86_400),
                wake_message,
                enabled: true,
                created_at: created,
                next_run_at: created,
                last_run: None,
                missed_count: 0,
                state: "active".into(),
                awaiting_completion: false,
                awaiting_run_id: None,
                runs: Vec::new(),
            };
            schedule.next_run_at = next_at(&schedule, created);
            let out = public_schedule(&schedule);
            store.schedules.push(schedule);
            persist(&runtime, &store)?;
            Ok(json!({"schedule":out}))
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
            Ok(json!({"schedules":rows,"version":STORE_VERSION}))
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
                schedule.next_run_at = next_at(schedule, now());
            }
            if let Some(j) = params.get("jitter_secs").and_then(Value::as_u64) {
                schedule.jitter_secs = j.min(86_400);
            }
            if params.get("interval_secs").is_some()
                || params.get("daily_at").is_some()
                || params.get("kind").is_some()
            {
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
            persist(&runtime, &store)?;
            Ok(json!({"schedule":out}))
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
            schedule.next_run_at = 0.0;
            let out = public_schedule(schedule);
            persist(&runtime, &store)?;
            Ok(json!({"schedule":out}))
        }
        _ => Err((
            ErrorCode::InvalidParams,
            "action must be create, list, update, or cancel".into(),
        )),
    }
}

#[derive(Clone, Debug, Default)]
struct TargetSnapshot {
    alive: bool,
    turn_started_at: Option<f64>,
    completed_turn_at: Option<f64>,
    busy: bool,
}

type PublishFn<'a> = dyn FnMut(&str, &str, &str, &str) -> std::result::Result<f64, String> + 'a;

fn find_run_mut<'a>(schedule: &'a mut WakeSchedule, id: &str) -> Option<&'a mut WakeRun> {
    schedule.runs.iter_mut().find(|r| r.run_id == id)
}
fn latest_delivery(schedule: &WakeSchedule) -> Option<(String, f64)> {
    let id = schedule.awaiting_run_id.as_ref()?;
    schedule
        .runs
        .iter()
        .find(|r| &r.run_id == id)
        .and_then(|r| r.delivered_at.map(|at| (r.run_id.clone(), at)))
}

/// Deterministic tick engine. Production supplies live snapshots and the native publisher.
fn tick_store(
    store: &mut Store,
    now: f64,
    boot_at: f64,
    first_tick: bool,
    targets: &HashMap<String, TargetSnapshot>,
    publish: &mut PublishFn<'_>,
) -> bool {
    let mut changed = false;
    for schedule in &mut store.schedules {
        if !schedule.enabled || schedule.state != "active" {
            continue;
        }
        let target = targets
            .get(&schedule.session_uid)
            .cloned()
            .unwrap_or_default();
        // Busy state never suppresses the first delivery: the native queue
        // retains it until the session can consume it.
        let _busy = target.busy;
        if schedule.awaiting_completion {
            if let Some((_, delivered_at)) = latest_delivery(schedule) {
                if target
                    .turn_started_at
                    .is_some_and(|started| started > delivered_at)
                    && target
                        .completed_turn_at
                        .is_some_and(|ended| ended > delivered_at)
                {
                    schedule.awaiting_completion = false;
                    schedule.awaiting_run_id = None;
                } else if now >= delivered_at + cadence_secs(schedule) {
                    if let Some(id) = schedule.awaiting_run_id.clone() {
                        if let Some(old) = find_run_mut(schedule, &id) {
                            old.outcome = "missed".into();
                            old.reason = Some("stale".into());
                            schedule.last_run = Some(old.clone());
                            schedule.missed_count += 1;
                        }
                    }
                    schedule.awaiting_completion = false;
                    schedule.awaiting_run_id = None;
                }
            } else {
                schedule.awaiting_completion = false;
                schedule.awaiting_run_id = None;
                changed = true;
            }
        }
        let pending = schedule.runs.iter().position(|r| r.outcome == "pending");
        let run_index = if let Some(i) = pending {
            i
        } else if schedule.next_run_at <= now {
            if first_tick {
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
                        coalesced_into: None,
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
                coalesced_into: None,
            };
            schedule.runs.push(run);
            let i = schedule.runs.len() - 1;
            schedule.next_run_at = next_at(schedule, now);
            i
        } else {
            continue;
        };
        let run_id = schedule.runs[run_index].run_id.clone();
        if !target.alive {
            let run = &mut schedule.runs[run_index];
            run.outcome = "missed".into();
            run.reason = Some("session_dead".into());
            schedule.last_run = Some(run.clone());
            schedule.missed_count += 1;
            changed = true;
        } else if schedule.awaiting_completion {
            let old_id = schedule.awaiting_run_id.clone();
            let run = &mut schedule.runs[run_index];
            run.outcome = "coalesced".into();
            run.reason = Some("previous_wake_unconsumed".into());
            run.coalesced_into = old_id;
            schedule.last_run = Some(run.clone());
            changed = true;
        } else {
            let marker = format!("[cm-schedule {} {}]", schedule.id, run_id);
            let text = format!(
                "{} Wake from CM schedule {}. Continue your work; this is an automated notice.",
                schedule
                    .wake_message
                    .clone()
                    .unwrap_or_else(|| "Scheduled check-in.".into()),
                marker
            );
            match publish(&schedule.session_uid, &run_id, &text, &marker) {
                Ok(delivered_at) => {
                    let run = &mut schedule.runs[run_index];
                    run.outcome = "delivered".into();
                    run.delivered_at = Some(delivered_at);
                    run.reason = None;
                    schedule.last_run = Some(run.clone());
                    schedule.awaiting_completion = true;
                    schedule.awaiting_run_id = Some(run_id);
                    changed = true;
                }
                Err(reason) => {
                    let run = &mut schedule.runs[run_index];
                    run.outcome = "missed".into();
                    run.reason = Some(format!("delivery_failed: {reason}"));
                    schedule.last_run = Some(run.clone());
                    schedule.missed_count += 1;
                    changed = true;
                }
            }
        }
        if schedule.runs.len() > MAX_HISTORY {
            let drop_n = schedule.runs.len() - MAX_HISTORY;
            schedule.runs.drain(0..drop_n);
        }
    }
    changed
}

pub struct WakeScheduler {
    state: Arc<Mutex<DaemonState>>,
    runtime: Arc<RuntimeStore>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    boot_at: f64,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl WakeScheduler {
    pub fn new(state: Arc<Mutex<DaemonState>>) -> std::io::Result<Self> {
        Ok(Self {
            state,
            runtime: runtime()?,
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            boot_at: now(),
            thread: Mutex::new(None),
        })
    }
    pub fn start(&self) -> std::io::Result<()> {
        let mut slot = self.thread.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_some() {
            return Ok(());
        }
        let state = Arc::clone(&self.state);
        let runtime = Arc::clone(&self.runtime);
        let stop = Arc::clone(&self.shutdown);
        let boot = self.boot_at;
        *slot = Some(
            thread::Builder::new()
                .name("cm-session-wakes".into())
                .spawn(move || {
                    let mut first = true;
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        Self::tick_runtime(&state, &runtime, boot, first);
                        first = false;
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
    pub fn tick_once(state: &Arc<Mutex<DaemonState>>) {
        if let Ok(runtime) = runtime() {
            Self::tick_runtime(state, &runtime, now(), false);
        }
    }
    fn tick_runtime(
        state: &Arc<Mutex<DaemonState>>,
        runtime: &Arc<RuntimeStore>,
        boot: f64,
        first: bool,
    ) {
        let mut targets = HashMap::new();
        let messaging_root;
        {
            let state = state.lock().unwrap_or_else(|p| p.into_inner());
            messaging_root = state.messaging_root.clone();
            for (uid, session) in &state.sessions {
                let agent = crate::agent_state::current(session);
                let pty_idle = crate::control::methods::session_pty_idle_for_schedule(session);
                targets.insert(
                    uid.clone(),
                    TargetSnapshot {
                        alive: !session.last_exit.kernel_set(),
                        turn_started_at: agent.engine_turn_started_at.or(agent.latest_start_at),
                        completed_turn_at: agent.last_turn.ended_at,
                        busy: !pty_idle,
                    },
                );
            }
        }
        let mut store = runtime.store.lock().unwrap_or_else(|p| p.into_inner());
        let mut publish = |uid: &str, run_id: &str, text: &str, marker: &str| {
            crate::notifications::publish(
                &messaging_root,
                uid,
                &format!("session-schedule:{run_id}"),
                "session-schedule",
                text,
                marker,
            )
            .map(|event| event["created_at"].as_f64().unwrap_or_else(now))
            .map_err(|e| e.to_string())
        };
        if tick_store(&mut store, now(), boot, first, &targets, &mut publish) {
            let _ = persist(runtime, &store);
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
    fn schedule(kind: WakeKind, next: f64) -> WakeSchedule {
        WakeSchedule {
            id: "s".into(),
            session_uid: "u".into(),
            kind,
            jitter_secs: 0,
            wake_message: None,
            enabled: true,
            created_at: 0.0,
            next_run_at: next,
            last_run: None,
            missed_count: 0,
            state: "active".into(),
            awaiting_completion: false,
            awaiting_run_id: None,
            runs: Vec::new(),
        }
    }
    fn run(store: &mut Store, at: f64, target: TargetSnapshot, calls: &mut Vec<String>) {
        let mut targets = HashMap::new();
        targets.insert("u".into(), target);
        let mut publish = |_: &str, id: &str, _: &str, _: &str| {
            calls.push(id.into());
            Ok(at)
        };
        assert!(tick_store(store, at, at, false, &targets, &mut publish));
    }
    #[test]
    fn parses_interval_and_daily() {
        assert_eq!(
            parse_kind(&json!({"interval_secs":21600})).unwrap(),
            WakeKind::Interval { every_secs: 21600 }
        );
        assert_eq!(
            parse_kind(&json!({"daily_at":"13:00Z"})).unwrap(),
            WakeKind::DailyAt {
                hour: 13,
                minute: 0
            }
        );
    }
    #[test]
    fn normal_delivery_and_busy_target_publish() {
        for busy in [false, true] {
            let mut s = Store {
                version: 1,
                schedules: vec![schedule(WakeKind::Interval { every_secs: 10 }, 1.0)],
            };
            let mut c: Vec<String> = Vec::new();
            run(
                &mut s,
                2.0,
                TargetSnapshot {
                    alive: true,
                    busy,
                    ..Default::default()
                },
                &mut c,
            );
            assert_eq!(c.len(), 1);
            assert_eq!(s.schedules[0].runs[0].outcome, "delivered");
        }
    }
    #[test]
    fn coalesces_until_completion_then_delivers() {
        let mut s = Store {
            version: 1,
            schedules: vec![schedule(WakeKind::Interval { every_secs: 10 }, 1.0)],
        };
        let mut c: Vec<String> = Vec::new();
        run(
            &mut s,
            2.0,
            TargetSnapshot {
                alive: true,
                ..Default::default()
            },
            &mut c,
        );
        s.schedules[0].next_run_at = 5.0;
        run(
            &mut s,
            5.0,
            TargetSnapshot {
                alive: true,
                ..Default::default()
            },
            &mut c,
        );
        assert_eq!(c.len(), 1);
        assert_eq!(s.schedules[0].runs[1].outcome, "coalesced");
        s.schedules[0].next_run_at = 15.0;
        run(
            &mut s,
            15.0,
            TargetSnapshot {
                alive: true,
                turn_started_at: Some(10.0),
                completed_turn_at: Some(15.0),
                ..Default::default()
            },
            &mut c,
        );
        assert_eq!(c.len(), 2);
    }
    #[test]
    fn completion_and_stale() {
        let mut s = Store {
            version: 1,
            schedules: vec![schedule(WakeKind::Interval { every_secs: 10 }, 1.0)],
        };
        let mut c: Vec<String> = Vec::new();
        run(
            &mut s,
            2.0,
            TargetSnapshot {
                alive: true,
                ..Default::default()
            },
            &mut c,
        );
        s.schedules[0].next_run_at = 11.0;
        run(
            &mut s,
            11.0,
            TargetSnapshot {
                alive: true,
                ..Default::default()
            },
            &mut c,
        );
        assert!(s.schedules[0].awaiting_completion);
        s.schedules[0].next_run_at = 12.0;
        run(
            &mut s,
            12.0,
            TargetSnapshot {
                alive: true,
                ..Default::default()
            },
            &mut c,
        );
        assert_eq!(c.len(), 2);
        assert_eq!(s.schedules[0].runs[0].reason.as_deref(), Some("stale"));
    }
    #[test]
    fn brain_down_and_pending_retry() {
        let mut s = Store {
            version: 1,
            schedules: vec![schedule(WakeKind::Interval { every_secs: 10 }, 1.0)],
        };
        let mut c: Vec<String> = Vec::new();
        let mut t = HashMap::new();
        t.insert(
            "u".into(),
            TargetSnapshot {
                alive: true,
                ..Default::default()
            },
        );
        let mut f = |_: &str, id: &str, _: &str, _: &str| {
            c.push(id.into());
            Ok(100.0)
        };
        assert!(tick_store(&mut s, 100.0, 100.0, true, &t, &mut f));
        assert_eq!(c.len(), 1);
        assert!(s.schedules[0]
            .runs
            .iter()
            .any(|r| r.reason.as_deref() == Some("brain_down")));
        let mut pending = schedule(WakeKind::Interval { every_secs: 10 }, 999.0);
        pending.runs.push(WakeRun {
            run_id: "same".into(),
            scheduled_at: 1.0,
            delivered_at: None,
            outcome: "pending".into(),
            reason: None,
            coalesced_into: None,
        });
        let mut p = Store {
            version: 1,
            schedules: vec![pending],
        };
        let mut pc: Vec<String> = Vec::new();
        let mut pf = |_: &str, id: &str, _: &str, _: &str| {
            pc.push(id.into());
            Ok(2.0)
        };
        tick_store(&mut p, 2.0, 2.0, false, &t, &mut pf);
        assert_eq!(pc, ["same"]);
    }
    #[test]
    fn dead_and_auth_refusal() {
        let mut s = Store {
            version: 1,
            schedules: vec![schedule(WakeKind::Interval { every_secs: 10 }, 1.0)],
        };
        let mut c: Vec<String> = Vec::new();
        run(&mut s, 2.0, TargetSnapshot::default(), &mut c);
        assert_eq!(
            s.schedules[0].runs[0].reason.as_deref(),
            Some("session_dead")
        );
        assert!(authorized(&DaemonState::new(), &Caller::session("caller"), "target").is_err());
    }
    #[test]
    fn daily_and_paused_shape() {
        let s = schedule(
            WakeKind::DailyAt {
                hour: 13,
                minute: 0,
            },
            0.0,
        );
        let at = chrono::DateTime::parse_from_rfc3339("2026-11-15T14:00:00Z")
            .unwrap()
            .timestamp() as f64;
        assert!(next_at(&s, at) > at);
        assert_eq!(
            public_schedule(&WakeSchedule {
                enabled: false,
                state: "paused".into(),
                ..s
            })["next_run"],
            Value::Null
        );
    }

    #[test]
    fn cancel_pause_update_recompute_next_run() {
        let mut s = schedule(WakeKind::Interval { every_secs: 10 }, 1.0);
        let old = s.next_run_at;
        s.next_run_at = next_at(&s, 5.0);
        assert!(s.next_run_at > old);
        s.enabled = false;
        s.state = "paused".into();
        assert_eq!(public_schedule(&s)["next_run"], Value::Null);
        s.state = "cancelled".into();
        s.next_run_at = 0.0;
        assert_eq!(public_schedule(&s)["next_run"], Value::Null);
    }
}
