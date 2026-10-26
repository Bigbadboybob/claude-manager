//! Session handoff (#25): move a CM session to another engine, model or
//! effort, keeping its CM identity.
//!
//! Three cases, cheapest first:
//! - **same engine, nothing to change**: a transcript rotation (#24,
//!   [`crate::rotation`]; Claude `--fork-session`, Codex `codex fork`);
//! - **same engine, new model/effort** (a *repick*): stop, resume the same
//!   conversation with the new `--model`/`--effort`;
//! - **another engine** (a *switch*): the outgoing agent replies with a
//!   handoff document (CM writes it to `~/.cm/handoffs/<uid>/<ts>.md`), CM
//!   stops it, spawns the new engine fresh at the SAME uid (chat identity,
//!   org seat, task, workspace, permissions and parent edge carry over),
//!   tells it to read the document, and confirms from the new engine's
//!   transcript. If the new engine does not confirm, CM resumes the old one
//!   and says so.
//!
//! A CM uid is one registry slot and the MCP auth identity, so the old and
//! new processes never overlap: stop → spawn → confirm → else resume the
//! old (Swarm-Coord ruling on #25). The old conversation stays resumable,
//! and the lineage is in the resume-identity archive.
//!
//! Every handoff waits for the same safe boundary as a rotation, and only
//! one handoff runs at a time on a host (bulk requests go one by one).
//! Shells and continuous sessions are refused: a continuous session's
//! engine is its task's setting, which the scheduler applies on each fire.

use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::control::protocol::ErrorCode;
use crate::rotation::Origin;
use crate::state::DaemonState;

/// `killed_by` of a handoff's stop: the uid comes straight back.
pub const HANDOFF_KILLER: &str = "handoff";

type MethodResult = Result<Value, (ErrorCode, String)>;

/// How long the outgoing agent has to reply with its handoff document.
const WRITE_DEADLINE: Duration = Duration::from_secs(10 * 60);
/// How long the new engine has to answer its first message.
const CONFIRM_DEADLINE: Duration = Duration::from_secs(5 * 60);
/// A queued request that never reached a boundary is dropped after this.
const DEFAULT_WAIT: Duration = Duration::from_secs(6 * 3600);
/// The most a `now` request waits in the queue.
const NOW_WAIT: Duration = Duration::from_secs(10 * 60);
const TICK: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Rotate,
    Repick,
    Switch,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Rotate => "rotate",
            Kind::Repick => "repick",
            Kind::Switch => "switch",
        }
    }
}

/// What a handoff asks for. `None` keeps the session's current value.
#[derive(Clone, Debug)]
pub struct Request {
    pub to_engine: Option<String>,
    pub to_model: Option<String>,
    pub to_effort: Option<String>,
    pub reason: String,
    pub origin: Origin,
    /// Waive the idle and Owner-quiet waits (an operator acting now).
    pub now: bool,
    /// Another session that asked for this one ("label (uid)"), when it is
    /// not the session itself or the operator.
    pub requested_by: Option<String>,
}

/// The daemon's engine name for what a caller wrote (`claude` → `claude-code`).
pub fn engine_name(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "claude" | "claude-code" => Some("claude-code"),
        "codex" => Some("codex"),
        _ => None,
    }
}

/// A validated plan: what will change.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub kind: Kind,
    pub from_engine: String,
    pub to_engine: String,
    pub to_model: Option<String>,
    pub to_effort: Option<String>,
}

/// Decide the kind of handoff for a session currently on
/// `engine`/`model`/`effort`, refusing what cannot be handed off.
pub fn plan(engine: &str, model: Option<&str>, effort: Option<&str>, req: &Request) -> Result<Plan, String> {
    if engine == "bash" {
        return Err("shell sessions are never handed off".into());
    }
    let from = engine_name(engine).ok_or_else(|| format!("{engine} sessions cannot be handed off"))?;
    let to = match req.to_engine.as_deref().filter(|e| !e.trim().is_empty()) {
        Some(raw) => engine_name(raw).ok_or_else(|| format!("unknown engine '{raw}' (claude or codex)"))?,
        None => from,
    };
    let key = crate::models::engine_key(to).expect("an agent engine");
    let to_model = match req.to_model.as_deref().filter(|m| !m.trim().is_empty()) {
        Some(m) => Some(crate::models::validate_model(m)?),
        None => None,
    };
    let to_effort = match req.to_effort.as_deref().filter(|e| !e.trim().is_empty()) {
        Some(e) => Some(crate::models::validate_effort(key, e)?),
        None => None,
    };
    let kind = if to != from {
        Kind::Switch
    } else if to_model.as_deref().is_some_and(|m| Some(m) != model) || to_effort.as_deref().is_some_and(|e| Some(e) != effort) {
        Kind::Repick
    } else {
        Kind::Rotate
    };
    Ok(Plan { kind, from_engine: from.into(), to_engine: to.into(), to_model, to_effort })
}

// ---- records ----------------------------------------------------------------

#[derive(Clone, Debug)]
struct Record {
    phase: &'static str,
    kind: Kind,
    origin: Origin,
    reason: String,
    requested_at: f64,
    started_at: Option<f64>,
    from_engine: String,
    to_engine: String,
    to_model: Option<String>,
    to_effort: Option<String>,
    from_id: Option<String>,
    to_id: Option<String>,
    handoff_file: Option<String>,
    deferred: Option<String>,
    outcome: Option<String>,
    finished_at: Option<f64>,
    requested_by: Option<String>,
}

impl Record {
    fn to_json(&self) -> Value {
        json!({
            "phase": self.phase,
            "kind": self.kind.as_str(),
            "origin": format!("{:?}", self.origin).to_ascii_lowercase(),
            "reason": self.reason,
            "requested_at": self.requested_at,
            "started_at": self.started_at,
            "from_engine": self.from_engine,
            "to_engine": self.to_engine,
            "to_model": self.to_model,
            "to_effort": self.to_effort,
            "from_transcript_id": self.from_id,
            "to_transcript_id": self.to_id,
            "handoff_file": self.handoff_file,
            "deferred": self.deferred,
            "outcome": self.outcome,
            "finished_at": self.finished_at,
            "requested_by": self.requested_by,
        })
    }
}

#[derive(Default)]
struct Registry {
    records: HashMap<String, Record>,
    queue: VecDeque<Queued>,
    /// Sessions being handed off now: any number at once, one per uid.
    running: std::collections::HashSet<String>,
}

struct Queued {
    uid: String,
    req: Request,
    plan: Plan,
    until: Instant,
}

fn registry() -> &'static Mutex<Registry> {
    static R: std::sync::OnceLock<Mutex<Registry>> = std::sync::OnceLock::new();
    R.get_or_init(|| Mutex::new(Registry::default()))
}

fn update(uid: &str, f: impl FnOnce(&mut Record)) {
    if let Some(r) = registry().lock().unwrap_or_else(|p| p.into_inner()).records.get_mut(uid) {
        f(r);
    }
}

fn now() -> f64 {
    crate::control::methods::now_unix_f64()
}

/// The latest handoff of `uid` (queued, running or finished).
pub fn status(uid: &str) -> Option<Value> {
    registry().lock().unwrap_or_else(|p| p.into_inner()).records.get(uid).map(Record::to_json)
}

/// Claim `uid` for a handoff unless a handoff or rotation of that same
/// session is running. Handoffs of different sessions run in parallel (Owner,
/// 2026-10-26); only one session's old and new engine must never overlap.
/// Called with the state lock held (`_locked`), as rotation's `admit` is, so
/// the two never both get the same uid.
fn take_slot(_locked: &DaemonState, reg: &mut Registry, uid: &str) -> bool {
    if reg.running.contains(uid) || crate::rotation::in_progress(uid) {
        return false;
    }
    reg.running.insert(uid.to_string());
    true
}

/// Tests: forget `uid`'s queued handoff.
#[cfg(test)]
pub(crate) fn test_drop_queued(uid: &str) {
    let mut reg = registry().lock().unwrap_or_else(|p| p.into_inner());
    reg.queue.retain(|q| q.uid != uid);
    reg.records.remove(uid);
}

/// A session being handed off now, if any.
pub fn running() -> Option<String> {
    registry().lock().unwrap_or_else(|p| p.into_inner()).running.iter().next().cloned()
}

/// Whether `uid`'s process is being stopped and replaced right now.
pub fn in_progress(uid: &str) -> bool {
    registry().lock().unwrap_or_else(|p| p.into_inner()).running.contains(uid)
}

/// A failed handoff: recorded, and Owner is alerted when it left the uid not
/// running.
/// `killed_by` on the `Exited` viewers get when a handoff leaves the session
/// not running (see `rotation::announce_not_back`).
pub const KILLED_BY_FAILED: &str = "handoff failed";

fn fail(state_arc: &Arc<Mutex<DaemonState>>, uid: &str, outcome: String) {
    finish(uid, "failed", outcome.clone());
    let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
    if !state.sessions.contains_key(uid) {
        crate::rotation::announce_not_back(&state, uid, KILLED_BY_FAILED);
        let message = format!(
            "A handoff of this session failed and it is NOT running now ({outcome}). A-R revives it."
        );
        if let Err(e) = crate::owner_attention::raise_system(&state, uid, &message) {
            eprintln!("cm-daemon: handoff alert for {uid}: {e}");
        }
    }
}

/// After a brain restart mid-handoff, with the uid live. On the old engine
/// nothing happened yet; a handoff already recorded in the lineage finished
/// before the restart. Otherwise the new engine came up but was not
/// confirmed: send its takeover notice again and confirm (or roll back to
/// the old engine) exactly as the normal path does, on a worker thread.
/// Returns whether that worker took the marker over.
pub(crate) fn recover_live(state_arc: &Arc<Mutex<DaemonState>>, uid: &str, snap: crate::rotation::Snapshot, file: Option<&str>) -> bool {
    let (engine, done) = {
        let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        (
            st.sessions.get(uid).map(|s| s.session_type.clone()),
            crate::resume_identity::handoff_recorded(&st, uid, &snap.from_id),
        )
    };
    let (Some(engine), Some(file)) = (engine, file) else {
        eprintln!("cm-daemon: handoff of {uid} was interrupted by a restart before the switch; nothing to do");
        return false;
    };
    if engine == snap.entry.session_type {
        eprintln!("cm-daemon: handoff of {uid} was interrupted by a restart before the switch; nothing to do");
        return false;
    }
    if done {
        eprintln!("cm-daemon: handoff of {uid} had finished before the restart; nothing to do");
        return false;
    }
    let file = PathBuf::from(file);
    let state = Arc::clone(state_arc);
    let uid = uid.to_string();
    crate::rotation::hold(&state_arc.lock().unwrap_or_else(|p| p.into_inner()), &uid, crate::rotation::HoldOwner::Handoff);
    {
        let mut reg = registry().lock().unwrap_or_else(|p| p.into_inner());
        reg.running.insert(uid.clone());
        reg.records.insert(uid.clone(), Record {
            phase: "confirming",
            kind: Kind::Switch,
            origin: Origin::Operator,
            reason: "completing a handoff a daemon restart interrupted".into(),
            requested_at: now(),
            started_at: Some(now()),
            from_engine: snap.entry.session_type.clone(),
            to_engine: engine.clone(),
            to_model: None,
            to_effort: None,
            from_id: Some(snap.from_id.clone()).filter(|id| !id.is_empty()),
            to_id: None,
            handoff_file: Some(file.to_string_lossy().into_owned()),
            deferred: None,
            outcome: None,
            finished_at: None,
            requested_by: None,
        });
    }
    let worker_uid = uid.clone();
    let spawned = std::thread::Builder::new().name(format!("cm-handoff-recover-{uid}")).spawn(move || {
        let uid = worker_uid;
        let from = who(&snap.entry.session_type, snap.entry.model.as_deref());
        let to = who(&engine, None);
        confirm_takeover(&state, &uid, &snap, &file, &from, &to, &engine, "resumed after a daemon restart");
        crate::rotation::inflight_remove(&state.lock().unwrap_or_else(|p| p.into_inner()), &uid);
    });
    if spawned.is_err() {
        registry().lock().unwrap_or_else(|p| p.into_inner()).running.remove(&uid);
    }
    spawned.is_ok()
}

fn finish(uid: &str, phase: &'static str, outcome: String) {
    eprintln!("cm-daemon: handoff of {uid}: {outcome}");
    crate::rotation::unhold(uid, crate::rotation::HoldOwner::Handoff);
    let mut reg = registry().lock().unwrap_or_else(|p| p.into_inner());
    if let Some(r) = reg.records.get_mut(uid) {
        r.phase = phase;
        r.outcome = Some(outcome);
        r.finished_at = Some(now());
    }
    reg.running.remove(uid);
}

// ---- requests -----------------------------------------------------------------

/// Validate a handoff of `uid` and queue it for its next safe boundary
/// (`req.now`: check the boundary now, without the idle and Owner-quiet
/// waits, and refuse if it fails). Returns the queued record.
pub fn request(state_arc: &Arc<Mutex<DaemonState>>, uid: &str, req: Request, wait: Option<Duration>) -> MethodResult {
    let plan = {
        let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        let sess = state
            .sessions
            .get(uid)
            .ok_or_else(|| (ErrorCode::NotFound, format!("session '{uid}' is not live on this host")))?;
        if sess.continuous_task_id.is_some() {
            return Err((
                ErrorCode::Conflict,
                "continuous sessions are scheduler-owned: change the continuous task's engine or model instead".into(),
            ));
        }
        if sess.workflow_run_id.is_some() {
            return Err((
                ErrorCode::Conflict,
                "workflow participants are driven by their run: stop the workflow first".into(),
            ));
        }
        let plan = plan(&sess.session_type, sess.model.as_deref(), sess.effort.as_deref(), &req)
            .map_err(|e| (ErrorCode::InvalidParams, e))?;
        if req.now {
            boundary(&state, uid, &req).map_err(|why| (ErrorCode::Conflict, format!("not at a safe boundary: {why}")))?;
        }
        plan
    };
    let mut reg = registry().lock().unwrap_or_else(|p| p.into_inner());
    if reg.running.contains(uid) {
        return Err((ErrorCode::Conflict, "a handoff of this session is already running".into()));
    }
    reg.queue.retain(|q| q.uid != uid);
    // The target's notices quote the reason: it says who asked.
    let mut req = req;
    if let Some(by) = &req.requested_by {
        req.reason = if req.reason.trim().is_empty() {
            format!("requested by {by}")
        } else {
            format!("{}; requested by {by}", req.reason)
        };
    }
    let record = Record {
        phase: "queued",
        kind: plan.kind,
        origin: req.origin,
        reason: req.reason.clone(),
        requested_at: now(),
        started_at: None,
        from_engine: plan.from_engine.clone(),
        to_engine: plan.to_engine.clone(),
        to_model: plan.to_model.clone(),
        to_effort: plan.to_effort.clone(),
        from_id: None,
        to_id: None,
        handoff_file: None,
        deferred: None,
        outcome: None,
        finished_at: None,
        requested_by: req.requested_by.clone(),
    };
    let out = record.to_json();
    reg.records.insert(uid.to_string(), record);
    // A `now` request passed its boundary check just now; if it still
    // cannot start soon (another handoff or rotation holds the host), it is
    // dropped rather than run long after Owner asked.
    let wait = if req.now { wait.unwrap_or(NOW_WAIT).min(NOW_WAIT) } else { wait.unwrap_or(DEFAULT_WAIT) };
    let until = Instant::now() + wait;
    if req.now {
        reg.queue.push_front(Queued { uid: uid.into(), req, plan, until });
    } else {
        reg.queue.push_back(Queued { uid: uid.into(), req, plan, until });
    }
    drop(reg);
    tick(state_arc);
    Ok(json!({ "ok": true, "session_uid": uid, "queued": true, "handoff": out }))
}

/// Queue a handoff for several sessions (Owner's bulk use). Each runs at its
/// own boundary, in parallel. Returns what was queued and what was refused.
pub fn request_many(state_arc: &Arc<Mutex<DaemonState>>, uids: &[String], req: &Request, wait: Option<Duration>) -> Value {
    let mut queued = Vec::new();
    let mut refused = Vec::new();
    for uid in uids {
        let mut one = req.clone();
        one.now = false;
        match request(state_arc, uid, one, wait) {
            Ok(_) => queued.push(json!(uid)),
            Err((_, why)) => refused.push(json!({"session_uid": uid, "reason": why})),
        }
    }
    json!({"ok": true, "queued": queued, "refused": refused})
}

/// The live sessions a bulk selection names: every agent session on
/// `from_engine` (or both engines), never shells, continuous sessions or
/// workflow participants.
pub fn select(state: &DaemonState, from_engine: Option<&str>) -> Vec<String> {
    let mut uids: Vec<String> = state
        .sessions
        .values()
        .filter(|s| matches!(s.session_type.as_str(), "claude-code" | "codex"))
        .filter(|s| from_engine.is_none_or(|e| s.session_type == e))
        .filter(|s| s.continuous_task_id.is_none() && s.workflow_run_id.is_none())
        .map(|s| s.uid.clone())
        .collect();
    uids.sort();
    uids
}

fn boundary(state: &DaemonState, uid: &str, req: &Request) -> Result<(), String> {
    let c = &state.config.transcript_rotation;
    if crate::rotation::in_progress(uid) {
        return Err("a transcript rotation is running".into());
    }
    // `now`, and an operator's (Owner's) request, skip the idle and
    // Owner-quiet waits: Owner ordered it. An operator request still waits
    // out a turn, a job and an unsent draft; `now` waives the draft too.
    let (idle, quiet, draft) = if req.now {
        (Duration::ZERO, Duration::ZERO, false)
    } else if req.origin == Origin::Operator {
        (Duration::ZERO, Duration::ZERO, true)
    } else {
        (Duration::from_secs(c.idle_secs), Duration::from_secs(c.owner_quiet_secs), true)
    };
    crate::rotation::boundary_checked(
        state,
        uid,
        &["claude-code", "codex"],
        crate::rotation::Checks { idle, owner_quiet: quiet, draft, at_turn_end: false },
    )
}

/// Start every queued handoff whose session is at a safe boundary; they run
/// in parallel. Expired requests are dropped and recorded.
pub fn tick(state_arc: &Arc<Mutex<DaemonState>>) {
    let budget = registry().lock().unwrap_or_else(|p| p.into_inner()).queue.len();
    for _ in 0..budget {
        if !tick_one(state_arc) {
            break;
        }
    }
}

/// Start the first queued handoff at a safe boundary. Returns whether one
/// was taken off the queue (started, or failed to start).
fn tick_one(state_arc: &Arc<Mutex<DaemonState>>) -> bool {
    let next = {
        let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        // Take the queue out: the boundary check reads this registry
        // (`in_progress`), so it must not be held across it.
        let queue = {
            let mut reg = registry().lock().unwrap_or_else(|p| p.into_inner());
            std::mem::take(&mut reg.queue)
        };
        let mut chosen = None;
        let mut keep = VecDeque::new();
        let mut expired = Vec::new();
        let mut deferred = Vec::new();
        for q in queue {
            if chosen.is_some() {
                keep.push_back(q);
                continue;
            }
            if !state.sessions.contains_key(&q.uid) {
                expired.push((q.uid.clone(), "the session is no longer live".to_string()));
                continue;
            }
            match boundary(&state, &q.uid, &q.req) {
                Ok(()) => chosen = Some(q),
                Err(why) if Instant::now() >= q.until => {
                    expired.push((q.uid.clone(), format!("never reached a safe boundary (last: {why})")))
                }
                Err(why) => {
                    deferred.push((q.uid.clone(), why));
                    keep.push_back(q);
                }
            }
        }
        let mut reg = registry().lock().unwrap_or_else(|p| p.into_inner());
        // Requests queued meanwhile go after the ones already waiting.
        keep.extend(std::mem::take(&mut reg.queue));
        reg.queue = keep;
        for (uid, why) in deferred {
            if let Some(r) = reg.records.get_mut(&uid) {
                r.deferred = Some(why);
            }
        }
        for (uid, why) in expired {
            if let Some(r) = reg.records.get_mut(&uid) {
                r.phase = "dropped";
                r.outcome = Some(why);
                r.finished_at = Some(now());
            }
        }
        let Some(q) = chosen else { return false };
        let snap = match crate::rotation::snapshot(&state, &q.uid) {
            Ok(s) => Some(s),
            // A repick or a switch needs no transcript (a session that never
            // bound one restarts fresh); a rotation does.
            Err(_) if q.plan.kind != Kind::Rotate => fresh_snapshot(&state, &q.uid),
            Err(_) => None,
        };
        let Some(snap) = snap else {
            if let Some(r) = reg.records.get_mut(&q.uid) {
                r.phase = "failed";
                r.outcome = Some("the session has no transcript to hand off from".into());
                r.finished_at = Some(now());
            }
            return true;
        };
        if q.plan.kind != Kind::Rotate && !take_slot(&state, &mut reg, &q.uid) {
            reg.queue.push_front(q);
            return false;
        }
        if q.plan.kind != Kind::Rotate {
            // Wakes wait for whatever runs at the uid once this is done, so
            // none gets in ahead of the handoff request or the takeover.
            crate::rotation::hold(&state, &q.uid, crate::rotation::HoldOwner::Handoff);
        }
        if let Some(r) = reg.records.get_mut(&q.uid) {
            r.phase = "starting";
            r.started_at = Some(now());
            r.deferred = None;
            r.from_id = Some(snap.from_id.clone()).filter(|id| !id.is_empty());
        }
        (q, snap)
    };
    let (q, snap) = next;
    if q.plan.kind == Kind::Rotate {
        // The same engine with nothing to change: #24's rotation (it checks
        // the boundary itself and keeps its own record).
        let reason = if q.req.reason.is_empty() { "handoff".to_string() } else { q.req.reason.clone() };
        match crate::rotation::start(state_arc, &q.uid, &reason, q.req.origin, q.req.now) {
            Ok(_) => finish(&q.uid, "done", "same engine and model: rotated the transcript (see rotation status)".into()),
            Err((_, e)) => finish(&q.uid, "failed", format!("rotation refused: {e}")),
        }
        return true;
    }
    let state = Arc::clone(state_arc);
    let uid = q.uid.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("cm-handoff-{uid}"))
        .spawn(move || {
            // A brain restart mid-handoff resumes the old engine and
            // conversation (#24's in-flight marker).
            if let Err(e) = crate::rotation::inflight_put_with(
                &state.lock().unwrap_or_else(|p| p.into_inner()),
                &q.uid,
                &snap,
                json!({"kind": "handoff"}),
            ) {
                eprintln!("cm-daemon: handoff of {}: in-flight marker not written: {e}", q.uid);
            }
            match q.plan.kind {
                Kind::Repick => run_repick(&state, &q.uid, &q.plan, &q.req, snap),
                _ => run_switch(&state, &q.uid, &q.plan, &q.req, snap),
            }
            crate::rotation::inflight_remove(&state.lock().unwrap_or_else(|p| p.into_inner()), &q.uid);
        });
    if let Err(e) = spawned {
        finish(&uid, "failed", format!("handoff worker: {e}"));
    }
    true
}

/// The snapshot of a session that never bound a transcript.
fn fresh_snapshot(state: &DaemonState, uid: &str) -> Option<crate::rotation::Snapshot> {
    let sess = state.sessions.get(uid)?;
    let worktree = state.workspaces.get(&sess.workspace_id)?.worktree_path.clone()?;
    let mut entry = crate::resume_identity::entry(state, sess);
    entry.last_exit = None;
    entry.reported_done_at = None;
    entry.report_reason = None;
    Some(crate::rotation::Snapshot {
        entry,
        workspace_id: sess.workspace_id.clone(),
        worktree,
        from_path: PathBuf::new(),
        from_id: String::new(),
        from_bytes: 0,
    })
}

/// Run queued handoffs: a background thread, every [`TICK`].
pub fn start_worker(state: &Arc<Mutex<DaemonState>>) {
    let state = Arc::clone(state);
    let _ = std::thread::Builder::new().name("cm-handoff-queue".into()).spawn(move || loop {
        std::thread::sleep(TICK);
        tick(&state);
    });
}

// ---- the handoff document -----------------------------------------------------

const OPEN: &str = "<cm-handoff>";
const CLOSE: &str = "</cm-handoff>";

fn who(engine: &str, model: Option<&str>) -> String {
    let name = if engine == "codex" { "Codex" } else { "Claude" };
    match model {
        Some(m) => format!("{name} ({m})"),
        None => name.to_string(),
    }
}

/// What the outgoing agent is asked to reply with.
pub fn write_request(marker: &str, to: &str, reason: &str) -> String {
    let why = if reason.trim().is_empty() { String::new() } else { format!(" Reason: {reason}.") };
    format!(
        "[cm-handoff-request {marker}] CM is handing this session over to {to}, at the same CM \
         identity (chat name, task, workspace, permissions, board holds).{why} Your successor \
         starts with no memory of this conversation. Reply with a handoff document for it and \
         nothing else, between a line {OPEN} and a line {CLOSE}. Include: the goal and who asked \
         for it; where things stand; open items and holds by board and number (for example \
         swarm-focused-design #25); waits and their ETAs; decisions, standing rules and Owner \
         rulings that still apply; branches, commits and files that matter; and the exact next \
         step. Do not start new work. End your turn after the reply."
    )
}

/// The new engine's first message.
pub fn takeover_notice(marker: &str, label: &str, from: &str, reason: &str, file: &Path) -> String {
    let why = if reason.trim().is_empty() { String::new() } else { format!(" ({reason})") };
    format!(
        "[cm-handoff {marker}] You are taking over the CM session '{label}' from {from}{why}. \
         You keep its CM identity: chat name, task, workspace, permissions and board holds. \
         Your predecessor wrote a handoff document for you: {}. Read it first. Then re-read from \
         CM rather than assuming: your board items, standing rules and Owner rulings, and your \
         chat inbox. Your predecessor's monitors stopped with it: check list_monitors and \
         re-arm any watch the document says you still need (monitor_sessions). Then continue \
         with the next step it names.",
        file.display()
    )
}

fn repick_notice(marker: &str, model: &str, effort: &str) -> String {
    format!(
        "[cm-handoff {marker}] CM restarted this session on {model} at {effort} effort. Same \
         conversation, CM identity, task and workspace. Re-read your board items and inbox from \
         CM if anything may have moved, then carry on."
    )
}

fn fallback_notice(why: &str, file: Option<&Path>) -> String {
    let doc = file.map(|f| format!(" Your handoff document is kept at {}.", f.display())).unwrap_or_default();
    format!(
        "[cm-handoff] CM tried to hand this session over, but it did not take ({why}). You are \
         back on your own conversation; nothing was lost.{doc} Carry on."
    )
}

/// Text of a transcript row written by the assistant (Claude or Codex).
fn assistant_text(row: &Value) -> Option<String> {
    let content = if row["type"] == "assistant" {
        row.pointer("/message/content")?
    } else if row["type"] == "response_item" && row.pointer("/payload/role").and_then(Value::as_str) == Some("assistant") {
        row.pointer("/payload/content")?
    } else {
        return None;
    };
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    let parts: Vec<&str> = content
        .as_array()?
        .iter()
        .filter(|i| matches!(i["type"].as_str(), Some("text" | "output_text")))
        .filter_map(|i| i["text"].as_str())
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

/// The handoff document an agent replied with after the request carrying
/// `marker`: the text between [`OPEN`] and [`CLOSE`] in its assistant rows
/// since then. Reads only the file's tail.
pub fn extract_document(path: &Path, marker: &str) -> Option<String> {
    let text = tail(path)?;
    let needle = format!("[cm-handoff-request {marker}]");
    // The marker is a fresh nonce, so its first occurrence is the request
    // itself. Claude also appends a `last-prompt` row repeating the request
    // AFTER the reply; searching from the last occurrence missed the reply.
    let after = &text[text.find(&needle)?..];
    let mut reply = String::new();
    for line in after.lines().skip(1) {
        let Ok(row) = serde_json::from_str::<Value>(line) else { continue };
        if let Some(t) = assistant_text(&row) {
            reply.push_str(&t);
            reply.push('\n');
        }
    }
    let start = reply.find(OPEN)? + OPEN.len();
    let end = start + reply[start..].find(CLOSE)?;
    let doc = reply[start..end].trim();
    (!doc.is_empty()).then(|| doc.to_string())
}

/// The last few MB of a transcript (they only grow; the rows we look for
/// are recent).
fn tail(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL: u64 = 8 * 1024 * 1024;
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(TAIL))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Where a handoff document goes: `~/.cm/handoffs/<uid>/<unix ts>.md`.
fn document_path(uid: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".cm/handoffs").join(uid).join(format!("{}.md", now() as u64)))
}

fn write_document(path: &Path, header: &str, doc: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    writeln!(f, "{header}\n\n{doc}")
}

// ---- running a handoff --------------------------------------------------------

fn live_transcript(state_arc: &Arc<Mutex<DaemonState>>, uid: &str) -> (bool, Option<PathBuf>) {
    let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
    let sess = st.sessions.get(uid);
    (sess.is_some(), sess.and_then(|s| s.transcript_path.clone()).map(PathBuf::from))
}

fn idle_since(state_arc: &Arc<Mutex<DaemonState>>, uid: &str, after: f64) -> bool {
    let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
    st.sessions.get(uid).is_some_and(|s| {
        let a = crate::agent_state::current(s);
        a.state == crate::agent_state::State::Idle && a.since >= after
    })
}

/// Wait until `uid`'s (new) transcript, other than `old`, records `needle`
/// and an assistant reply after it.
fn confirmed(state_arc: &Arc<Mutex<DaemonState>>, uid: &str, old: &Path, needle: &str) -> Result<(PathBuf, u64), String> {
    let deadline = Instant::now() + CONFIRM_DEADLINE;
    let started = now();
    while Instant::now() < deadline {
        let (live, path) = live_transcript(state_arc, uid);
        if !live {
            return Err("the new session exited before answering".into());
        }
        // An engine failure (bad model, auth, rate limit) will not answer:
        // fall back now rather than at the deadline.
        if let Some(kind) = errored_since(state_arc, uid, started) {
            return Err(format!("the new session errored ({kind})"));
        }
        if let Some(p) = path.filter(|p| p.as_path() != old) {
            if replied_after(&p, needle) {
                let len = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                return Ok((p, len));
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err("the new session did not answer its first message (API error or a menu?)".into())
}

/// The error kind, when `uid`'s agent errored after `since`.
fn errored_since(state_arc: &Arc<Mutex<DaemonState>>, uid: &str, since: f64) -> Option<String> {
    let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
    let a = crate::agent_state::current(st.sessions.get(uid)?);
    (a.state == crate::agent_state::State::Errored && a.since >= since).then(|| {
        a.detail.error_kind.clone().unwrap_or_else(|| "engine error".into())
    })
}

/// Whether `path` holds `needle` followed by an assistant row.
fn replied_after(path: &Path, needle: &str) -> bool {
    let Some(text) = tail(path) else { return false };
    let Some(at) = text.rfind(needle) else { return false };
    text[at..].lines().skip(1).any(|l| serde_json::from_str::<Value>(l).is_ok_and(|row| assistant_text(&row).is_some()))
}

fn stop(state_arc: &Arc<Mutex<DaemonState>>, uid: &str) -> crate::rotation::Stopped {
    crate::rotation::stop_tracked(state_arc, uid, HANDOFF_KILLER, "handoff")
}

/// Respawn the snapshot's own engine and conversation after a failed step.
fn restore_old(state_arc: &Arc<Mutex<DaemonState>>, uid: &str, snap: &crate::rotation::Snapshot, why: &str, file: Option<&Path>) {
    update(uid, |r| r.phase = "falling back");
    if state_arc.lock().unwrap_or_else(|p| p.into_inner()).sessions.contains_key(uid)
        && stop(state_arc, uid) == crate::rotation::Stopped::Stuck
    {
        fail(state_arc, uid, format!("{why}; the new process is still running 10 min after its kill"));
        return;
    }
    match crate::control::methods::restore_one_session_launch(state_arc, &snap.workspace_id, &snap.worktree, &snap.entry, None, false) {
        Ok(_) => {
            crate::rotation::deliver(state_arc, uid, fallback_notice(why, file));
            fail(state_arc, uid, format!("{why}; resumed the original {} session", snap.entry.session_type));
        }
        Err((_, e)) => fail(state_arc, uid, format!("{why}; resuming the original also failed: {e}")),
    }
}

fn set_pick(entry: &mut crate::manifest::ManifestEntry, plan: &Plan) {
    if plan.kind == Kind::Switch {
        // Another engine: the old engine's model and effort do not apply.
        entry.model = None;
        entry.model_source = None;
        entry.effort = None;
        entry.effort_source = None;
    }
    if let Some(m) = &plan.to_model {
        entry.model = Some(m.clone());
        entry.model_source = Some(crate::models::ModelSource::Explicit.as_str().into());
    }
    if let Some(e) = &plan.to_effort {
        entry.effort = Some(e.clone());
        entry.effort_source = Some(crate::models::ModelSource::Explicit.as_str().into());
    }
}

fn run_repick(state_arc: &Arc<Mutex<DaemonState>>, uid: &str, plan: &Plan, _req: &Request, snap: crate::rotation::Snapshot) {
    update(uid, |r| r.phase = "stopping");
    match stop(state_arc, uid) {
        crate::rotation::Stopped::Yes => {}
        crate::rotation::Stopped::Late => {
            restore_old(state_arc, uid, &snap, "the session stopped only after a delay", None);
            return;
        }
        crate::rotation::Stopped::Stuck => {
            fail(state_arc, uid, "the kill was sent but the process is still running after 10 min".into());
            return;
        }
    }
    let mut entry = snap.entry.clone();
    set_pick(&mut entry, plan);
    update(uid, |r| r.phase = "restarting");
    if let Err((_, e)) = crate::control::methods::restore_one_session_launch(state_arc, &snap.workspace_id, &snap.worktree, &entry, None, false) {
        restore_old(state_arc, uid, &snap, &format!("the restart did not start: {e}"), None);
        return;
    }
    let (model, effort) = {
        let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        let s = st.sessions.get(uid);
        (s.and_then(|s| s.model.clone()).unwrap_or_default(), s.and_then(|s| s.effort.clone()).unwrap_or_default())
    };
    let marker = crate::rotation::nonce();
    update(uid, |r| r.phase = "confirming");
    if !crate::rotation::deliver(state_arc, uid, repick_notice(&marker, &model, &effort)) {
        restore_old(state_arc, uid, &snap, "the restarted session vanished before its first message", None);
        return;
    }
    // A resume keeps a Claude transcript; a Codex resume may write a new
    // rollout. Either way the notice and a reply must land.
    let needle = format!("[cm-handoff {marker}]");
    match confirmed(state_arc, uid, Path::new(""), &needle) {
        Ok((path, _)) => {
            let id = crate::session::transcript_id_from_path(&path.to_string_lossy());
            update(uid, |r| r.to_id = id);
            finish(uid, "done", format!("restarted on {model} at {effort} effort, same conversation"));
        }
        Err(why) => restore_old(state_arc, uid, &snap, &why, None),
    }
}

fn run_switch(state_arc: &Arc<Mutex<DaemonState>>, uid: &str, plan: &Plan, req: &Request, snap: crate::rotation::Snapshot) {
    let from_who = who(&plan.from_engine, snap.entry.model.as_deref());
    let to_who = who(&plan.to_engine, plan.to_model.as_deref());
    // 1. The outgoing agent writes its handoff document (as a reply: CM
    //    writes the file, so a sandboxed engine needs no write access).
    update(uid, |r| r.phase = "writing handoff");
    let marker = crate::rotation::nonce();
    let asked_at = now();
    if !crate::rotation::deliver_live(state_arc, uid, write_request(&marker, &to_who, &req.reason)) {
        fail(state_arc, uid, "the session vanished before the handoff request".into());
        return;
    }
    let deadline = Instant::now() + WRITE_DEADLINE;
    let doc = loop {
        let (live, path) = live_transcript(state_arc, uid);
        if !live {
            fail(state_arc, uid, "the session exited while writing its handoff; nothing was started".into());
            return;
        }
        let doc = path.as_deref().and_then(|p| extract_document(p, &marker));
        if let Some(doc) = doc.filter(|_| idle_since(state_arc, uid, asked_at)) {
            break doc;
        }
        if Instant::now() >= deadline {
            crate::rotation::deliver_live(
                state_arc,
                uid,
                "[cm-handoff] CM cancelled the handoff: no handoff document arrived in time. Nothing changed; carry on.".into(),
            );
            fail(state_arc, uid, "no handoff document within 10 min; nothing changed".into());
            return;
        }
        std::thread::sleep(Duration::from_secs(1));
    };
    let Some(file) = document_path(uid) else {
        fail(state_arc, uid, "no HOME to write the handoff document under; nothing changed".into());
        return;
    };
    let header = format!(
        "# Handoff: {} ({uid})\n\nFrom {from_who} (transcript {}) to {to_who}, written {}.",
        snap.entry.label,
        if snap.from_id.is_empty() { "none" } else { &snap.from_id },
        crate::control::methods::fmt_unix_utc(now()),
    );
    if let Err(e) = write_document(&file, &header, &doc) {
        fail(state_arc, uid, format!("could not write the handoff document: {e}; nothing changed"));
        return;
    }
    update(uid, |r| r.handoff_file = Some(file.to_string_lossy().into_owned()));
    if let Err(e) = crate::rotation::inflight_put_with(
        &state_arc.lock().unwrap_or_else(|p| p.into_inner()),
        uid,
        &snap,
        json!({"kind": "handoff", "file": file}),
    ) {
        eprintln!("cm-daemon: handoff of {uid}: in-flight marker not updated: {e}");
    }
    // 2. Stop the old engine.
    update(uid, |r| r.phase = "stopping");
    match stop(state_arc, uid) {
        crate::rotation::Stopped::Yes => {}
        crate::rotation::Stopped::Late => {
            restore_old(state_arc, uid, &snap, "the session stopped only after a delay", None);
            return;
        }
        crate::rotation::Stopped::Stuck => {
            fail(state_arc, uid, "the kill was sent but the process is still running after 10 min".into());
            return;
        }
    }
    // 3. The new engine, fresh, at the same uid.
    let mut entry = snap.entry.clone();
    entry.session_type = plan.to_engine.clone();
    entry.transcript_id = None;
    entry.transcript_path = None;
    set_pick(&mut entry, plan);
    update(uid, |r| r.phase = "spawning");
    if let Err((_, e)) = crate::control::methods::restore_one_session_launch(state_arc, &snap.workspace_id, &snap.worktree, &entry, None, false) {
        restore_old(state_arc, uid, &snap, &format!("{} did not start: {e}", to_who), Some(&file));
        return;
    }
    // 4. Its first message: read the document.
    confirm_takeover(state_arc, uid, &snap, &file, &from_who, &to_who, &plan.to_engine, &req.reason);
}

/// Tell the new engine to take over, confirm from its transcript, record the
/// lineage; else roll back to the old engine.
#[allow(clippy::too_many_arguments)]
fn confirm_takeover(
    state_arc: &Arc<Mutex<DaemonState>>,
    uid: &str,
    snap: &crate::rotation::Snapshot,
    file: &Path,
    from_who: &str,
    to_who: &str,
    to_engine: &str,
    reason: &str,
) {
    let marker = crate::rotation::nonce();
    update(uid, |r| r.phase = "confirming");
    if !crate::rotation::deliver(state_arc, uid, takeover_notice(&marker, &snap.entry.label, from_who, reason, file)) {
        restore_old(state_arc, uid, snap, "the new session vanished before its first message", Some(file));
        return;
    }
    let needle = format!("[cm-handoff {marker}]");
    match confirmed(state_arc, uid, &snap.from_path, &needle) {
        Ok((path, bytes)) => {
            let to_id = crate::session::transcript_id_from_path(&path.to_string_lossy());
            update(uid, |r| r.to_id = to_id.clone());
            {
                let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
                let lineage = crate::resume_identity::Handoff {
                    uid: uid.into(),
                    from_engine: snap.entry.session_type.clone(),
                    from_id: snap.from_id.clone(),
                    to_engine: to_engine.into(),
                    to_id: to_id.clone().unwrap_or_default(),
                    file: file.to_string_lossy().into_owned(),
                    at: now(),
                };
                if let Err(e) = crate::resume_identity::record_handoff(&st, lineage) {
                    eprintln!("cm-daemon: handoff of {uid}: lineage not recorded: {e}");
                }
            }
            finish(
                uid,
                "done",
                format!(
                    "handed off from {from_who} to {to_who}: transcript {} ({} so far), document {}",
                    to_id.as_deref().unwrap_or("?"),
                    bytes,
                    file.display()
                ),
            );
        }
        Err(why) => restore_old(state_arc, uid, snap, &why, Some(file)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(engine: Option<&str>, model: Option<&str>, effort: Option<&str>) -> Request {
        Request {
            to_engine: engine.map(str::to_string),
            to_model: model.map(str::to_string),
            to_effort: effort.map(str::to_string),
            reason: String::new(),
            origin: Origin::Operator,
            now: false,
            requested_by: None,
        }
    }

    #[test]
    fn plan_picks_the_cheapest_kind_and_refuses_shells() {
        let p = plan("claude-code", Some("claude-opus-5-5"), Some("xhigh"), &req(None, None, None)).unwrap();
        assert_eq!(p.kind, Kind::Rotate);
        let p = plan("claude-code", Some("claude-opus-5-5"), Some("xhigh"), &req(Some("claude"), Some("claude-opus-5-5"), None)).unwrap();
        assert_eq!(p.kind, Kind::Rotate, "the same model is no change");
        let p = plan("claude-code", Some("claude-opus-5-5"), Some("xhigh"), &req(None, None, Some("high"))).unwrap();
        assert_eq!(p.kind, Kind::Repick);
        let p = plan("claude-code", None, None, &req(Some("codex"), Some("gpt-6-luna"), None)).unwrap();
        assert_eq!((p.kind, p.to_engine.as_str(), p.to_model.as_deref()), (Kind::Switch, "codex", Some("gpt-6-luna")));
        assert!(plan("bash", None, None, &req(Some("codex"), None, None)).unwrap_err().contains("never"));
        assert!(plan("claude-code", None, None, &req(Some("gemini"), None, None)).is_err());
        assert!(plan("codex", None, None, &req(None, None, Some("max"))).is_err(), "codex has no max effort");
    }

    #[test]
    fn the_document_is_read_from_the_reply_after_the_request() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let user = |t: &str| json!({"type": "user", "message": {"role": "user", "content": t}});
        let claude = |t: &str| json!({"type": "assistant", "message": {"content": [{"type": "text", "text": t}]}});
        let codex = |t: &str| json!({"type": "response_item", "payload": {"type": "message", "role": "assistant",
                                                                          "content": [{"type": "output_text", "text": t}]}});
        let rows = [
            user("[cm-handoff-request old1] ..."),
            claude("<cm-handoff>\nstale\n</cm-handoff>"),
            user("[cm-handoff-request ab12] write it"),
            claude("Here it is.\n<cm-handoff>\n# Goal"),
            codex("rotate things\n</cm-handoff>"),
        ];
        std::fs::write(&path, rows.iter().map(|r| format!("{r}\n")).collect::<String>()).unwrap();
        assert_eq!(extract_document(&path, "ab12").as_deref(), Some("# Goal\nrotate things"));
        assert_eq!(extract_document(&path, "old1").as_deref(), Some("stale"));
        assert!(extract_document(&path, "nope").is_none());
        assert!(replied_after(&path, "[cm-handoff-request ab12]"));
        std::fs::write(&path, format!("{}\n", user("[cm-handoff zz] go"))).unwrap();
        assert!(!replied_after(&path, "[cm-handoff zz]"), "no reply yet");
    }

    #[test]
    fn prompts_name_what_the_successor_needs() {
        let w = write_request("m1", "Codex (gpt-6-luna)", "Owner asked");
        for needed in ["<cm-handoff>", "</cm-handoff>", "board and number", "ETAs", "next step", "Owner asked"] {
            assert!(w.contains(needed), "{needed}: {w}");
        }
        let t = takeover_notice("m2", "lane-a", "Claude", "", Path::new("/h/.cm/handoffs/u/1.md"));
        assert!(t.contains("/h/.cm/handoffs/u/1.md") && t.contains("board items") && t.contains("inbox"));
        assert!(t.contains("re-arm") && t.contains("list_monitors"), "monitors died with the old engine");
        assert!(fallback_notice("x", Some(Path::new("/f.md"))).contains("/f.md"));
    }

    #[test]
    fn a_switch_drops_the_old_engines_pick() {
        let mut e: crate::manifest::ManifestEntry = serde_json::from_value(json!({
            "uid": "u", "label": "l", "session_type": "claude-code", "model": "claude-opus-5-5",
            "model_source": "explicit", "effort": "max", "effort_source": "explicit"})).unwrap();
        let p = plan("claude-code", Some("claude-opus-5-5"), Some("max"), &req(Some("codex"), None, None)).unwrap();
        set_pick(&mut e, &p);
        assert_eq!((e.model, e.effort), (None, None), "codex resolves its own defaults");
        let p = plan("claude-code", Some("claude-opus-5-5"), Some("max"), &req(None, None, Some("high"))).unwrap();
        let mut e2: crate::manifest::ManifestEntry =
            serde_json::from_value(json!({"uid": "u", "label": "l", "session_type": "claude-code", "model": "claude-opus-5-5"})).unwrap();
        set_pick(&mut e2, &p);
        assert_eq!((e2.model.as_deref(), e2.effort.as_deref()), (Some("claude-opus-5-5"), Some("high")));
    }

    /// The queue pass checks boundaries (which read this registry) without
    /// holding it: a regression here deadlocked the daemon on the first
    /// queued handoff.
    #[test]
    fn a_queue_pass_does_not_deadlock_on_its_own_registry() {
        let state = Arc::new(Mutex::new(DaemonState::new()));
        let mut sp = crate::session::SpawnParams::new("ts-tick-0", "t", "/bin/sleep");
        sp.args = vec!["30".into()];
        sp.session_type = "claude-code".into();
        state.lock().unwrap().sessions.insert("ts-tick-0".into(), crate::session::DaemonSession::spawn(sp).unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        let st = Arc::clone(&state);
        std::thread::spawn(move || {
            let r = request(&st, "ts-tick-0", req(Some("codex"), None, None), None);
            tick(&st);
            let _ = tx.send(r.is_ok());
        });
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)), Ok(true), "request + tick returned");
        let status = status("ts-tick-0").unwrap();
        assert!(status["deferred"].is_string(), "a fresh process is not idle: deferred, {status}");
        registry().lock().unwrap().queue.retain(|q| q.uid != "ts-tick-0");
        for s in state.lock().unwrap().sessions.values_mut() {
            let _ = s.kill();
        }
    }

    /// #25 review: a rotation and a handoff admitted from two threads (each
    /// under the state lock, as `rotation::start` and `tick` do) are never
    /// both running.
    #[test]
    fn document_found_when_a_last_prompt_row_repeats_the_request() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let req = json!({"type": "user", "message": {"role": "user", "content": "[cm-handoff-request abcd1234] write it"}});
        let reply = json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "<cm-handoff>\n# Goal\nship it\n</cm-handoff>"}]}});
        let last = json!({"type": "last-prompt", "lastPrompt": "[cm-handoff-request abcd1234] write it"});
        std::fs::write(&path, format!("{req}\n{reply}\n{last}\n")).unwrap();
        assert_eq!(extract_document(&path, "abcd1234").as_deref(), Some("# Goal\nship it"));
    }

    #[test]
    fn a_rotation_and_a_handoff_never_both_get_the_same_session() {
        let _serial = crate::rotation::test_registry_lock();
        let state = Arc::new(Mutex::new(DaemonState::new()));
        let both = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let wins = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let rot = {
            let (state, both, wins) = (Arc::clone(&state), Arc::clone(&both), Arc::clone(&wins));
            std::thread::spawn(move || {
                for _ in 0..500 {
                    let admitted = {
                        let st = state.lock().unwrap();
                        let ok = crate::rotation::admit(&st, "ts-race-r", crate::rotation::test_rotation_record("ts-race-r")).is_ok();
                        if ok && in_progress("ts-race-r") {
                            both.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }
                        ok
                    };
                    if admitted {
                        wins.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        std::thread::yield_now();
                        crate::rotation::test_finish("ts-race-r");
                    }
                }
            })
        };
        let hand = {
            let (state, both, wins) = (Arc::clone(&state), Arc::clone(&both), Arc::clone(&wins));
            std::thread::spawn(move || {
                for _ in 0..500 {
                    let admitted = {
                        let st = state.lock().unwrap();
                        let mut reg = registry().lock().unwrap();
                        let ok = take_slot(&st, &mut reg, "ts-race-r");
                        if ok && crate::rotation::in_progress("ts-race-r") {
                            both.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }
                        ok
                    };
                    if admitted {
                        wins.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        std::thread::yield_now();
                        registry().lock().unwrap().running.remove("ts-race-r");
                    }
                }
            })
        };
        rot.join().unwrap();
        hand.join().unwrap();
        assert_eq!(both.load(std::sync::atomic::Ordering::SeqCst), 0, "never both");
        assert!(wins.load(std::sync::atomic::Ordering::SeqCst) > 0, "the race actually admitted work");
    }

    /// #25 review: after a restart, a handoff already in the lineage (it
    /// finished before the marker was cleared) gets no duplicate takeover,
    /// and a session still on its old engine is left alone.
    #[test]
    fn recovery_skips_a_finished_handoff_and_one_not_yet_switched() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = DaemonState::new();
        st.daemon_sessions_path = Some(dir.path().join("daemon-sessions.json"));
        let mut sp = crate::session::SpawnParams::new("ts-rl-0", "r", "/bin/sleep");
        sp.args = vec!["30".into()];
        sp.session_type = "codex".into();
        st.sessions.insert("ts-rl-0".into(), crate::session::DaemonSession::spawn(sp).unwrap());
        let snap = |engine: &str| crate::rotation::Snapshot {
            entry: serde_json::from_value(json!({"uid": "ts-rl-0", "label": "r", "session_type": engine})).unwrap(),
            workspace_id: "ws".into(),
            worktree: dir.path().into(),
            from_path: dir.path().join("old.jsonl"),
            from_id: "old-id".into(),
            from_bytes: 0,
        };
        crate::resume_identity::record_handoff(&st, crate::resume_identity::Handoff {
            uid: "ts-rl-0".into(), from_engine: "claude-code".into(), from_id: "old-id".into(),
            to_engine: "codex".into(), to_id: "new".into(), file: "/f.md".into(), at: 1.0,
        }).unwrap();
        let state = Arc::new(Mutex::new(st));
        assert!(!recover_live(&state, "ts-rl-0", snap("claude-code"), Some("/f.md")), "finished: no duplicate");
        assert!(!recover_live(&state, "ts-rl-0", snap("codex"), Some("/f.md")), "not switched yet");
        assert!(running().is_none());
        for s in state.lock().unwrap().sessions.values_mut() {
            let _ = s.kill();
        }
    }

    #[test]
    fn engine_names_are_normalized() {
        assert_eq!(engine_name("Claude"), Some("claude-code"));
        assert_eq!(engine_name("codex"), Some("codex"));
        assert_eq!(engine_name("bash"), None);
    }
}
