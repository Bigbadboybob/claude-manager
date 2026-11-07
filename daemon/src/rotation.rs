//! Transcript rotation (#24): move an oversized Claude conversation to a
//! fresh transcript, at the same CM session.
//!
//! Claude Code (2.1.310) reads the whole transcript file on every resume and
//! keeps a long session's history in memory; compaction never shrinks the
//! file. Its own "Resume from summary" only runs `/compact` in place. What
//! does move a conversation is `claude --resume <id> --fork-session
//! --session-id <new>`: for a file over 5 MB the fork copies only the chain
//! after the last compact boundary (the latest summary plus what followed)
//! into a new file, written when the first message is recorded.
//!
//! A rotation therefore:
//! 1. checks the safe boundary ([`boundary`]) unless forced;
//! 2. stops the process (killed_by [`ROTATION_KILLER`], which monitors and
//!    viewers read as "coming back");
//! 3. respawns it at the SAME uid through the restore path with a fork
//!    launch, so chat identity, org seat, global perms, task and workspace
//!    carry over and the row rebinds to the pinned new transcript;
//! 4. delivers the after-notice as the first message (which writes the new
//!    file);
//! 5. verifies the new file exists and carries the conversation, else
//!    resumes the old transcript and reports the failure.
//!
//! The old file is never written (CM does not mutate Claude-owned
//! transcripts); the lineage is in the resume-identity archive, which keeps
//! every transcript id a uid has had.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::control::protocol::ErrorCode;
use crate::state::DaemonState;

/// `killed_by` of the stop a rotation does: the session comes straight
/// back at the same uid.
pub const ROTATION_KILLER: &str = "transcript-rotation";

type MethodResult = Result<Value, (ErrorCode, String)>;

/// Who asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    Operator,
    Agent,
    Policy,
}

impl Origin {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Origin::Operator => "operator",
            Origin::Agent => "agent",
            Origin::Policy => "policy",
        }
    }
}

/// One rotation's record, kept for `status` / `list_sessions`.
#[derive(Clone, Debug)]
pub(crate) struct Rotation {
    phase: &'static str,
    origin: Origin,
    reason: String,
    started_at: f64,
    from_id: String,
    from_bytes: u64,
    to_id: Option<String>,
    to_bytes: Option<u64>,
    path: &'static str,
    outcome: Option<String>,
    finished_at: Option<f64>,
    /// The session that asked for this rotation of another session (#25
    /// follow-up), as "label (uid)".
    requested_by: Option<String>,
}

impl Rotation {
    fn to_json(&self) -> Value {
        json!({
            "phase": self.phase,
            "origin": self.origin.as_str(),
            "reason": self.reason,
            "started_at": self.started_at,
            "from_transcript_id": self.from_id,
            "from_bytes": self.from_bytes,
            "to_transcript_id": self.to_id,
            "to_bytes": self.to_bytes,
            "path": self.path,
            "outcome": self.outcome,
            "finished_at": self.finished_at,
            "requested_by": self.requested_by,
        })
    }
}

fn registry() -> &'static Mutex<HashMap<String, Rotation>> {
    static ROTATIONS: std::sync::OnceLock<Mutex<HashMap<String, Rotation>>> = std::sync::OnceLock::new();
    ROTATIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn update(uid: &str, f: impl FnOnce(&mut Rotation)) {
    if let Some(r) = registry().lock().unwrap_or_else(|p| p.into_inner()).get_mut(uid) {
        f(r);
    }
}

/// The latest rotation for `uid` (running or finished), for `list_sessions`
/// and the rotate RPC's status.
pub fn status(uid: &str) -> Option<Value> {
    registry().lock().unwrap_or_else(|p| p.into_inner()).get(uid).map(Rotation::to_json)
}

/// Tests that touch the host-wide rotation registry take this, so a running
/// record one test plants never trips another's one-at-a-time claim.
#[cfg(test)]
pub(crate) fn test_registry_lock() -> std::sync::MutexGuard<'static, ()> {
    static L: Mutex<()> = Mutex::new(());
    L.lock().unwrap_or_else(|p| p.into_inner())
}

/// Tests: drop `uid`'s arm and due hold.
#[cfg(test)]
pub(crate) fn test_drop_arm(uid: &str) {
    arm_remove(uid);
    unhold(uid, HoldOwner::Due);
}

/// Tests: a rotation record for [`admit`].
#[cfg(test)]
pub(crate) fn test_rotation_record(uid: &str) -> Rotation {
    Rotation {
        phase: "stopping", origin: Origin::Operator, reason: String::new(), started_at: now(),
        from_id: uid.into(), from_bytes: 0, to_id: None, to_bytes: None, path: "fork",
        outcome: None, finished_at: None, requested_by: None,
    }
}

/// Tests: finish `uid`'s rotation record.
#[cfg(test)]
pub(crate) fn test_finish(uid: &str) {
    finish(uid, "done", "test".into());
}

/// Tests: mark `uid` as mid-rotation (`done` finishes it).
#[cfg(test)]
pub(crate) fn test_set_running(uid: &str, running: bool) {
    let mut reg = registry().lock().unwrap_or_else(|p| p.into_inner());
    if running {
        reg.insert(uid.into(), Rotation {
            phase: "forking", origin: Origin::Operator, reason: String::new(), started_at: now(),
            from_id: String::new(), from_bytes: 0, to_id: None, to_bytes: None, path: "fork",
            outcome: None, finished_at: None, requested_by: None,
        });
    } else {
        reg.remove(uid);
    }
}

/// Tests: mark `uid` as being rotated off `from_id`, in `phase`.
#[cfg(test)]
pub(crate) fn test_set_rotating(uid: &str, from_id: &str, phase: &'static str) {
    registry().lock().unwrap_or_else(|p| p.into_inner()).insert(uid.into(), Rotation {
        phase, origin: Origin::Operator, reason: String::new(), started_at: now(),
        from_id: from_id.into(), from_bytes: 0, to_id: None, to_bytes: None, path: "fork",
        outcome: None, finished_at: None, requested_by: None,
    });
}

/// Whether `uid` is being rotated right now (its process is expected to
/// stop and come back).
pub fn in_progress(uid: &str) -> bool {
    registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(uid)
        .is_some_and(|r| r.finished_at.is_none())
}

fn now() -> f64 {
    crate::control::methods::now_unix_f64()
}

/// When the running phase of each rotation ends at the latest (unix secs).
fn etas() -> &'static Mutex<HashMap<String, f64>> {
    static ETAS: std::sync::OnceLock<Mutex<HashMap<String, f64>>> = std::sync::OnceLock::new();
    ETAS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Enter `phase`, which ends within `within` at the latest (`None`: unknown).
fn set_phase(uid: &str, phase: &'static str, within: Option<Duration>) {
    update(uid, |r| r.phase = phase);
    let mut etas = etas().lock().unwrap_or_else(|p| p.into_inner());
    match within {
        Some(d) => etas.insert(uid.to_string(), now() + d.as_secs_f64()),
        None => etas.remove(uid),
    };
}

/// Whether a transcript report (a turn-end hook, the TUI's or the `/proc`
/// detector's `set_transcript_path`) that names `path` for `uid` must be
/// ignored: `uid` is being moved off that very transcript, and its fork is
/// starting or being verified. Only the rotation itself rebinds the row
/// until it finishes (its fallback resumes the original through the
/// restore path, not through a report).
pub fn refuses_restamp(uid: &str, path: &str) -> bool {
    let Some(id) = crate::session::transcript_id_from_path(path) else { return false };
    registry().lock().unwrap_or_else(|p| p.into_inner()).get(uid).is_some_and(|r| {
        r.finished_at.is_none() && matches!(r.phase, "forking" | "notifying" | "verifying") && r.from_id == id
    })
}

/// The first words of every "a rotation is running" refusal.
pub const BUSY: &str = "transcript rotation in progress";

fn phase_label(phase: &str) -> &'static str {
    match phase {
        "stopping" => "stopping the old process",
        "forking" => "starting the fork",
        "notifying" => "delivering the fork's first message",
        "verifying" => "confirming the fork's new transcript",
        "falling back" => "resuming the original transcript",
        _ => "finishing",
    }
}

fn short_secs(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    if s >= 60 { format!("{}m{:02}s", s / 60, s % 60) } else { format!("{s}s") }
}

/// What to tell someone who asks for a restart (Alt+T, A-R, a revive) while
/// `uid` is being rotated: the phase and how long it can still take. It
/// ends "comes straight back" (what viewers key their wait on).
pub fn busy_message(uid: &str) -> Option<String> {
    let reg = registry().lock().unwrap_or_else(|p| p.into_inner());
    let r = reg.get(uid).filter(|r| r.finished_at.is_none())?;
    let t = now();
    let left = etas()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(uid)
        .copied()
        .filter(|&e| e > t)
        .map(|e| format!(", at most {} left", short_secs(e - t)))
        .unwrap_or_default();
    Some(format!(
        "{BUSY}: {} (started {} ago{left}); CM restarts the session itself and it comes straight back",
        phase_label(r.phase),
        short_secs(t - r.started_at)
    ))
}

/// Why `uid` cannot rotate right now, if it cannot. The safe boundary:
/// a live Claude session with a transcript, idle for `idle` with nothing in
/// flight (no open turn, no background jobs), an empty native notification
/// queue (the rotation's own notice aside), Owner not viewing or typing in
/// it for `owner_quiet`, not a
/// workflow participant, not a continuous tick mid-run, and not already
/// rotating.
pub fn boundary(state: &DaemonState, uid: &str, idle: Duration, owner_quiet: Duration) -> Result<(), String> {
    boundary_for(state, uid, idle, owner_quiet, &["claude-code"])
}

/// [`boundary`] for sessions of any of `engines` (#25: an explicit Codex
/// rotation or a handoff; the automatic policy stays Claude-only).
pub fn boundary_for(
    state: &DaemonState,
    uid: &str,
    idle: Duration,
    owner_quiet: Duration,
    engines: &[&str],
) -> Result<(), String> {
    boundary_checked(state, uid, engines, Checks { idle, owner_quiet, draft: !owner_quiet.is_zero(), at_turn_end: false, crons_block: false })
}

/// What a boundary check asks for.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Checks {
    /// How long the agent must have been idle.
    pub idle: Duration,
    /// How long Owner must not have viewed or typed in it (and whether a
    /// possible unsent draft blocks it): the automatic policy only.
    pub owner_quiet: Duration,
    /// Whether Owner keystrokes since the agent's last turn (a possible
    /// unsent draft) block it: everyone but an operator forcing it.
    pub draft: bool,
    /// Checked right at a turn end (or forced): queued wakes do not block
    /// (they are held for the new process, `notifications::set_hold`), only
    /// one being submitted to the old process right now does.
    pub at_turn_end: bool,
    /// Whether a pending session cron blocks it. A rotation carries crons
    /// over (`session_crons`: saved, named in the after-notice, re-created
    /// by the agent), so for it they never block (#114: 787 of ~2,200
    /// deferrals of a busy lead); a handoff does not carry them yet.
    pub crons_block: bool,
}

/// An unconfirmed receipt older than this does not block a boundary...
const STALE_RECEIPT_SECS: f64 = 10.0 * 60.0;
/// ...and one older than this is retired (`notifications::retire_stale`).
const RETIRE_RECEIPT_SECS: f64 = 24.0 * 3600.0;

/// Whether a background job's process is still there (a job whose process
/// is gone has exited, even if the agent's last snapshot still lists it).
fn job_alive(job: &crate::agent_state::Job) -> bool {
    job.pid.is_none_or(|pid| std::path::Path::new(&format!("/proc/{pid}")).exists())
}

pub(crate) fn boundary_checked(state: &DaemonState, uid: &str, engines: &[&str], checks: Checks) -> Result<(), String> {
    let Checks { idle, owner_quiet, draft, at_turn_end, crons_block } = checks;
    let sess = state.sessions.get(uid).ok_or_else(|| format!("session '{uid}' is not live on this host"))?;
    if !engines.contains(&sess.session_type.as_str()) {
        return Err(format!("{} sessions cannot do this (only {})", sess.session_type, engines.join(", ")));
    }
    if sess.transcript_path.is_none() {
        return Err("the session has no bound transcript".into());
    }
    if let Some(busy) = busy_message(uid) {
        return Err(busy);
    }
    if crate::handoff::in_progress(uid) {
        return Err("a handoff is running".into());
    }
    if sess.workflow_run_id.is_some() {
        return Err("workflow participant (its run drives it)".into());
    }
    if let Some(ct) = &sess.continuous_task_id {
        let mid_run = crate::continuous::task::load_one(ct).is_some_and(|t| t.in_flight.is_some());
        if mid_run {
            return Err("continuous tick mid-run".into());
        }
    }
    let agent = crate::agent_state::current(sess);
    if agent.state != crate::agent_state::State::Idle {
        return Err(format!("not idle ({})", agent.state.as_str()));
    }
    let idle_for = now() - agent.since;
    if idle_for < idle.as_secs_f64() {
        return Err(format!("idle {:.0}s of the {}s required", idle_for.max(0.0), idle.as_secs()));
    }
    let jobs = agent.background.jobs.iter().filter(|j| job_alive(j)).count();
    if jobs > 0 {
        return Err(format!("{jobs} background job(s) running"));
    }
    // A scheduled wakeup lives in the process: a restart loses it unless
    // the caller carries it over (a rotation does).
    if crons_block && !agent.background.crons.is_empty() {
        return Err(format!("{} scheduled wakeup(s) pending", agent.background.crons.len()));
    }
    if let Some(at) = state.owner_seen.get(uid) {
        if at.elapsed() < owner_quiet {
            return Err(format!("Owner viewed or typed in it {}s ago", at.elapsed().as_secs()));
        }
    }
    let typed = *sess.last_operator_input_at.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(at) = typed {
        if at.elapsed() < owner_quiet {
            return Err(format!("Owner typed in it {}s ago", at.elapsed().as_secs()));
        }
        // Keystrokes after the agent's last turn ended, with no turn since:
        // an unsent draft may sit in its composer. Only an operator forcing
        // it has decided otherwise (an agent's own request has not).
        let typed_unix = now() - at.elapsed().as_secs_f64();
        if draft && typed_unix > agent.since {
            return Err("Owner typed in it since its last turn (a draft may be unsent)".into());
        }
    }
    let t = now();
    let pending = crate::notifications::snapshot(&state.messaging_root, uid)
        .map(|(events, _)| {
            events
                .iter()
                .filter(|e| {
                    let age = t - e["updated_at"].as_f64().or_else(|| e["created_at"].as_f64()).unwrap_or(0.0);
                    match e["status"].as_str() {
                        // Queued wakes wait for the new process (held) at a
                        // turn end; on a poll they are still to come.
                        Some("pending") => !at_turn_end,
                        // Handed to the old process and not yet seen in its
                        // transcript: a kill now could lose it. Past the age
                        // limit it never will be seen, and does not block.
                        Some("submitting" | "submitted" | "uncertain") => age < STALE_RECEIPT_SECS,
                        _ => false,
                    }
                })
                // The rotation's own before-notice never holds it back (a
                // stuck consumer would otherwise block rotation forever).
                .filter(|e| !matches!(e["source"].as_str(), Some("rotation" | "handoff")))
                .count()
        })
        .unwrap_or(0);
    if pending > 0 {
        return Err(if at_turn_end {
            format!("{pending} notification(s) being delivered (not yet confirmed)")
        } else {
            format!("{pending} notification(s) not yet delivered")
        });
    }
    Ok(())
}

/// Whether a boundary refusal can never clear by waiting (anything else
/// clears at a later turn end, so a forced request arms instead).
fn permanent_refusal(why: &str) -> bool {
    why.contains("is not live on this host")
        || why.contains("sessions cannot do this")
        || why.contains("has no bound transcript")
        || why.contains("workflow participant")
}

/// Whether a refusal is about the next few seconds (the agent is mid-turn,
/// a wake being delivered): worth holding new wakes for.
fn hold_worthy(why: &str) -> bool {
    // "not idle (working)" only: working-background, a waiting human or an
    // error can last for hours, and so can background jobs.
    why.contains("not idle (working)") || why.contains("being delivered")
}

/// What a rotation needs from the live session, captured under the lock.
pub(crate) struct Snapshot {
    pub(crate) entry: crate::manifest::ManifestEntry,
    pub(crate) workspace_id: String,
    pub(crate) worktree: PathBuf,
    pub(crate) from_path: PathBuf,
    pub(crate) from_id: String,
    pub(crate) from_bytes: u64,
}

pub(crate) fn snapshot(state: &DaemonState, uid: &str) -> Result<Snapshot, String> {
    let sess = state.sessions.get(uid).ok_or_else(|| format!("session '{uid}' is not live"))?;
    let from_path = PathBuf::from(sess.transcript_path.clone().ok_or("no bound transcript")?);
    let from_id = crate::session::transcript_id_from_path(&from_path.to_string_lossy())
        .ok_or("the transcript path names no id")?;
    let worktree = state
        .workspaces
        .get(&sess.workspace_id)
        .and_then(|w| w.worktree_path.clone())
        .ok_or("the session's workspace has no worktree path")?;
    let mut entry = crate::resume_identity::entry(state, sess);
    entry.transcript_id = Some(from_id.clone());
    entry.transcript_path = Some(from_path.to_string_lossy().into_owned());
    entry.last_exit = None;
    entry.reported_done_at = None;
    entry.report_reason = None;
    Ok(Snapshot {
        from_bytes: std::fs::metadata(&from_path).map(|m| m.len()).unwrap_or(0),
        entry,
        workspace_id: sess.workspace_id.clone(),
        worktree,
        from_path,
        from_id,
    })
}

/// Start a rotation of `uid`. Returns at once with the rotation record; the
/// stop/fork/verify runs on a worker thread (`status` reports its phase).
/// `force` skips the boundary check (operator use, e.g. a session that is
/// unusably slow and never settles).
pub fn start(
    state_arc: &Arc<Mutex<DaemonState>>,
    uid: &str,
    reason: &str,
    origin: Origin,
    force: bool,
) -> MethodResult {
    // A running rotation or handoff is not a reason to arm a second one.
    if let Some(busy) = busy_message(uid) {
        return Err((ErrorCode::Conflict, busy));
    }
    if crate::handoff::in_progress(uid) {
        return Err((ErrorCode::Conflict, "a handoff of this session is running".into()));
    }
    let checks = {
        let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        let c = &state.config.transcript_rotation;
        if force {
            // `force` (an operator's explicit request) waives the idle and
            // Owner-quiet waits and does not wait for queued wakes (held for
            // the new process); a tool call, background job, a wake going in
            // right now, a workflow transition or a continuous run still
            // refuse, and then the request is ARMED for the next turn end.
            Checks { idle: Duration::ZERO, owner_quiet: Duration::ZERO, draft: false, at_turn_end: true, crons_block: false }
        } else {
            Checks {
                idle: Duration::from_secs(c.idle_secs),
                owner_quiet: if origin == Origin::Policy { Duration::from_secs(c.owner_quiet_secs) } else { Duration::ZERO },
                draft: origin != Origin::Operator,
                at_turn_end: false,
                crons_block: false,
            }
        }
    };
    match start_with(state_arc, uid, reason, origin, checks) {
        Err((ErrorCode::Conflict, why)) if force && !permanent_refusal(&why) => {
            let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
            let mut v = arm(&state, uid, reason, origin)?;
            v["deferred"] = json!(why);
            Ok(v)
        }
        other => other,
    }
}

/// [`start`] with explicit boundary checks.
pub(crate) fn start_with(
    state_arc: &Arc<Mutex<DaemonState>>,
    uid: &str,
    reason: &str,
    origin: Origin,
    checks: Checks,
) -> MethodResult {
    let (snap, record) = {
        let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        // #25: an explicit request may rotate Codex too (`codex fork`); the
        // automatic policy stays Claude-only.
        let engines: &[&str] = if origin == Origin::Policy { &["claude-code"] } else { &["claude-code", "codex"] };
        boundary_checked(&state, uid, engines, checks).map_err(|why| (ErrorCode::Conflict, format!("not at a safe boundary: {why}")))?;
        if origin != Origin::Operator {
            if let Some(left) = pause_left() {
                return Err((ErrorCode::Conflict, format!("pausing {}s after the last rotation", left.as_secs())));
            }
        }
        let snap = snapshot(&state, uid).map_err(|e| (ErrorCode::Conflict, e))?;
        let requested_by = arms().lock().unwrap_or_else(|p| p.into_inner()).get(uid).and_then(|a| a.requested_by.clone());
        let rotation = Rotation {
            phase: "stopping",
            origin,
            reason: reason.to_string(),
            started_at: now(),
            from_id: snap.from_id.clone(),
            from_bytes: snap.from_bytes,
            to_id: None,
            to_bytes: None,
            path: "fork",
            outcome: None,
            finished_at: None,
            requested_by,
        };
        let record = rotation.to_json();
        admit(&state, uid, rotation).map_err(|why| (ErrorCode::Conflict, why))?;
        // New wakes wait for the new process (lifted when this finishes).
        hold(&state, uid, HoldOwner::Rotation);
        arm_remove(uid);
        (snap, record)
    };
    // A brain restart between the stop and the respawn must not leave the
    // session dead: the marker lets the next brain resume the old transcript.
    if let Err(e) = inflight_put(&state_arc.lock().unwrap_or_else(|p| p.into_inner()), uid, &snap) {
        eprintln!("cm-daemon: rotation of {uid}: in-flight marker not written: {e}");
    }
    let state = Arc::clone(state_arc);
    let uid_owned = uid.to_string();
    std::thread::Builder::new()
        .name(format!("cm-rotate-{uid}"))
        .spawn(move || {
            run(&state, &uid_owned, snap);
            inflight_remove(&state.lock().unwrap_or_else(|p| p.into_inner()), &uid_owned);
        })
        .map_err(|e| (ErrorCode::Internal, format!("rotation worker: {e}")))?;
    Ok(json!({ "ok": true, "session_uid": uid, "rotation": record }))
}

const STOP_DEADLINE: Duration = Duration::from_secs(20);
/// How long the fork has to record its first message: 180 s plus 60 s per
/// GB of source (Claude reads the whole source to fork it; a 1.7 GB
/// transcript forked in 23 s on cm-sessions with a warm cache), at most 20 min.
fn verify_deadline(source_bytes: u64) -> Duration {
    Duration::from_secs_f64((180.0 + 60.0 * source_bytes as f64 / 1e9).min(1200.0))
}

/// The pause between two rotations on a host (not for an operator's own).
const PAUSE_BETWEEN: Duration = Duration::from_secs(120);

pub(crate) fn wait_gone(state_arc: &Arc<Mutex<DaemonState>>, uid: &str) -> bool {
    wait_gone_for(state_arc, uid, STOP_DEADLINE)
}

fn wait_gone_for(state_arc: &Arc<Mutex<DaemonState>>, uid: &str, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if !state_arc.lock().unwrap_or_else(|p| p.into_inner()).sessions.contains_key(uid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

/// How a stop went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stopped {
    /// Gone within the stop deadline.
    Yes,
    /// Gone, but only after the deadline: Owner was alerted meanwhile.
    Late,
    /// Still running 10 minutes after the kill: Owner alerted.
    Stuck,
}

/// Stop `uid` (attributed to `killer`) and wait for it. A process that is
/// still there after the stop deadline is not "left as it was": the kill
/// was sent, so Owner is alerted and CM keeps watching it for 10 minutes.
pub(crate) fn stop_tracked(state_arc: &Arc<Mutex<DaemonState>>, uid: &str, killer: &str, what: &str) -> Stopped {
    crate::control::methods::kill_with_attribution(state_arc, uid, killer);
    if wait_gone(state_arc, uid) {
        return Stopped::Yes;
    }
    let alert = |message: String| {
        let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        if let Err(e) = crate::owner_attention::raise_system(&st, uid, &message) {
            eprintln!("cm-daemon: {what} alert for {uid}: {e}");
        }
    };
    alert(format!(
        "CM stopped this session for a {what}, but it is still running {}s later. CM keeps \
         watching it and resumes its original conversation once it exits.",
        STOP_DEADLINE.as_secs()
    ));
    if wait_gone_for(state_arc, uid, Duration::from_secs(600)) {
        return Stopped::Late;
    }
    alert(format!(
        "CM stopped this session for a {what}, but it is still running 10 minutes later. \
         Check the process; kill_session or A-R once it is gone."
    ));
    Stopped::Stuck
}

/// How far into a fork's file CM looks for its first message row. A fork
/// opens with metadata and `file-history-snapshot` rows (34 KB each on EP's
/// 2026-11-07 fork, whose first message sat at byte 275,257: the old 256 KB
/// window never saw it, and a working fork was rolled back).
const MESSAGE_SCAN_BYTES: u64 = 256 * 1024 * 1024;

/// Whether one transcript line is a message row: a Claude chain row (a
/// top-level `user`/`assistant` with a `parentUuid`) or a Codex
/// `response_item`.
fn is_message_row(line: &[u8]) -> bool {
    let has = |needle: &[u8]| line.windows(needle.len()).any(|w| w == needle);
    if !(has(b"\"user\"") || has(b"\"assistant\"") || has(b"\"response_item\"")) {
        return false;
    }
    let Ok(row) = serde_json::from_slice::<Value>(line) else { return false };
    match row["type"].as_str() {
        Some("user" | "assistant") => row.get("parentUuid").is_some(),
        Some("response_item") => true,
        _ => false,
    }
}

/// The offset just past the first message row at or after `since` in
/// `path` (scanning at most [`MESSAGE_SCAN_BYTES`]), if there is one.
fn first_message_after(path: &Path, since: u64) -> Option<u64> {
    use std::io::{BufRead, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(since)).ok()?;
    let mut reader = std::io::BufReader::new(f);
    let (mut at, mut line) = (since, Vec::new());
    while at - since < MESSAGE_SCAN_BYTES {
        line.clear();
        let n = reader.read_until(b'\n', &mut line).ok()?;
        // A row still being written (no newline yet) is read next time.
        if n == 0 || line.last() != Some(&b'\n') {
            return None;
        }
        at += n as u64;
        if is_message_row(&line) {
            return Some(at);
        }
    }
    None
}

/// A fork's transcript is in place: the file exists and holds a message
/// row anywhere (Claude's chain rows, or Codex's `response_item`s).
/// Returns its length.
fn fork_written(path: &Path) -> Option<u64> {
    let len = std::fs::metadata(path).ok()?.len();
    first_message_after(path, 0).map(|_| len)
}

/// Whether the fork at `path` is in use: the after-notice's marker is
/// recorded, or any new turn is. A Claude fork's file is only written when
/// its first message is recorded, whatever that message was (a wake that got
/// in first still proves the fork took). A Codex fork's rollout starts with
/// the copied history, so it needs a user row after `since` (its length when
/// first bound).
fn fork_took(path: &Path, marker: &str, claude: bool, since: u64) -> bool {
    if tail_contains(path, marker) || claude {
        return true;
    }
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else { return false };
    if f.seek(SeekFrom::Start(since)).is_err() {
        return false;
    }
    let mut buf = Vec::new();
    let _ = f.take(4 * 1024 * 1024).read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    text.lines().any(|l| l.contains("\"role\":\"user\"") || l.contains("\"type\":\"user\""))
}

/// Whether the last `TAIL` bytes of `path` contain `marker` (the delivered
/// notice was recorded in this transcript).
pub(crate) fn tail_contains(path: &Path, marker: &str) -> bool {
    const TAIL: u64 = 4 * 1024 * 1024;
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else { return false };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if f.seek(SeekFrom::Start(len.saturating_sub(TAIL))).is_err() {
        return false;
    }
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).is_ok() && buf.windows(marker.len()).any(|w| w == marker.as_bytes())
}

/// A short random id for a notice marker.
pub(crate) fn nonce() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{:08x}", (n as u64 ^ (std::process::id() as u64).rotate_left(32)) & 0xffff_ffff)
}

fn mb(bytes: u64) -> String {
    if bytes >= 10_000_000 {
        format!("{:.0} MB", bytes as f64 / 1e6)
    } else if bytes >= 1_000_000 {
        format!("{:.1} MB", bytes as f64 / 1e6)
    } else {
        format!("{} KB", bytes / 1000)
    }
}

/// The first message of the rotated session (it writes the new file).
pub fn after_notice(from_bytes: u64) -> String {
    after_notice_marked(from_bytes, "")
}

fn after_notice_marked(from_bytes: u64, marker: &str) -> String {
    after_notice_for(from_bytes, marker, None)
}

fn after_notice_for(from_bytes: u64, marker: &str, requested_by: Option<&str>) -> String {
    let asked = requested_by.map(|by| format!(" {by} asked for it.")).unwrap_or_default();
    format!(
        "[cm-rotation{marker}] CM moved this conversation to a fresh transcript: the old one was {},{asked} \
         and the agent slows down badly past a few hundred MB. You continue from your latest \
         compaction summary. Nothing else changed: same CM session, chat name, task and workspace. \
         Before you continue, re-read anything handoff-critical from CM rather than assuming it: \
         your board items, standing rules and Owner rulings, open monitors, and your chat inbox. \
         Then carry on with what you were doing.",
        mb(from_bytes)
    )
}

/// A phrase only the fallback notice carries (its delivery confirmation).
const FALLBACK_NEEDLE: &str = "CM tried to move this conversation to a fresh transcript";

pub(crate) fn fallback_notice(why: &str) -> String {
    format!(
        "[cm-rotation] CM tried to move this conversation to a fresh transcript, but it did not \
         take ({why}). You are back on your original transcript; nothing was lost. Carry on."
    )
}

/// A live view of a session's rendered screen, fed from its PTY output.
struct ScreenWatch {
    text: Arc<Mutex<String>>,
    last_output: Arc<Mutex<Instant>>,
    enter: Arc<Mutex<&'static [u8]>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl ScreenWatch {
    fn start(state_arc: &Arc<Mutex<DaemonState>>, uid: &str) -> Option<ScreenWatch> {
        let (fanout, cols, rows) = {
            let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
            let s = st.sessions.get(uid)?;
            (Arc::clone(&s.fanout), s.last_cols.max(80), s.last_rows.max(24))
        };
        let rx = fanout.subscribe();
        let watch = ScreenWatch {
            text: Arc::new(Mutex::new(String::new())),
            last_output: Arc::new(Mutex::new(Instant::now())),
            enter: Arc::new(Mutex::new(b"\r")),
            stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let (text, last, enter, stop) =
            (Arc::clone(&watch.text), Arc::clone(&watch.last_output), Arc::clone(&watch.enter), Arc::clone(&watch.stop));
        std::thread::Builder::new()
            .name(format!("cm-rotate-screen-{uid}"))
            .spawn(move || {
                let mut tracker = crate::workflow::pty_tracker::PtyModeTracker::with_size(cols as usize, rows as usize);
                while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                    match rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(bytes) => {
                            tracker.feed(&bytes, Instant::now());
                            *text.lock().unwrap_or_else(|p| p.into_inner()) = tracker.visible_text();
                            *enter.lock().unwrap_or_else(|p| p.into_inner()) = tracker.enter_bytes();
                            *last.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
            })
            .ok()?;
        Some(watch)
    }

    fn screen(&self) -> String {
        self.text.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The composer is drawn and the screen has been still for `quiet`.
    fn ready(&self, quiet: Duration) -> bool {
        crate::claude_channels::client_ready(&self.screen())
            && self.last_output.lock().unwrap_or_else(|p| p.into_inner()).elapsed() >= quiet
    }
}

impl Drop for ScreenWatch {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// How a [`deliver_ready`] went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Delivered {
    /// The transcript recorded it.
    Confirmed,
    /// Typed, but not seen in the transcript (the caller's own verification
    /// decides what that means).
    Unconfirmed,
    /// The session is gone.
    Vanished,
    /// Owner typed in it while CM waited: nothing was typed (it would have
    /// mixed with their draft); the notice went to the native queue.
    OwnerTyped,
}

/// Once the composer is drawn, how long CM waits for the screen to settle
/// before typing anyway: a busy session's footer (a background job's timer,
/// a spinner) never settles. Both of EP's 2026-11-07 waits ran the full
/// 239 s and the notice was then recorded within 5 s.
const COMPOSER_GRACE: Duration = Duration::from_secs(10);

/// Queue `text` for `uid` on the durable native notification queue (it
/// never types into the composer; rotation notices are exempt from the
/// wake hold).
fn publish_notice(state_arc: &Arc<Mutex<DaemonState>>, uid: &str, key: &str, text: &str, marker: &str) {
    let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
    if let Err(e) = crate::notifications::publish(&st.messaging_root, uid, key, "rotation", text, marker) {
        eprintln!("cm-daemon: rotation of {uid}: notice not queued: {e}");
    }
}

/// Deliver `text` to a Claude session that was just (re)started, once its
/// composer is really up, and make sure it was submitted. A large transcript
/// keeps loading after the process is up, and text typed meanwhile can sit
/// unsent (#24, 1 GB fork, 2026-10-26). So: wait for the composer footer and
/// a still screen (up to the size-scaled deadline); type it; then until the
/// transcript records `needle`, press Enter if it is visibly sitting in the
/// composer (up to 3 times), or type it once more if it vanished while the
/// agent sits idle. Human input stops the re-drive.
///
/// The wait for a still composer ends early (#114): when Owner types in it
/// (nothing is typed then: the notice goes to the native queue, keyed
/// `key`), when `turns` (a transcript and the offset to look past) records
/// a new message (the process is up and taking turns, so text typed now is
/// queued by Claude), or [`COMPOSER_GRACE`] after the composer is first
/// drawn.
fn deliver_ready(
    state_arc: &Arc<Mutex<DaemonState>>,
    uid: &str,
    text: String,
    needle: &str,
    source_bytes: u64,
    turns: Option<(&Path, u64)>,
    key: &str,
) -> Delivered {
    let Some(watch) = ScreenWatch::start(state_arc, uid) else { return Delivered::Vanished };
    let started = Instant::now();
    let budget = verify_deadline(source_bytes);
    let mut composer_at: Option<Instant> = None;
    while !watch.ready(Duration::from_millis(1500)) {
        let handle = {
            let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
            match st.sessions.get(uid) {
                Some(s) => s.input_handle(),
                None => return Delivered::Vanished,
            }
        };
        if handle.operator_quiet_for().is_some_and(|q| q < started.elapsed()) {
            eprintln!("cm-daemon: rotation of {uid}: Owner is typing in it; the notice goes to the native queue");
            publish_notice(state_arc, uid, key, &text, needle);
            return Delivered::OwnerTyped;
        }
        if let Some((path, since)) = turns {
            if first_message_after(path, since).is_some() {
                eprintln!(
                    "cm-daemon: rotation of {uid}: already taking turns after {:.1}s; typing now",
                    started.elapsed().as_secs_f64()
                );
                break;
            }
        }
        if crate::claude_channels::client_ready(&watch.screen()) {
            if composer_at.get_or_insert_with(Instant::now).elapsed() >= COMPOSER_GRACE {
                eprintln!(
                    "cm-daemon: rotation of {uid}: composer up for {}s but the screen keeps changing; typing",
                    COMPOSER_GRACE.as_secs()
                );
                break;
            }
        }
        if started.elapsed() >= budget {
            eprintln!("cm-daemon: rotation of {uid}: no composer after {}s; typing anyway", budget.as_secs());
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let waited = started.elapsed();
    if !deliver_live(state_arc, uid, text.clone()) {
        return Delivered::Vanished;
    }
    let typed = Instant::now();
    let (mut enters, mut retyped) = (0, false);
    while typed.elapsed() < Duration::from_secs(120) {
        std::thread::sleep(Duration::from_secs(1));
        let (live, path, handle, quiet) = {
            let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
            match st.sessions.get(uid) {
                Some(s) => (true, s.transcript_path.clone(), Some(s.input_handle()),
                            crate::agent_state::current(s).state != crate::agent_state::State::Working),
                None => (false, None, None, false),
            }
        };
        if !live {
            return Delivered::Vanished;
        }
        let recorded = path.as_deref().is_some_and(|p| tail_contains(Path::new(p), needle))
            || turns.is_some_and(|(p, _)| tail_contains(p, needle));
        if recorded {
            eprintln!(
                "cm-daemon: rotation of {uid}: notice confirmed (composer after {:.1}s, {enters} extra Enter(s){})",
                waited.as_secs_f64(),
                if retyped { ", retyped once" } else { "" }
            );
            return Delivered::Confirmed;
        }
        let Some(handle) = handle else { continue };
        if handle.operator_quiet_for().is_some_and(|q| q < typed.elapsed()) {
            eprintln!("cm-daemon: rotation of {uid}: Owner typed in it; leaving the notice to them");
            return Delivered::Unconfirmed;
        }
        let since = typed.elapsed().as_secs();
        let screen = watch.screen();
        let sitting = screen.contains(needle.trim_end_matches(']'));
        if sitting && enters < 3 && since >= 10 * (enters + 1) as u64 {
            enters += 1;
            let enter = *watch.enter.lock().unwrap_or_else(|p| p.into_inner());
            eprintln!("cm-daemon: rotation of {uid}: the notice sits unsent in the composer; Enter ({enters}/3)");
            let _ = handle.write_recovery_enter(enter, typed);
        } else if !sitting && !retyped && since >= 30 && quiet && watch.ready(Duration::from_secs(2)) {
            retyped = true;
            eprintln!("cm-daemon: rotation of {uid}: the notice is neither in the composer nor recorded; typing it again");
            deliver_live(state_arc, uid, text.clone());
        }
    }
    Delivered::Unconfirmed
}

pub(crate) fn deliver(state_arc: &Arc<Mutex<DaemonState>>, uid: &str, text: String) -> bool {
    deliver_with(state_arc, uid, text, true, false)
}

/// Deliver a notice to a session resuming a large transcript. Strict mode
/// waits for the real composer-ready signal instead of typing at the old
/// render-wait cap, where a resume can still be loading.
pub(crate) fn deliver_resume(state_arc: &Arc<Mutex<DaemonState>>, uid: &str, text: String) -> bool {
    deliver_with(state_arc, uid, text, true, true)
}

/// [`deliver`] to a session that is already running (`fresh: false`: no
/// wait for a startup render).
pub(crate) fn deliver_live(state_arc: &Arc<Mutex<DaemonState>>, uid: &str, text: String) -> bool {
    deliver_with(state_arc, uid, text, false, false)
}

fn deliver_with(state_arc: &Arc<Mutex<DaemonState>>, uid: &str, text: String, fresh: bool, strict_ready: bool) -> bool {
    let io = {
        let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        state.sessions.get(uid).map(|s| (s.input_handle(), s.fanout.clone()))
    };
    let Some((handle, fanout)) = io else { return false };
    crate::control::methods::deliver_agent_prompt(handle, fanout, uid.to_string(), text, fresh, strict_ready, Arc::clone(state_arc));
    true
}

fn finish(uid: &str, phase: &'static str, outcome: String) {
    eprintln!("cm-daemon: rotation of {uid}: {outcome}");
    unhold(uid, HoldOwner::Rotation);
    etas().lock().unwrap_or_else(|p| p.into_inner()).remove(uid);
    update(uid, |r| {
        r.phase = phase;
        r.outcome = Some(outcome);
        r.finished_at = Some(now());
    });
    *last_finished().lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
    let mut f = failures().lock().unwrap_or_else(|p| p.into_inner());
    if phase == "failed" {
        f.entry(uid.to_string()).or_default().0 += 1;
        if let Some(e) = f.get_mut(uid) {
            e.1 = now();
        }
    } else if f.remove(uid).is_none() {
        return;
    }
    save_failures(&f);
}

/// Where the failure counts persist (set at brain start, next to the
/// session registry), so a deploy does not retry a failing session at once.
fn failures_path() -> &'static std::sync::OnceLock<PathBuf> {
    static P: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    &P
}

fn save_failures(f: &HashMap<String, (u32, f64)>) {
    let Some(path) = failures_path().get() else { return };
    let v: serde_json::Map<String, Value> =
        f.iter().map(|(uid, (count, at))| (uid.clone(), json!({"count": count, "at": at}))).collect();
    if let Err(e) = crate::state::write_json_atomic(path, &Value::Object(v).to_string(), true) {
        eprintln!("cm-daemon: rotation failure counts not saved: {e}");
    }
}

/// Load the persisted failure counts (brain start).
fn load_failures(path: &Path) {
    let _ = failures_path().set(path.to_path_buf());
    let loaded: HashMap<String, (u32, f64)> = inflight_read(path)
        .into_iter()
        .filter_map(|(uid, v)| Some((uid, (v["count"].as_u64()? as u32, v["at"].as_f64()?))))
        .collect();
    failures().lock().unwrap_or_else(|p| p.into_inner()).extend(loaded);
}

/// A failed rotation: recorded, and Owner is alerted from the second
/// consecutive failure on (the policy backs off exponentially meanwhile).
fn fail(state_arc: &Arc<Mutex<DaemonState>>, uid: &str, outcome: String) {
    finish(uid, "failed", outcome.clone());
    let count = failures().lock().unwrap_or_else(|p| p.into_inner()).get(uid).map_or(0, |e| e.0);
    let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
    // A session left not running needs Owner now; a repeated failure on a
    // running one is worth knowing too.
    let message = if !state.sessions.contains_key(uid) {
        announce_not_back(&state, uid, KILLED_BY_FAILED);
        format!(
            "Transcript rotation of this session failed and it is NOT running now ({outcome}). \
             A-R revives it on its original transcript."
        )
    } else if count >= 2 {
        format!(
            "Transcript rotation of this session failed {count} times in a row (latest: {outcome}). \
             CM backs off and retries later; Alt+T retries now."
        )
    } else {
        return;
    };
    if let Err(e) = crate::owner_attention::raise_system(&state, uid, &message) {
        eprintln!("cm-daemon: rotation alert for {uid}: {e}");
    }
}

/// `killed_by` on the `Exited` a viewer gets when a rotation leaves the
/// session not running (the stop's own `Exited` said "transcript-rotation").
pub const KILLED_BY_FAILED: &str = "transcript-rotation failed";

/// Tell viewers that a uid a rotation or handoff stopped is NOT coming back:
/// they followed the stop as a respawn gap (#24 B2) and keep the row
/// reconnecting until they hear otherwise.
pub(crate) fn announce_not_back(state: &DaemonState, uid: &str, killed_by: &str) {
    state.manifest_watcher.broadcast(crate::manifest::ManifestDiff::Exited {
        uid: uid.to_string(),
        last_exit: crate::manifest::LastExit {
            code: None,
            memory_cap_kill: false,
            kills_file_offset: None,
            exited_at: now(),
            killed_by: Some(killed_by.to_string()),
        },
    });
}

/// Consecutive failed rotations per session: (count, last failure at).
fn failures() -> &'static Mutex<HashMap<String, (u32, f64)>> {
    static F: std::sync::OnceLock<Mutex<HashMap<String, (u32, f64)>>> = std::sync::OnceLock::new();
    F.get_or_init(|| Mutex::new(HashMap::new()))
}

fn last_finished() -> &'static Mutex<Option<Instant>> {
    static L: std::sync::OnceLock<Mutex<Option<Instant>>> = std::sync::OnceLock::new();
    L.get_or_init(|| Mutex::new(None))
}

fn pause_left() -> Option<Duration> {
    let at = (*last_finished().lock().unwrap_or_else(|p| p.into_inner()))?;
    PAUSE_BETWEEN.checked_sub(at.elapsed()).filter(|d| !d.is_zero())
}

/// Admit a rotation of `uid` on the host: no handoff running and no other
/// rotation, then claimed. Takes the state guard's target so it can only be
/// called with the state lock held: a handoff admits under the same lock
/// (`handoff::tick`), so the two cannot both get in (lock order state →
/// handoff → rotation).
pub(crate) fn admit(_locked: &DaemonState, uid: &str, rotation: Rotation) -> Result<(), String> {
    if crate::claude_recovery::internal_active(uid)
        || crate::claude_recovery::lifecycle_gate(uid).try_read().is_err()
    {
        return Err("Claude native background recovery is running".into());
    }
    if crate::claude_recovery::admission_blocked(uid) {
        return Err("Claude native background recovery is running or needs reconciliation".into());
    }
    if crate::codex_recovery::in_progress(uid) {
        return Err("native Codex continuation recovery is running".into());
    }
    if crate::handoff::in_progress(uid) {
        return Err("a handoff of this session is running".into());
    }
    claim(uid, rotation)
}

/// Register `rotation` for `uid` unless another rotation is running: one at a
/// time per host (a fork reads its whole source; a deploy may find many big
/// sessions). Checked and inserted under one lock, so two starters (Alt+T
/// and a policy pass) cannot both get in.
fn claim(uid: &str, rotation: Rotation) -> Result<(), String> {
    let mut reg = registry().lock().unwrap_or_else(|p| p.into_inner());
    if let Some((other, _)) = reg.iter().find(|(_, r)| r.finished_at.is_none()) {
        return Err(format!("another rotation is running ({other}); one at a time per host"));
    }
    reg.insert(uid.to_string(), rotation);
    Ok(())
}

/// The session being rotated now, if any (one at a time per host).
pub(crate) fn running_any() -> Option<String> {
    registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .find(|(_, r)| r.finished_at.is_none())
        .map(|(uid, _)| uid.clone())
}

// ---- in-flight markers (a brain restart mid-rotation) ------------------------

fn inflight_path(state: &DaemonState) -> Option<PathBuf> {
    state.daemon_sessions_path.as_ref().map(|p| p.with_file_name("rotations-inflight.json"))
}

fn inflight_read(path: &Path) -> serde_json::Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

pub(crate) fn inflight_put(state: &DaemonState, uid: &str, snap: &Snapshot) -> std::io::Result<()> {
    inflight_put_with(state, uid, snap, json!({}))
}

/// [`inflight_put`] with extra fields (#25: `kind: "handoff"` and the
/// handoff document's `file`).
pub(crate) fn inflight_put_with(state: &DaemonState, uid: &str, snap: &Snapshot, extra: Value) -> std::io::Result<()> {
    let Some(path) = inflight_path(state) else { return Ok(()) };
    let mut all = inflight_read(&path);
    let mut marker = json!({
        "workspace_id": snap.workspace_id,
        "worktree": snap.worktree,
        "from_id": snap.from_id,
        "entry": snap.entry,
        "at": now(),
    });
    if let (Some(m), Some(x)) = (marker.as_object_mut(), extra.as_object()) {
        m.extend(x.clone());
    }
    all.insert(uid.to_string(), marker);
    crate::state::write_json_atomic(&path, &Value::Object(all).to_string(), true)
}

pub(crate) fn inflight_remove(state: &DaemonState, uid: &str) {
    let Some(path) = inflight_path(state) else { return };
    let mut all = inflight_read(&path);
    if all.remove(uid).is_some() {
        let _ = if all.is_empty() {
            std::fs::remove_file(&path)
        } else {
            crate::state::write_json_atomic(&path, &Value::Object(all).to_string(), true)
        };
    }
}

/// At brain start: a rotation the previous brain left between stopping a
/// session and confirming its fork. A session that is not live is resumed
/// on its original transcript and told so; a live one (the fork came up) is
/// left as it is. Returns how many were resumed.
pub fn recover_inflight(state_arc: &Arc<Mutex<DaemonState>>) -> usize {
    let (path, all) = {
        let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        // Wake holds belong to rotations and handoffs of the previous brain;
        // none survives it (the recoveries below hold again if they run).
        let _ = messaging_root().set(state.messaging_root.clone());
        let cleared = crate::notifications::clear_all_holds(&state.messaging_root);
        if cleared > 0 {
            eprintln!("cm-daemon: lifted {cleared} wake hold(s) left by the previous brain");
        }
        if let Some(p) = state.daemon_sessions_path.as_ref().map(|p| p.with_file_name("rotation-arms.json")) {
            load_arms(&p);
        }
        if let Some(p) = state.daemon_sessions_path.as_ref().map(|p| p.with_file_name("rotation-failures.json")) {
            load_failures(&p);
        }
        let Some(path) = inflight_path(&state) else { return 0 };
        let all = inflight_read(&path);
        (path, all)
    };
    let mut resumed = 0;
    for (uid, v) in &all {
        let entry: Option<crate::manifest::ManifestEntry> = serde_json::from_value(v["entry"].clone()).ok();
        let (Some(entry), Some(ws), Some(wt)) = (entry, v["workspace_id"].as_str(), v["worktree"].as_str()) else {
            eprintln!("cm-daemon: rotation marker for {uid} is unreadable; dropped");
            continue;
        };
        let live_path = {
            let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
            state.sessions.get(uid).map(|s| s.transcript_path.clone())
        };
        if let Some(live_path) = live_path {
            if v["kind"] == "handoff" {
                // #25: the new engine is live (it came up before the
                // restart); tell it about its handoff document if it switched.
                let snap = Snapshot {
                    from_path: PathBuf::from(entry.transcript_path.clone().unwrap_or_default()),
                    from_id: v["from_id"].as_str().unwrap_or_default().to_string(),
                    from_bytes: 0,
                    workspace_id: ws.to_string(),
                    worktree: PathBuf::from(wt),
                    entry,
                };
                // Confirms or rolls back on its own thread; that thread clears
                // the marker when it is done.
                if !crate::handoff::recover_live(state_arc, uid, snap, v["file"].as_str()) {
                    inflight_remove(&state_arc.lock().unwrap_or_else(|p| p.into_inner()), uid);
                }
                continue;
            }
            let snap = Snapshot {
                from_path: PathBuf::from(entry.transcript_path.clone().unwrap_or_default()),
                from_id: v["from_id"].as_str().unwrap_or_default().to_string(),
                from_bytes: 0,
                workspace_id: ws.to_string(),
                worktree: PathBuf::from(wt),
                entry,
            };
            match live_path.map(PathBuf::from).filter(|p| p != &snap.from_path) {
                // The fork never started, or the fallback already ran.
                None => eprintln!("cm-daemon: rotation of {uid} was interrupted by a restart; it is on its original transcript"),
                // A live fork: finish what the previous brain started (its
                // marker stays until that is done).
                Some(to_path) => {
                    let fork_path = v["fork_path"].as_str().map(PathBuf::from);
                    if complete_after_restart(state_arc, uid, snap, to_path, fork_path) {
                        continue;
                    }
                }
            }
            inflight_remove(&state_arc.lock().unwrap_or_else(|p| p.into_inner()), uid);
            continue;
        }
        // A Claude fork that already holds turns is resumed, not discarded
        // (#114): the original lacks them.
        let mut entry = entry;
        let fork = v["fork_path"].as_str().map(PathBuf::from).filter(|p| fork_written(p).is_some());
        if let Some(fork) = &fork {
            entry.transcript_id = crate::session::transcript_id_from_path(&fork.to_string_lossy());
            entry.transcript_path = Some(fork.to_string_lossy().into_owned());
        }
        match crate::control::methods::restore_one_session_launch(state_arc, ws, Path::new(wt), &entry, None, false) {
            Ok(_) => {
                resumed += 1;
                rebroadcast_binding(state_arc, uid);
                let text = if fork.is_some() {
                    "[cm-rotation] A daemon restart interrupted CM while it was moving this session to a \
                     fresh transcript. You are on the new transcript, with everything you did there. Re-read \
                     your board items and inbox from CM, then carry on."
                } else {
                    "[cm-rotation] A daemon restart interrupted CM while it was moving this session (a \
                     transcript rotation or handoff). You are back on your original conversation; \
                     nothing was lost. Carry on."
                };
                deliver(state_arc, uid, text.into());
                eprintln!(
                    "cm-daemon: rotation of {uid} was interrupted by a restart; resumed its {} transcript",
                    if fork.is_some() { "forked" } else { "original" }
                );
            }
            Err((_, e)) => eprintln!("cm-daemon: rotation of {uid} was interrupted; resuming it failed: {e}"),
        }
        inflight_remove(&state_arc.lock().unwrap_or_else(|p| p.into_inner()), uid);
    }
    let _ = path;
    resumed
}

/// A fork the previous brain spawned but did not confirm: deliver the
/// after-notice, verify the fork records it, record the lineage; else roll
/// back to the original transcript. Runs on its own thread, holding the
/// host's one rotation slot.
fn complete_after_restart(state_arc: &Arc<Mutex<DaemonState>>, uid: &str, snap: Snapshot, to_path: PathBuf, fork_path: Option<PathBuf>) -> bool {
    let record = Rotation {
        phase: "verifying",
        origin: Origin::Policy,
        reason: "completing a rotation a daemon restart interrupted".into(),
        started_at: now(),
        from_id: snap.from_id.clone(),
        from_bytes: snap.from_bytes,
        to_id: crate::session::transcript_id_from_path(&to_path.to_string_lossy()),
        to_bytes: None,
        path: "fork",
        outcome: None,
        finished_at: None,
        requested_by: None,
    };
    if let Err(why) = claim(uid, record) {
        eprintln!("cm-daemon: rotation of {uid} interrupted by a restart; not completed now: {why}");
        return false;
    }
    hold(&state_arc.lock().unwrap_or_else(|p| p.into_inner()), uid, HoldOwner::Rotation);
    let state = Arc::clone(state_arc);
    let uid = uid.to_string();
    let spawned = std::thread::Builder::new().name(format!("cm-rotate-recover-{uid}")).spawn(move || {
        complete_fork(&state, &uid, &snap, &to_path, fork_path.as_deref());
        inflight_remove(&state.lock().unwrap_or_else(|p| p.into_inner()), &uid);
    });
    spawned.is_ok()
}

fn complete_fork(state: &Arc<Mutex<DaemonState>>, uid: &str, snap: &Snapshot, to_path: &Path, fork_path: Option<&Path>) {
    // The fork the previous brain pinned, if it said, else the live path.
    let pinned = fork_path.unwrap_or(to_path);
    if fork_written(pinned).is_some() || fork_written(to_path).is_some() {
        // It already took (its file holds messages): no notice needed to tell.
    } else if !deliver(state, uid, after_notice(snap.from_bytes)) {
        resume_old(state, uid, snap, "the forked session vanished after a daemon restart", Some(pinned));
        return;
    }
    set_phase(uid, "verifying", Some(verify_deadline(std::fs::metadata(&snap.from_path).map(|m| m.len()).unwrap_or(0))));
    let deadline = Instant::now() + verify_deadline(std::fs::metadata(&snap.from_path).map(|m| m.len()).unwrap_or(0));
    while Instant::now() < deadline {
        let live = state.lock().unwrap_or_else(|p| p.into_inner()).sessions.contains_key(uid);
        if let Some((path, bytes)) = [pinned, to_path].into_iter().find_map(|p| fork_written(p).map(|b| (p, b))) {
            if live {
                succeed(state, uid, snap, path, bytes, "completed after a restart");
            } else {
                keep_fork(state, uid, snap, path, "the fork exited after recording turns");
            }
            return;
        }
        if !live {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    resume_old(state, uid, snap, "the fork a daemon restart interrupted wrote no transcript", Some(pinned));
}

/// The rotation took: record the lineage (the A-s / A-e pickers label the
/// old file) and finish.
fn succeed(state_arc: &Arc<Mutex<DaemonState>>, uid: &str, snap: &Snapshot, to_path: &Path, bytes: u64, how: &str) {
    let to_id = crate::session::transcript_id_from_path(&to_path.to_string_lossy());
    update(uid, |r| {
        r.to_id = to_id.clone();
        r.to_bytes = Some(bytes);
    });
    if let Some(to) = to_id.as_deref() {
        let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        if let Err(e) = crate::resume_identity::record_rotation(&st, &snap.from_id, to) {
            eprintln!("cm-daemon: rotation of {uid}: lineage not recorded: {e}");
        }
    }
    finish(
        uid,
        "done",
        format!(
            "{how}: {} ({}) to {} ({})",
            snap.from_id,
            mb(snap.from_bytes),
            to_id.as_deref().unwrap_or("?"),
            mb(bytes)
        ),
    );
}

/// Why a fork that was not confirmed in time must still be kept (#114:
/// never discard history): Owner typed in it, its file exists, or it is
/// mid-turn. `None`: nothing would be lost by resuming the original.
fn keep_reason(state_arc: &Arc<Mutex<DaemonState>>, uid: &str, spawned: Instant, owner_typed: bool, candidates: &[PathBuf]) -> Option<String> {
    let (typed, working) = {
        let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        match st.sessions.get(uid) {
            Some(s) => (
                s.input_handle().operator_quiet_for().is_some_and(|q| q < spawned.elapsed()),
                crate::agent_state::current(s).state == crate::agent_state::State::Working,
            ),
            None => (false, false),
        }
    };
    if owner_typed || typed {
        return Some("Owner typed in the fork".into());
    }
    if let Some((p, len)) = candidates.iter().find_map(|p| std::fs::metadata(p).ok().filter(|m| m.len() > 0).map(|m| (p, m.len()))) {
        return Some(format!("its file {} exists ({}) but CM found no turn in it", p.display(), mb(len)));
    }
    working.then(|| "it is mid-turn".to_string())
}

/// Keep an unconfirmed (or already exited) fork rather than discard what it
/// holds: tell Owner, and leave the session on it.
fn keep_fork(state_arc: &Arc<Mutex<DaemonState>>, uid: &str, snap: &Snapshot, fork: &Path, why: &str) {
    let live = state_arc.lock().unwrap_or_else(|p| p.into_inner()).sessions.contains_key(uid);
    let fork_id = crate::session::transcript_id_from_path(&fork.to_string_lossy()).unwrap_or_default();
    if !live {
        // The fork exited after recording turns: bring it back on the fork,
        // not on the original (which lacks them).
        let mut entry = snap.entry.clone();
        entry.transcript_id = Some(fork_id.clone());
        entry.transcript_path = Some(fork.to_string_lossy().into_owned());
        if let Err((_, e)) = crate::control::methods::restore_one_session_launch(
            state_arc, &snap.workspace_id, &snap.worktree, &entry, None, false,
        ) {
            fail(state_arc, uid, format!("{why}; the fork {fork_id} holds new turns, but resuming it failed: {e}"));
            return;
        }
        rebroadcast_binding(state_arc, uid);
    }
    let bytes = std::fs::metadata(fork).map(|m| m.len()).unwrap_or(0);
    if fork_written(fork).is_some() {
        succeed(state_arc, uid, snap, fork, bytes, &format!("kept the fork ({why})"));
    } else {
        update(uid, |r| r.to_id = Some(fork_id.clone()));
        finish(uid, "done", format!("kept the unconfirmed fork {fork_id} ({why})"));
    }
    let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
    let message = format!(
        "CM moved this session to a fresh transcript ({fork_id}) but could not confirm it ({why}). CM kept \
         the fork rather than discard its history. If the session misbehaves, A-e Transcript can point it \
         back at {} before A-R.",
        snap.from_id
    );
    if let Err(e) = crate::owner_attention::raise_system(&st, uid, &message) {
        eprintln!("cm-daemon: rotation of {uid}: kept-fork alert: {e}");
    }
}

/// Re-send `uid`'s live transcript binding to every viewer. A fallback
/// resumes the transcript the row had before the fork, so the restore path
/// sees no change to broadcast, while viewers were told about the fork
/// (#114: Owner's A-e picker still marked the rolled-back fork "this").
pub(crate) fn rebroadcast_binding(state_arc: &Arc<Mutex<DaemonState>>, uid: &str) {
    let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(sess) = st.sessions.get(uid) {
        let entry = crate::control::methods::transcript_updated_entry(sess);
        crate::control::methods::broadcast_transcript_updated(&st, uid, entry);
    }
}

fn run(state_arc: &Arc<Mutex<DaemonState>>, uid: &str, snap: Snapshot) {
    // Session crons die with the process: note them before the stop so the
    // after-notice can hand them back (#114).
    let crons = crate::session_crons::capture(&state_arc.lock().unwrap_or_else(|p| p.into_inner()), uid);
    // 1. Stop the process. Viewers and monitors see killed_by
    //    `transcript-rotation`: the uid comes straight back.
    set_phase(uid, "stopping", Some(STOP_DEADLINE));
    match stop_tracked(state_arc, uid, ROTATION_KILLER, "transcript rotation") {
        Stopped::Yes => {}
        Stopped::Late => {
            resume_old(state_arc, uid, &snap, "the session stopped only after a delay", None);
            return;
        }
        Stopped::Stuck => {
            fail(state_arc, uid, "the kill was sent but the process is still running after 10 min".into());
            return;
        }
    }
    let carried = crate::session_crons::with_prompts(&crons, Some(&snap.from_path));
    let cron_file = if carried.is_empty() {
        None
    } else {
        crate::session_crons::save(&state_arc.lock().unwrap_or_else(|p| p.into_inner()), uid, "transcript rotation", &carried)
    };
    let cron_note = crate::session_crons::notice(&carried, cron_file.as_deref());
    // 2. Fork at the same uid.
    set_phase(uid, "forking", Some(Duration::from_secs(60)));
    if let Err((_, e)) = crate::control::methods::restore_one_session_launch(
        state_arc,
        &snap.workspace_id,
        &snap.worktree,
        &snap.entry,
        None,
        true,
    ) {
        resume_old(state_arc, uid, &snap, &format!("the fork did not start: {e}"), None);
        return;
    }
    let spawned = Instant::now();
    let claude = snap.entry.session_type == "claude-code";
    // The file a Claude fork writes is the one the daemon pinned with
    // `--session-id` and stamped at spawn: verification watches IT, whatever
    // the row's transcript_path says later.
    let pinned: Option<PathBuf> = claude
        .then(|| {
            let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
            st.sessions.get(uid).and_then(|s| s.transcript_path.clone()).map(PathBuf::from)
        })
        .flatten()
        .filter(|p| p != &snap.from_path);
    if let Some(p) = &pinned {
        update(uid, |r| r.to_id = crate::session::transcript_id_from_path(&p.to_string_lossy()));
        let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        if let Err(e) = inflight_put_with(&st, uid, &snap, json!({ "fork_path": p })) {
            eprintln!("cm-daemon: rotation of {uid}: in-flight marker not updated: {e}");
        }
    }
    // 3. The first message writes a Claude fork's file; a Codex fork's
    //    rollout is bound by the detector. Either way the new transcript must
    //    record a turn (a Claude fork) or the notice (its marker) before the
    //    rotation counts.
    let marker = format!(" {}", nonce());
    let needle = format!("[cm-rotation{marker}]");
    let requester = registry().lock().unwrap_or_else(|p| p.into_inner()).get(uid).and_then(|r| r.requested_by.clone());
    let notice = format!("{}{cron_note}", after_notice_for(snap.from_bytes, &marker, requester.as_deref()));
    set_phase(uid, "notifying", Some(verify_deadline(snap.from_bytes) + Duration::from_secs(120)));
    let delivered = if claude {
        // A large fork loads its history after the process is up: wait for
        // its composer, then make sure the notice was submitted (#24: a 1 GB
        // fork took the notice while still loading and never sent it).
        deliver_ready(state_arc, uid, notice, &needle, snap.from_bytes,
                      pinned.as_deref().map(|p| (p, 0)), &format!("rotation-after:{uid}{marker}"))
    } else if deliver(state_arc, uid, notice) {
        Delivered::Unconfirmed
    } else {
        Delivered::Vanished
    };
    if delivered == Delivered::Vanished {
        resume_old(state_arc, uid, &snap, "the forked session vanished before its first message", pinned.as_deref());
        return;
    }
    set_phase(uid, "verifying", Some(verify_deadline(snap.from_bytes)));
    let deadline = Instant::now() + verify_deadline(snap.from_bytes);
    let mut first_len: Option<(PathBuf, u64)> = None;
    let mut candidates: Vec<PathBuf> = pinned.iter().cloned().collect();
    while Instant::now() < deadline {
        let (live, live_path) = {
            let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
            let sess = st.sessions.get(uid);
            (sess.is_some(), sess.and_then(|s| s.transcript_path.clone()).map(PathBuf::from))
        };
        if let Some(p) = live_path.filter(|p| p != &snap.from_path && !candidates.contains(p)) {
            candidates.push(p);
        }
        for to_path in &candidates {
            if first_len.as_ref().is_none_or(|(p, _)| p != to_path) && Some(to_path) != pinned.as_ref() {
                first_len = Some((to_path.clone(), std::fs::metadata(to_path).map(|m| m.len()).unwrap_or(0)));
            }
            let since = first_len.as_ref().filter(|(p, _)| p == to_path).map_or(0, |(_, n)| *n);
            if let Some(bytes) = fork_written(to_path).filter(|_| fork_took(to_path, &needle, claude, since)) {
                if live {
                    succeed(state_arc, uid, &snap, to_path, bytes, "rotated");
                } else {
                    // It took, then exited: bring it back on the fork.
                    keep_fork(state_arc, uid, &snap, to_path, "the fork exited after recording turns");
                }
                return;
            }
        }
        if !live {
            resume_old(state_arc, uid, &snap, "the forked session exited before writing its transcript", pinned.as_deref());
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let why = "the fork wrote no transcript in time (API error or a menu?)";
    if claude {
        if let (Some(fork), Some(reason)) = (&pinned, keep_reason(state_arc, uid, spawned, delivered == Delivered::OwnerTyped, &candidates)) {
            keep_fork(state_arc, uid, &snap, fork, &format!("{why}, but {reason}"));
            return;
        }
    }
    resume_old(state_arc, uid, &snap, why, pinned.as_deref());
}

/// The fork did not take: stop whatever runs at the uid and resume the old
/// transcript, as it was. A Claude `fork` whose file holds messages is
/// never discarded (#114): it is kept (and resumed if it exited) instead.
fn resume_old(state_arc: &Arc<Mutex<DaemonState>>, uid: &str, snap: &Snapshot, why: &str, fork: Option<&Path>) {
    if let Some(fork) = fork.filter(|p| fork_written(p).is_some()) {
        keep_fork(state_arc, uid, snap, fork, &format!("{why}, but the fork holds new turns"));
        return;
    }
    let source_bytes = std::fs::metadata(&snap.from_path).map(|m| m.len()).unwrap_or(snap.from_bytes);
    set_phase(uid, "falling back", Some(STOP_DEADLINE + verify_deadline(source_bytes) + Duration::from_secs(120)));
    let live = state_arc.lock().unwrap_or_else(|p| p.into_inner()).sessions.contains_key(uid);
    if live {
        if stop_tracked(state_arc, uid, ROTATION_KILLER, "transcript rotation") == Stopped::Stuck {
            fail(state_arc, uid, format!("{why}; the forked process is still running 10 min after its kill"));
            return;
        }
    }
    match crate::control::methods::restore_one_session_launch(
        state_arc,
        &snap.workspace_id,
        &snap.worktree,
        &snap.entry,
        None,
        false,
    ) {
        Ok(_) => {
            // Viewers were told about the fork: point them back.
            rebroadcast_binding(state_arc, uid);
            if snap.entry.session_type == "claude-code" {
                // The original transcript is the large one: same readiness,
                // but a turn it records proves it is up (#114: this wait
                // used to run its full 239 s a second time).
                let key = format!("rotation-fallback:{uid}:{}", nonce());
                deliver_ready(state_arc, uid, fallback_notice(why), FALLBACK_NEEDLE, snap.from_bytes,
                              Some((&snap.from_path, source_bytes)), &key);
            } else {
                deliver(state_arc, uid, fallback_notice(why));
            }
            // Again, for a viewer that was still reattaching the first time.
            rebroadcast_binding(state_arc, uid);
            update(uid, |r| r.path = "fallback-resume");
            fail(state_arc, uid, format!("{why}; resumed the original transcript {}", snap.from_id));
        }
        Err((_, e)) => fail(state_arc, uid, format!("{why}; resuming the original also failed: {e}")),
    }
}

// ---- the automatic policy -------------------------------------------------

/// Compaction counts per transcript, scanned incrementally (offset, count).
fn compaction_cache() -> &'static Mutex<HashMap<PathBuf, (u64, u64)>> {
    static CACHE: std::sync::OnceLock<Mutex<HashMap<PathBuf, (u64, u64)>>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `compact_boundary` rows in `path`, reading only what was appended since
/// the last call (the files only grow; a shrink restarts the count).
pub fn compactions(path: &Path) -> u64 {
    use std::io::{BufRead, Seek, SeekFrom};
    let Ok(meta) = std::fs::metadata(path) else { return 0 };
    let len = meta.len();
    let mut cache = compaction_cache().lock().unwrap_or_else(|p| p.into_inner());
    let (mut offset, mut count) = cache.get(path).copied().unwrap_or((0, 0));
    if len < offset {
        (offset, count) = (0, 0);
    }
    if let Ok(mut f) = std::fs::File::open(path) {
        if f.seek(SeekFrom::Start(offset)).is_ok() {
            let mut reader = std::io::BufReader::with_capacity(1 << 20, f);
            let mut line = Vec::new();
            loop {
                line.clear();
                match reader.read_until(b'\n', &mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if line.last() != Some(&b'\n') {
                            break; // a partial last line: count it next time
                        }
                        offset += n as u64;
                        if line.windows(18).any(|w| w == b"\"compact_boundary\"") {
                            count += 1;
                        }
                    }
                }
            }
        }
    }
    cache.insert(path.to_path_buf(), (offset, count));
    count
}

/// Sessions whose before-notice went out (uid → transcript id it was for).
fn noticed() -> &'static Mutex<HashMap<String, String>> {
    static N: std::sync::OnceLock<Mutex<HashMap<String, String>>> = std::sync::OnceLock::new();
    N.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Why a transcript is over the policy's thresholds, if it is.
pub fn over_threshold(cfg: &crate::config::TranscriptRotationConfig, bytes: u64, compactions: u64) -> Option<String> {
    if cfg.max_bytes > 0 && bytes >= cfg.max_bytes {
        return Some(format!("transcript is {} (threshold {})", mb(bytes), mb(cfg.max_bytes)));
    }
    if cfg.max_compactions > 0 && compactions >= cfg.max_compactions {
        return Some(format!("{compactions} compactions (threshold {})", cfg.max_compactions));
    }
    None
}

fn before_notice(why: &str) -> String {
    format!(
        "[cm-rotation] Heads-up: your {why}. Claude Code slows down badly on transcripts this \
         large, so CM will move this conversation to a fresh transcript right after your next \
         turn ends (with no tool or background job running). You will continue from your latest \
         compaction summary at the same CM session, name, task and workspace; messages that \
         arrive meanwhile wait for the new transcript. Keep anything a successor would need in \
         files, board items or notes, as usual; nothing to do otherwise."
    )
}

/// A rotation asked for but not at a boundary yet: an agent's own request,
/// or an operator's forced one that met a turn in progress. Carried out at
/// the session's next turn end ([`on_turn_end`]), else by the policy pass.
#[derive(Clone, Debug)]
struct Arm {
    reason: String,
    origin: Origin,
    armed_at: f64,
    requested_by: Option<String>,
}

fn arms() -> &'static Mutex<HashMap<String, Arm>> {
    static R: std::sync::OnceLock<Mutex<HashMap<String, Arm>>> = std::sync::OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Where arms persist (set at brain start), so a restart keeps them.
fn arms_path() -> &'static std::sync::OnceLock<PathBuf> {
    static P: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    &P
}

fn save_arms(arms: &HashMap<String, Arm>) {
    let Some(path) = arms_path().get() else { return };
    let v: serde_json::Map<String, Value> = arms
        .iter()
        .map(|(uid, a)| {
            (uid.clone(), json!({"reason": a.reason, "origin": a.origin.as_str(), "armed_at": a.armed_at,
                                 "requested_by": a.requested_by}))
        })
        .collect();
    if let Err(e) = crate::state::write_json_atomic(path, &Value::Object(v).to_string(), true) {
        eprintln!("cm-daemon: rotation arms not saved: {e}");
    }
}

/// Load persisted arms (brain start).
fn load_arms(path: &Path) {
    let _ = arms_path().set(path.to_path_buf());
    let loaded: HashMap<String, Arm> = inflight_read(path)
        .into_iter()
        .filter_map(|(uid, v)| {
            let origin = match v["origin"].as_str()? {
                "operator" => Origin::Operator,
                "agent" => Origin::Agent,
                _ => Origin::Policy,
            };
            Some((uid, Arm {
                reason: v["reason"].as_str()?.to_string(),
                origin,
                armed_at: v["armed_at"].as_f64()?,
                requested_by: v["requested_by"].as_str().map(str::to_string),
            }))
        })
        .collect();
    if !loaded.is_empty() {
        eprintln!("cm-daemon: {} armed rotation(s) carried over from the previous brain", loaded.len());
    }
    arms().lock().unwrap_or_else(|p| p.into_inner()).extend(loaded);
}

fn arm_insert(uid: &str, arm: Arm) {
    let mut a = arms().lock().unwrap_or_else(|p| p.into_inner());
    a.insert(uid.to_string(), arm);
    save_arms(&a);
}

fn arm_remove(uid: &str) {
    let mut a = arms().lock().unwrap_or_else(|p| p.into_inner());
    if a.remove(uid).is_some() {
        save_arms(&a);
    }
}

/// Whether a rotation of `uid` is armed (waits for its next turn end).
pub fn armed(uid: &str) -> bool {
    arms().lock().unwrap_or_else(|p| p.into_inner()).contains_key(uid)
}

/// An armed rotation that found no usable turn end in this long is dropped.
const ARM_EXPIRY_SECS: f64 = 6.0 * 3600.0;

/// How long the hold on new wakes lasts at most (a daemon that dies with a
/// hold in place stops holding after this).
const HOLD_SECS: f64 = 30.0 * 60.0;

/// Who holds a uid's wakes. Each feature lifts only its own hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HoldOwner {
    /// A due rotation waiting for a clean turn end (capped per episode).
    Due,
    /// A rotation that is running.
    Rotation,
    /// A handoff that is running.
    Handoff,
    /// Native Codex continuation recovery in the existing process.
    CodexRecovery,
    /// A guarded ordinary Claude limit-recovery prompt.
    ClaudeAccountRecovery,
}

struct Hold {
    root: PathBuf,
    owner: HoldOwner,
    /// When this due episode's hold began (the cap counts from here).
    episode_start: f64,
}

fn holds() -> &'static Mutex<HashMap<String, Hold>> {
    static H: std::sync::OnceLock<Mutex<HashMap<String, Hold>>> = std::sync::OnceLock::new();
    H.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The messaging root, for lifting a hold this brain has no record of.
fn messaging_root() -> &'static std::sync::OnceLock<PathBuf> {
    static R: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    &R
}

/// A due session's wakes are held for at most this long in all (one
/// episode); renewals never extend it, so wakes are delayed minutes, not
/// hours.
const DUE_HOLD_CAP_SECS: f64 = 10.0 * 60.0;
/// After a spent episode, no new due hold for this long.
const DUE_HOLD_REST_SECS: f64 = 60.0 * 60.0;

/// Hold `uid`'s new wakes for the new process (`notifications::set_hold`).
/// A due hold never replaces a running rotation's or handoff's, and stops
/// renewing once its episode has held for [`DUE_HOLD_CAP_SECS`]. Returns
/// whether wakes are held now.
pub(crate) fn hold(state: &DaemonState, uid: &str, owner: HoldOwner) -> bool {
    let _ = messaging_root().set(state.messaging_root.clone());
    let t = now();
    let mut h = holds().lock().unwrap_or_else(|p| p.into_inner());
    let (episode_start, until) = match (owner, h.get(uid)) {
        (HoldOwner::Due, Some(e)) if e.owner != HoldOwner::Due => return true,
        (HoldOwner::Due, Some(e)) => {
            let age = t - e.episode_start;
            if age >= DUE_HOLD_CAP_SECS + DUE_HOLD_REST_SECS {
                (t, t + DUE_HOLD_CAP_SECS)
            } else if age >= DUE_HOLD_CAP_SECS {
                return false;
            } else {
                (e.episode_start, e.episode_start + DUE_HOLD_CAP_SECS)
            }
        }
        (HoldOwner::Due, None) => (t, t + DUE_HOLD_CAP_SECS),
        (_, _) => (t, t + HOLD_SECS),
    };
    if let Err(e) = crate::notifications::set_hold(&state.messaging_root, uid, until) {
        eprintln!("cm-daemon: could not hold wakes of {uid}: {e}");
        return false;
    }
    h.insert(uid.to_string(), Hold { root: state.messaging_root.clone(), owner, episode_start });
    true
}

/// Lift `owner`'s hold on `uid`: queued wakes go to whatever runs there now.
/// Another owner's hold is left alone. A spent due episode keeps its record
/// (so it is not renewed at once), only the file goes.
pub(crate) fn unhold(uid: &str, owner: HoldOwner) {
    let mut h = holds().lock().unwrap_or_else(|p| p.into_inner());
    let root = match h.get(uid) {
        Some(e) if e.owner != owner => return,
        Some(e) => {
            let root = e.root.clone();
            let spent = owner == HoldOwner::Due && now() - e.episode_start >= DUE_HOLD_CAP_SECS;
            if !spent {
                h.remove(uid);
            }
            root
        }
        // Nothing in this brain owns it (a hold from before a restart).
        None => match messaging_root().get() {
            Some(root) => root.clone(),
            None => return,
        },
    };
    crate::notifications::clear_hold(&root, uid);
}

/// Who holds `uid`'s wakes in this brain, if anyone.
#[cfg(test)]
fn hold_owner(uid: &str) -> Option<HoldOwner> {
    holds().lock().unwrap_or_else(|p| p.into_inner()).get(uid).map(|h| h.owner)
}

/// Arm a rotation of `uid` for its next turn end, holding new wakes so that
/// turn end is not raced by one. Returns what it will wait for.
pub fn arm(state: &DaemonState, uid: &str, reason: &str, origin: Origin) -> Result<Value, (ErrorCode, String)> {
    arm_by(state, uid, reason, origin, None)
}

/// [`arm`], recording which other session asked (`requested_by`).
pub fn arm_by(
    state: &DaemonState,
    uid: &str,
    reason: &str,
    origin: Origin,
    requested_by: Option<String>,
) -> Result<Value, (ErrorCode, String)> {
    match state.sessions.get(uid) {
        Some(s) if matches!(s.session_type.as_str(), "claude-code" | "codex") && s.transcript_path.is_some() => {}
        Some(s) if !matches!(s.session_type.as_str(), "claude-code" | "codex") => {
            return Err((ErrorCode::InvalidParams, format!("only Claude and Codex sessions rotate (this is {})", s.session_type)))
        }
        Some(_) => return Err((ErrorCode::Conflict, "the session has no bound transcript yet".into())),
        None => return Err((ErrorCode::NotFound, format!("session '{uid}' is not live on this host"))),
    }
    arm_insert(uid, Arm { reason: reason.to_string(), origin, armed_at: now(), requested_by });
    hold(state, uid, HoldOwner::Due);
    Ok(json!({
        "ok": true,
        "queued": true,
        "armed": true,
        "session_uid": uid,
        "note": "CM moves this conversation to a fresh transcript as soon as its current turn ends \
                 (within a second or two). New wakes wait for the new transcript. End your turn normally.",
    }))
}

/// Queue `uid`'s own request (an agent asks mid-turn): armed for its next
/// turn end.
pub fn request(state: &DaemonState, uid: &str, reason: &str) -> Result<Value, (ErrorCode, String)> {
    arm(state, uid, reason, Origin::Agent)
}

/// An agent's request for its own session (`requested_by: None`) or, with
/// authority checked by the caller, for another one: armed for that
/// session's next turn end, at its own safe boundary.
pub fn request_by(
    state: &DaemonState,
    uid: &str,
    reason: &str,
    requested_by: Option<String>,
) -> Result<Value, (ErrorCode, String)> {
    arm_by(state, uid, reason, Origin::Agent, requested_by)
}

/// Called when `uid` reports a turn end (`session.turn_ended` /
/// `session.agent_report`). A session that is due (armed, or over the size
/// threshold) rotates right then, within about a second, before the next
/// wake starts another turn. Runs on its own thread; returns at once.
pub fn on_turn_end(state_arc: &Arc<Mutex<DaemonState>>, uid: &str) {
    if in_progress(uid) || crate::handoff::in_progress(uid) {
        return;
    }
    let due = armed(uid) || {
        let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        policy_due(&st, uid).is_some()
    };
    if !due {
        return;
    }
    let state = Arc::clone(state_arc);
    let uid = uid.to_string();
    let _ = std::thread::Builder::new().name(format!("cm-rotate-turn-{uid}")).spawn(move || {
        // The Stop report can reach the daemon before the agent's state
        // source (presence, the relay) says idle; that observation lands a
        // moment later, backdated. Start the moment it reads idle; if it
        // never does in the window (a wake already started the next turn),
        // try anyway: the refusal holds new wakes for the next turn end.
        let deadline = Instant::now() + TURN_END_WINDOW;
        loop {
            std::thread::sleep(TURN_END_POLL);
            let idle = {
                let st = state.lock().unwrap_or_else(|p| p.into_inner());
                st.sessions
                    .get(&uid)
                    .is_some_and(|s| crate::agent_state::current(s).state == crate::agent_state::State::Idle)
            };
            if idle || Instant::now() >= deadline {
                break;
            }
        }
        try_due(&state, &uid);
    });
}

/// How long after a reported turn end the agent may take to read idle.
const TURN_END_WINDOW: Duration = Duration::from_secs(5);
const TURN_END_POLL: Duration = Duration::from_millis(100);

/// Why the policy wants `uid` rotated now, if it does (over the size
/// threshold, not backing off, a Claude session the boundary can accept).
fn policy_due(state: &DaemonState, uid: &str) -> Option<String> {
    let cfg = &state.config.transcript_rotation;
    if !cfg.enabled || in_progress(uid) || crate::handoff::in_progress(uid) || recently_failed(uid) {
        return None;
    }
    let s = state.sessions.get(uid)?;
    if s.session_type != "claude-code" || s.workflow_run_id.is_some() {
        return None;
    }
    let path = PathBuf::from(s.transcript_path.clone()?);
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if rotation_did_not_shrink(uid, &path, cfg.max_bytes) {
        return None;
    }
    // Size only here (a stat); compaction counts are the policy pass's.
    over_threshold(cfg, bytes, 0)
}

/// Whether `path` is the transcript `uid`'s last rotation produced and it is
/// still over the threshold: rotating it again would not help (a fork keeps
/// everything after the last compaction; under 5 MB Claude copies the whole
/// file), so the policy leaves it until it has a new transcript.
fn rotation_did_not_shrink(uid: &str, path: &Path, max_bytes: u64) -> bool {
    let id = crate::session::transcript_id_from_path(&path.to_string_lossy());
    registry().lock().unwrap_or_else(|p| p.into_inner()).get(uid).is_some_and(|r| {
        r.phase == "done" && r.to_id.is_some() && r.to_id == id && r.to_bytes.is_some_and(|b| max_bytes > 0 && b >= max_bytes)
    })
}

/// Retire `uid`'s receipts that will never be confirmed.
fn retire_stale_receipts(state: &DaemonState, uid: &str) {
    match crate::notifications::retire_stale(&state.messaging_root, uid, RETIRE_RECEIPT_SECS, now()) {
        Ok(0) => {}
        Ok(n) => eprintln!("cm-daemon: retired {n} stale unconfirmed notification receipt(s) of {uid}"),
        Err(e) => eprintln!("cm-daemon: retiring stale receipts of {uid}: {e}"),
    }
}

/// The turn-end attempt for a due session.
fn try_due(state_arc: &Arc<Mutex<DaemonState>>, uid: &str) {
    // A rotation or handoff already running owns this session (and its hold).
    if in_progress(uid) || crate::handoff::in_progress(uid) {
        return;
    }
    retire_stale_receipts(&state_arc.lock().unwrap_or_else(|p| p.into_inner()), uid);
    let arm = arms().lock().unwrap_or_else(|p| p.into_inner()).get(uid).cloned();
    let (reason, origin, checks) = match arm {
        // An armed request: no idle wait, no Owner-quiet window; only an
        // operator's (Owner's) own request skips the unsent-draft check.
        Some(a) => (
            a.reason,
            a.origin,
            Checks { idle: Duration::ZERO, owner_quiet: Duration::ZERO, draft: a.origin != Origin::Operator, at_turn_end: true, crons_block: false },
        ),
        None => {
            let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
            let Some(why) = policy_due(&st, uid) else { return };
            if notice_first(&st, uid, &why) {
                // The notice's own turn ends next; rotate then.
                return;
            }
            let quiet = Duration::from_secs(st.config.transcript_rotation.owner_quiet_secs);
            (why, Origin::Policy, Checks { idle: Duration::ZERO, owner_quiet: quiet, draft: true, at_turn_end: true, crons_block: false })
        }
    };
    match start_with(state_arc, uid, &reason, origin, checks) {
        Ok(_) => eprintln!("cm-daemon: rotating {uid} at its turn end: {reason}"),
        Err((_, why)) if permanent_refusal(&why) => {
            eprintln!("cm-daemon: rotation of {uid} dropped: {why}");
            arm_remove(uid);
            unhold(uid, HoldOwner::Due);
        }
        Err((_, why)) => {
            eprintln!("cm-daemon: rotation of {uid} deferred at its turn end: {why}");
            // A wake raced this turn end, or the agent went straight on:
            // hold new wakes so the next turn end is clean. A lasting reason
            // (Owner around, a scheduled wakeup) lets them flow meanwhile.
            // Only this feature's own due hold is renewed or lifted, and a
            // due hold is capped per episode (`hold`).
            if hold_worthy(&why) {
                let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
                hold(&st, uid, HoldOwner::Due);
            } else {
                unhold(uid, HoldOwner::Due);
            }
        }
    }
}

/// A session exited on its own (or was killed by someone else): its arm and
/// due hold go with it. A rotation's or handoff's own stop is theirs to
/// handle. Called from `handle_session_exit` (state lock held).
pub(crate) fn on_session_exit(uid: &str) {
    if in_progress(uid) || crate::handoff::in_progress(uid) {
        return;
    }
    arm_remove(uid);
    unhold(uid, HoldOwner::Due);
}

/// Publish the policy's before-notice for `uid`'s current transcript, once.
/// Returns whether it went out now (the rotation then waits a turn).
fn notice_first(state: &DaemonState, uid: &str, why: &str) -> bool {
    let Some(path) = state.sessions.get(uid).and_then(|s| s.transcript_path.clone()) else { return false };
    let id = crate::session::transcript_id_from_path(&path).unwrap_or_default();
    let first = {
        let mut n = noticed().lock().unwrap_or_else(|p| p.into_inner());
        n.get(uid) != Some(&id) && {
            n.insert(uid.to_string(), id.clone());
            true
        }
    };
    if first {
        let marker = format!("[cm-rotation-notice {id}]");
        let text = format!("{marker} {}", before_notice(why));
        match crate::notifications::publish(
            &state.messaging_root,
            uid,
            &format!("rotation-notice:{id}"),
            "rotation",
            &text,
            &marker,
        ) {
            Ok(_) => {}
            // This transcript was already noticed (by an earlier brain, with
            // older wording): nothing more to say before rotating.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return false,
            Err(e) => {
                eprintln!("cm-daemon: rotation notice for {uid}: {e}");
                return false;
            }
        }
    }
    first
}

/// After a failed rotation the policy leaves the session alone this long.
const FAILURE_BACKOFF_SECS: f64 = 6.0 * 3600.0;

/// Whether the policy should still leave `uid` alone after failures: 6 h
/// after the first, doubling with each consecutive one, at most a week.
fn recently_failed(uid: &str) -> bool {
    failures().lock().unwrap_or_else(|p| p.into_inner()).get(uid).is_some_and(|&(count, at)| {
        now() - at < backoff_secs(count)
    })
}

fn backoff_secs(failures: u32) -> f64 {
    (FAILURE_BACKOFF_SECS * 2f64.powi(failures.saturating_sub(1).min(16) as i32)).min(7.0 * 86400.0)
}

/// One policy pass: notice, then rotate at a safe boundary.
pub fn tick(state_arc: &Arc<Mutex<DaemonState>>) {
    let (cfg, candidates) = {
        let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        let cfg = state.config.transcript_rotation.clone();
        if !cfg.enabled {
            return;
        }
        let candidates: Vec<(String, PathBuf)> = state
            .sessions
            .iter()
            .filter(|(_, s)| s.session_type == "claude-code")
            // The boundary always refuses these: no notice, no attempt.
            .filter(|(_, s)| s.workflow_run_id.is_none())
            .filter(|(_, s)| {
                s.continuous_task_id
                    .as_deref()
                    .is_none_or(|ct| crate::continuous::task::load_one(ct).is_none_or(|t| t.in_flight.is_none()))
            })
            .filter_map(|(uid, s)| Some((uid.clone(), PathBuf::from(s.transcript_path.clone()?))))
            .collect();
        (cfg, candidates)
    };
    // Armed requests first (a fallback: they normally run at the next turn
    // end). No threshold, no before-notice (they asked), only the boundary.
    let asked: Vec<(String, f64)> =
        arms().lock().unwrap_or_else(|p| p.into_inner()).iter().map(|(u, a)| (u.clone(), a.armed_at)).collect();
    for (uid, armed_at) in asked {
        if now() - armed_at > ARM_EXPIRY_SECS {
            eprintln!("cm-daemon: armed rotation of {uid} expired without a usable turn end");
            arm_remove(&uid);
            unhold(&uid, HoldOwner::Due);
            continue;
        }
        try_due(state_arc, &uid);
    }
    // Sizes first (a stat each); compactions are counted only for files big
    // enough to matter, off a low-priority thread (the first pass after a
    // brain start reads them from the beginning).
    let sized: Vec<(String, PathBuf, u64)> = candidates
        .into_iter()
        .map(|(uid, path)| {
            let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            (uid, path, bytes)
        })
        .collect();
    let scan: Vec<PathBuf> = sized
        .iter()
        .filter(|(_, _, bytes)| cfg.max_compactions > 0 && *bytes >= compaction_scan_floor(&cfg))
        .map(|(_, p, _)| p.clone())
        .collect();
    let counted = count_compactions_quietly(scan);
    for (uid, path, bytes) in sized {
        if armed(&uid) {
            continue;
        }
        if rotation_did_not_shrink(&uid, &path, cfg.max_bytes) {
            continue;
        }
        let why = over_threshold(&cfg, bytes, counted.get(&path).copied().unwrap_or(0));
        let Some(why) = why else { continue };
        if recently_failed(&uid) {
            continue;
        }
        retire_stale_receipts(&state_arc.lock().unwrap_or_else(|p| p.into_inner()), &uid);
        if notice_first(&state_arc.lock().unwrap_or_else(|p| p.into_inner()), &uid, &why) {
            continue; // rotate at its next turn end, after the notice
        }
        match start(state_arc, &uid, &why, Origin::Policy, false) {
            Ok(_) => {
                eprintln!("cm-daemon: rotating {uid}: {why}");
                return; // one per pass; the next waits for it and a pause
            }
            Err((_, refusal)) => {
                eprintln!("cm-daemon: rotation of {uid} deferred: {refusal}")
            }
        }
    }
}

/// Files smaller than this are not scanned for compactions: a quarter of
/// the size threshold (a session that compacts that often grows fast).
fn compaction_scan_floor(cfg: &crate::config::TranscriptRotationConfig) -> u64 {
    if cfg.max_bytes == 0 {
        0
    } else {
        cfg.max_bytes / 4
    }
}

/// [`compactions`] for each path, on a short-lived thread at the lowest CPU
/// and idle I/O priority. (Niceness is per thread and inherited, so the
/// policy thread itself, whose rotation workers respawn sessions, keeps its
/// own.)
fn count_compactions_quietly(paths: Vec<PathBuf>) -> HashMap<PathBuf, u64> {
    if paths.is_empty() {
        return HashMap::new();
    }
    std::thread::Builder::new()
        .name("cm-rotation-scan".into())
        .spawn(move || {
            lower_priority();
            paths.into_iter().map(|p| {
                let n = compactions(&p);
                (p, n)
            }).collect()
        })
        .ok()
        .and_then(|h| h.join().ok())
        .unwrap_or_default()
}

/// nice 19 and the idle I/O class, for the calling thread only.
fn lower_priority() {
    // SAFETY: plain syscalls on the calling thread's own id.
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::setpriority(libc::PRIO_PROCESS, tid, 19);
        const IOPRIO_WHO_PROCESS: libc::c_int = 1;
        const IOPRIO_CLASS_IDLE: libc::c_int = 3;
        libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, tid as libc::c_int, IOPRIO_CLASS_IDLE << 13);
    }
}

/// The policy thread (checks every `check_secs`; config re-read each pass).
pub fn start_policy(state_arc: &Arc<Mutex<DaemonState>>) {
    let state = Arc::clone(state_arc);
    let _ = std::thread::Builder::new().name("cm-rotation-policy".into()).spawn(move || loop {
        let secs = state.lock().unwrap_or_else(|p| p.into_inner()).config.transcript_rotation.check_secs.max(30);
        std::thread::sleep(Duration::from_secs(secs));
        tick(&state);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds_name_the_reason() {
        let cfg = crate::config::TranscriptRotationConfig::default();
        assert!(over_threshold(&cfg, 799_000_000, 10_000).is_none(), "the first release counts no compactions");
        assert!(over_threshold(&cfg, 800_000_000, 0).unwrap().contains("800 MB"));
        let cfg = crate::config::TranscriptRotationConfig { max_compactions: 200, ..cfg };
        assert!(over_threshold(&cfg, 1_000, 200).unwrap().contains("200 compactions"));
        assert_eq!(compaction_scan_floor(&cfg), 200_000_000);
        let off = crate::config::TranscriptRotationConfig { max_bytes: 0, max_compactions: 0, ..cfg };
        assert!(over_threshold(&off, u64::MAX, u64::MAX).is_none());
    }

    #[test]
    fn deadlines_and_backoff_scale() {
        assert_eq!(verify_deadline(9_000_000), Duration::from_secs_f64(180.54));
        assert_eq!(verify_deadline(0), Duration::from_secs(180));
        assert_eq!(verify_deadline(1_700_000_000), Duration::from_secs(282));
        assert_eq!(verify_deadline(u64::MAX), Duration::from_secs(1200));
        assert_eq!(backoff_secs(1), 6.0 * 3600.0);
        assert_eq!(backoff_secs(3), 24.0 * 3600.0);
        assert_eq!(backoff_secs(30), 7.0 * 86400.0);
    }

    #[test]
    fn only_one_rotation_can_claim_the_host() {
        let _serial = test_registry_lock();
        let r = |uid: &str| Rotation {
            phase: "stopping", origin: Origin::Operator, reason: String::new(), started_at: now(),
            from_id: uid.into(), from_bytes: 0, to_id: None, to_bytes: None, path: "fork",
            outcome: None, finished_at: None, requested_by: None,
        };
        claim("ts-claim-a", r("a")).unwrap();
        assert!(claim("ts-claim-b", r("b")).unwrap_err().contains("ts-claim-a"));
        assert_eq!(running_any().as_deref(), Some("ts-claim-a"));
        finish("ts-claim-a", "done", "ok".into());
        claim("ts-claim-b", r("b")).unwrap();
        finish("ts-claim-b", "done", "ok".into());
    }

    #[test]
    fn a_live_unconfirmed_fork_is_completed_after_a_restart() {
        let _serial = test_registry_lock();
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old-id.jsonl");
        let fork = dir.path().join("new-id.jsonl");
        std::fs::write(&old, "{\"type\":\"user\",\"parentUuid\":null}\n").unwrap();
        std::fs::write(&fork, "{\"type\":\"user\",\"parentUuid\":null}\n").unwrap();
        let mut state = DaemonState::new();
        state.daemon_sessions_path = Some(dir.path().join("daemon-sessions.json"));
        let mut sp = crate::session::SpawnParams::new("ts-recover-0", "r", "/bin/sleep");
        sp.args = vec!["30".into()];
        sp.session_type = "claude-code".into();
        let mut sess = crate::session::DaemonSession::spawn(sp).unwrap();
        sess.transcript_path = Some(fork.to_string_lossy().into_owned());
        state.sessions.insert("ts-recover-0".into(), sess);
        let entry: crate::manifest::ManifestEntry = serde_json::from_value(json!({
            "uid": "ts-recover-0", "label": "r", "session_type": "claude-code",
            "transcript_id": "old-id", "transcript_path": old})).unwrap();
        let snap = Snapshot { entry, workspace_id: "ws".into(), worktree: dir.path().into(),
                              from_path: old.clone(), from_id: "old-id".into(), from_bytes: 0 };
        inflight_put(&state, "ts-recover-0", &snap).unwrap();
        let state = Arc::new(Mutex::new(state));
        assert_eq!(recover_inflight(&state), 0, "nothing resumed: the fork is live");
        let deadline = Instant::now() + Duration::from_secs(10);
        while status("ts-recover-0").is_none_or(|r| r["finished_at"].is_null()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let r = status("ts-recover-0").expect("recorded");
        assert_eq!(r["phase"], "done", "{r}");
        let archive = std::fs::read_to_string(dir.path().join("daemon-resume-identities.json")).unwrap();
        assert!(archive.contains("\"old-id\": \"new-id\""), "{archive}");
        assert!(!dir.path().join("rotations-inflight.json").exists(), "the marker is cleared once done");
        for s in state.lock().unwrap().sessions.values_mut() {
            let _ = s.kill();
        }
    }

    /// An idle claude-code session (a sleeping process) with a transcript.
    fn idle_session(uid: &str, idle_secs: u64) -> crate::session::DaemonSession {
        let mut sp = crate::session::SpawnParams::new(uid, "r", "/bin/sleep");
        sp.args = vec!["30".into()];
        sp.session_type = "claude-code".into();
        let mut s = crate::session::DaemonSession::spawn(sp).unwrap();
        s.transcript_path = Some("/tmp/never-read.jsonl".into());
        let ago = |secs: u64| Instant::now().checked_sub(Duration::from_secs(secs));
        *s.last_input_at.lock().unwrap() = ago(idle_secs + 100);
        *s.last_activity_at.lock().unwrap() = ago(idle_secs);
        *s.last_turn_end_at.lock().unwrap() = ago(idle_secs);
        s
    }

    /// #24 (urgent): at a turn end, queued wakes and stale receipts do not
    /// block (they are held for the new process); a wake being submitted
    /// right now does. An exited background job does not count.
    #[test]
    fn turn_end_checks_ignore_queued_wakes_and_exited_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = DaemonState::new();
        st.messaging_root = dir.path().to_path_buf();
        st.sessions.insert("ts-te-0".into(), idle_session("ts-te-0", 3));
        let at_end = Checks { idle: Duration::ZERO, owner_quiet: Duration::ZERO, draft: false, at_turn_end: true, crons_block: false };
        let polled = Checks { idle: Duration::ZERO, owner_quiet: Duration::ZERO, draft: false, at_turn_end: false, crons_block: false };
        let engines = ["claude-code"];
        assert_eq!(boundary_checked(&st, "ts-te-0", &engines, at_end), Ok(()));
        crate::notifications::publish(dir.path(), "ts-te-0", "chat-1", "chat", "[m1] hi", "[m1]").unwrap();
        assert!(boundary_checked(&st, "ts-te-0", &engines, polled).unwrap_err().contains("not yet delivered"));
        assert_eq!(boundary_checked(&st, "ts-te-0", &engines, at_end), Ok(()), "queued: held, not blocking");
        // A stale receipt (submitted long ago, never observed) does not block.
        let file = crate::notifications::directory(dir.path(), "ts-te-0");
        let set_status = |status: &str, updated_at: f64| {
            for e in std::fs::read_dir(&file).unwrap().flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "json") && !p.ends_with("transport.json") {
                    let mut v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
                    v["status"] = json!(status);
                    v["updated_at"] = json!(updated_at);
                    std::fs::write(&p, v.to_string()).unwrap();
                }
            }
        };
        set_status("uncertain", now() - 3600.0);
        assert_eq!(boundary_checked(&st, "ts-te-0", &engines, at_end), Ok(()));
        assert_eq!(boundary_checked(&st, "ts-te-0", &engines, polled), Ok(()), "a stale receipt never blocks");
        set_status("submitted", now());
        assert!(boundary_checked(&st, "ts-te-0", &engines, at_end).unwrap_err().contains("being delivered"));
        // Accepted by the old process minutes ago and still not seen in its
        // transcript: a kill could lose it, so it still blocks.
        set_status("uncertain", now() - 120.0);
        assert!(boundary_checked(&st, "ts-te-0", &engines, at_end).unwrap_err().contains("being delivered"));
        set_status("observed", now());
        // A job whose process is gone has exited; a live one still blocks.
        let job = |pid: u32| crate::agent_state::Job {
            id: "bd3axlkpv".into(), kind: crate::agent_state::JobKind::Shell, label: "b".into(), pid: Some(pid), cpu: None, first_seen_at: 1.0, wakes_agent: true,
        };
        st.sessions["ts-te-0"].agent_state.lock().unwrap().inputs.background.jobs.push(job(u32::MAX - 7));
        assert_eq!(boundary_checked(&st, "ts-te-0", &engines, at_end), Ok(()), "exited job ignored");
        st.sessions["ts-te-0"].agent_state.lock().unwrap().inputs.background.jobs.push(job(std::process::id()));
        assert!(boundary_checked(&st, "ts-te-0", &engines, at_end).unwrap_err().contains("background job"));
        for s in st.sessions.values_mut() {
            let _ = s.kill();
        }
    }

    /// #24 (11:47Z SEJD): a wake recorded in the fork before the
    /// after-notice still proves the fork took.
    #[test]
    fn a_fork_that_recorded_any_turn_counts() {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join("c.jsonl");
        std::fs::write(&claude, "{\"type\":\"user\",\"parentUuid\":null,\"message\":{\"content\":\"chat wake\"}}\n").unwrap();
        assert!(fork_took(&claude, "[cm-rotation abc]", true, 0), "a Claude fork's file means a turn was recorded");
        let codex = dir.path().join("x.jsonl");
        std::fs::write(&codex, "{\"type\":\"response_item\",\"payload\":{\"role\":\"user\"}}\n").unwrap();
        let copied = std::fs::metadata(&codex).unwrap().len();
        assert!(!fork_took(&codex, "[cm-rotation abc]", false, copied), "copied history alone is not a new turn");
        use std::io::Write;
        writeln!(std::fs::OpenOptions::new().append(true).open(&codex).unwrap(),
                 "{{\"type\":\"response_item\",\"payload\":{{\"role\":\"user\",\"content\":\"chat wake\"}}}}").unwrap();
        assert!(fork_took(&codex, "[cm-rotation abc]", false, copied));
    }

    /// A rotation whose fork is still over the threshold is not repeated.
    #[test]
    fn a_rotation_that_did_not_shrink_is_not_repeated() {
        let _serial = test_registry_lock();
        let mut r = test_rotation_record("old-x");
        r.phase = "done";
        r.to_id = Some("new-x".into());
        r.to_bytes = Some(500_000);
        r.finished_at = Some(now());
        registry().lock().unwrap().insert("ts-shrink-0".into(), r);
        assert!(rotation_did_not_shrink("ts-shrink-0", Path::new("/p/new-x.jsonl"), 300_000));
        assert!(!rotation_did_not_shrink("ts-shrink-0", Path::new("/p/later-y.jsonl"), 300_000), "a newer transcript may rotate");
        assert!(!rotation_did_not_shrink("ts-shrink-0", Path::new("/p/new-x.jsonl"), 800_000), "it did get under");
        registry().lock().unwrap().remove("ts-shrink-0");
    }

    /// #24 review: each feature lifts only its own hold, a running
    /// rotation's or handoff's hold is never replaced by a due one, and a due
    /// episode's hold is capped.
    #[test]
    fn holds_are_owned_and_a_due_hold_is_capped() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = DaemonState::new();
        st.messaging_root = dir.path().to_path_buf();
        let held = || crate::notifications::held(dir.path(), "ts-own-0", now());
        assert!(hold(&st, "ts-own-0", HoldOwner::Handoff) && held());
        assert!(hold(&st, "ts-own-0", HoldOwner::Due), "already held (by the handoff)");
        assert_eq!(hold_owner("ts-own-0"), Some(HoldOwner::Handoff), "a due hold never replaces it");
        unhold("ts-own-0", HoldOwner::Due);
        unhold("ts-own-0", HoldOwner::Rotation);
        assert!(held(), "only the handoff lifts its hold");
        unhold("ts-own-0", HoldOwner::Handoff);
        assert!(!held() && hold_owner("ts-own-0").is_none());
        // A due hold, upgraded by the rotation that starts, lifted by it.
        assert!(hold(&st, "ts-own-0", HoldOwner::Due));
        assert!(hold(&st, "ts-own-0", HoldOwner::Rotation));
        unhold("ts-own-0", HoldOwner::Due);
        assert!(held(), "a turn-end attempt never lifts a running rotation's hold");
        unhold("ts-own-0", HoldOwner::Rotation);
        assert!(!held());
        // A due episode that has held 10 minutes is not renewed.
        holds().lock().unwrap().insert("ts-own-0".into(), Hold {
            root: dir.path().to_path_buf(), owner: HoldOwner::Due, episode_start: now() - DUE_HOLD_CAP_SECS - 1.0,
        });
        assert!(!hold(&st, "ts-own-0", HoldOwner::Due), "spent: wakes flow");
        assert!(!held());
        unhold("ts-own-0", HoldOwner::Due);
        assert_eq!(hold_owner("ts-own-0"), Some(HoldOwner::Due), "the spent episode is remembered");
        // A fresh due hold expires within the cap, not 30 minutes.
        holds().lock().unwrap().remove("ts-own-0");
        assert!(hold(&st, "ts-own-0", HoldOwner::Due));
        assert!(!crate::notifications::held(dir.path(), "ts-own-0", now() + DUE_HOLD_CAP_SECS + 1.0));
        // An exit ends the episode.
        on_session_exit("ts-own-0");
        assert!(!held() && hold_owner("ts-own-0").is_none());
        // After a restart nothing in memory owns a leftover hold: it is lifted.
        crate::notifications::set_hold(dir.path(), "ts-own-1", now() + 600.0).unwrap();
        let _ = messaging_root().set(dir.path().to_path_buf());
        if messaging_root().get() == Some(&dir.path().to_path_buf()) {
            unhold("ts-own-1", HoldOwner::Rotation);
            assert!(!crate::notifications::held(dir.path(), "ts-own-1", now()));
        }
        crate::notifications::set_hold(dir.path(), "ts-own-2", now() + 600.0).unwrap();
        assert!(crate::notifications::clear_all_holds(dir.path()) >= 1, "brain start clears leftovers");
        assert!(!crate::notifications::held(dir.path(), "ts-own-2", now()));
        assert!(!crate::notifications::held(dir.path(), "ts-own-1", now()));
    }

    /// A forced request while a rotation of that session runs is refused,
    /// not armed as a second rotation.
    #[test]
    fn a_force_during_a_running_rotation_is_refused() {
        let _serial = test_registry_lock();
        let state = Arc::new(Mutex::new(DaemonState::new()));
        test_set_running("ts-dup-0", true);
        let r = start(&state, "ts-dup-0", "again", Origin::Operator, true);
        test_set_running("ts-dup-0", false);
        let why = r.unwrap_err().1;
        assert!(why.starts_with(BUSY) && why.contains("starting the fork") && why.contains("comes straight back"), "{why}");
        assert!(!armed("ts-dup-0"));
    }

    /// A rotation that leaves the session down tells viewers it is not
    /// coming back (they would otherwise wait out the respawn gap).
    #[test]
    fn a_failure_that_leaves_it_down_announces_it() {
        let state = Arc::new(Mutex::new(DaemonState::new()));
        let (rx, _guard) = state.lock().unwrap().manifest_watcher.subscribe();
        fail(&state, "ts-down-0", "resume failed".into());
        match rx.try_recv().expect("an Exited diff") {
            crate::manifest::ManifestDiff::Exited { uid, last_exit } => {
                assert_eq!(uid, "ts-down-0");
                assert_eq!(last_exit.killed_by.as_deref(), Some(KILLED_BY_FAILED));
            }
            other => panic!("unexpected {other:?}"),
        }
        failures().lock().unwrap().remove("ts-down-0");
    }

    /// Arms survive a brain restart.
    #[test]
    fn arms_persist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rotation-arms.json");
        std::fs::write(&path, r#"{"ts-arm-p":{"reason":"agent asked","origin":"agent","armed_at":1e18}}"#).unwrap();
        load_arms(&path);
        assert!(armed("ts-arm-p"));
        assert_eq!(arms().lock().unwrap()["ts-arm-p"].origin, Origin::Agent);
        if arms_path().get() == Some(&path) {
            arm_remove("ts-arm-p");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
        } else {
            arm_remove("ts-arm-p");
        }
    }

    #[test]
    fn refusals_are_classified() {
        assert!(permanent_refusal("not at a safe boundary: workflow participant (its run drives it)"));
        assert!(permanent_refusal("bash sessions cannot do this (only claude-code)"));
        assert!(!permanent_refusal("not at a safe boundary: not idle (working)"));
        assert!(hold_worthy("not idle (working)") && hold_worthy("1 notification(s) being delivered (not yet confirmed)"));
        assert!(!hold_worthy("Owner typed in it 3s ago") && !hold_worthy("1 scheduled wakeup(s) pending"));
        // These can last for hours: never worth holding wakes for.
        assert!(!hold_worthy("not idle (working-background)") && !hold_worthy("2 background job(s) running"));
        assert!(!hold_worthy("not idle (waiting-on-human)"));
    }

    /// A forced request that meets a turn in progress is armed for the next
    /// turn end (and holds new wakes) instead of being refused.
    #[test]
    fn a_forced_rotation_mid_turn_is_armed_and_holds_wakes() {
        let _serial = test_registry_lock();
        let dir = tempfile::tempdir().unwrap();
        let mut st = DaemonState::new();
        st.messaging_root = dir.path().to_path_buf();
        let mut sp = crate::session::SpawnParams::new("ts-arm-0", "r", "/bin/sleep");
        sp.args = vec!["30".into()];
        sp.session_type = "claude-code".into();
        let mut busy = crate::session::DaemonSession::spawn(sp).unwrap();
        busy.transcript_path = Some("/tmp/never-read.jsonl".into());
        st.sessions.insert("ts-arm-0".into(), busy);
        let state = Arc::new(Mutex::new(st));
        let v = start(&state, "ts-arm-0", "Owner asked", Origin::Operator, true).expect("armed, not refused");
        assert_eq!(v["armed"], true, "{v}");
        assert!(armed("ts-arm-0"));
        assert!(crate::notifications::held(dir.path(), "ts-arm-0", now()), "wakes held for the new process");
        assert!(start(&state, "ts-arm-0", "x", Origin::Operator, false).is_err(), "unforced still refuses");
        // A permanent refusal is not armed.
        state.lock().unwrap().sessions.get_mut("ts-arm-0").unwrap().workflow_run_id = Some("wf".into());
        assert!(start(&state, "ts-arm-0", "x", Origin::Operator, true).is_err());
        arm_remove("ts-arm-0");
        unhold("ts-arm-0", HoldOwner::Due);
        assert!(!crate::notifications::held(dir.path(), "ts-arm-0", now()));
        for s in state.lock().unwrap().sessions.values_mut() {
            let _ = s.kill();
        }
    }

    #[test]
    fn failure_counts_persist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rotation-failures.json");
        std::fs::write(&path, r#"{"ts-persist-f":{"count":3,"at":1e18}}"#).unwrap();
        load_failures(&path);
        assert!(recently_failed("ts-persist-f"), "a deploy keeps the backoff");
        assert_eq!(failures().lock().unwrap()["ts-persist-f"].0, 3);
    }

    #[test]
    fn failures_count_until_a_success() {
        finish("ts-fail-x", "failed", "a".into());
        finish("ts-fail-x", "failed", "b".into());
        assert_eq!(failures().lock().unwrap().get("ts-fail-x").map(|e| e.0), Some(2));
        assert!(recently_failed("ts-fail-x"));
        finish("ts-fail-x", "done", "ok".into());
        assert!(!recently_failed("ts-fail-x"));
    }

    #[test]
    fn an_interrupted_rotation_marker_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = DaemonState::new();
        state.daemon_sessions_path = Some(dir.path().join("daemon-sessions.json"));
        let entry: crate::manifest::ManifestEntry =
            serde_json::from_value(json!({"uid": "ts-m-0", "label": "m", "session_type": "claude-code",
                                          "transcript_id": "old"})).unwrap();
        let snap = Snapshot {
            entry,
            workspace_id: "ws".into(),
            worktree: dir.path().to_path_buf(),
            from_path: dir.path().join("old.jsonl"),
            from_id: "old".into(),
            from_bytes: 1,
        };
        inflight_put(&state, "ts-m-0", &snap).unwrap();
        let all = inflight_read(&inflight_path(&state).unwrap());
        assert_eq!(all["ts-m-0"]["entry"]["transcript_id"], "old");
        inflight_remove(&state, "ts-m-0");
        assert!(!inflight_path(&state).unwrap().exists(), "the last marker removes the file");
    }

    #[test]
    fn compactions_are_counted_incrementally() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        std::fs::write(&path, "{\"type\":\"user\"}\n{\"type\":\"system\",\"subtype\":\"compact_boundary\"}\n").unwrap();
        assert_eq!(compactions(&path), 1);
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        write!(f, "{{\"subtype\":\"compact_boundary\"}}\n{{\"subtype\":\"compact_bou").unwrap();
        assert_eq!(compactions(&path), 2, "the partial line waits");
        writeln!(f, "ndary\"}}").unwrap();
        assert_eq!(compactions(&path), 3);
        std::fs::write(&path, "{\"subtype\":\"compact_boundary\"}\n").unwrap();
        assert_eq!(compactions(&path), 1, "a rewritten (shorter) file is recounted");
    }

    #[test]
    fn a_fork_counts_as_written_only_with_message_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.jsonl");
        assert!(fork_written(&path).is_none());
        std::fs::write(&path, "{\"type\":\"ai-title\"}\n").unwrap();
        assert!(fork_written(&path).is_none(), "metadata alone is not the conversation");
        std::fs::write(&path, "{\"type\":\"user\",\"parentUuid\":null}\n").unwrap();
        assert!(fork_written(&path).is_some());
    }

    #[test]
    fn notices_carry_what_the_agent_must_do() {
        let after = after_notice(1_670_000_000);
        assert!(after.contains("1670 MB") && after.contains("board items") && after.contains("standing rules"));
        assert_eq!(mb(323_505), "323 KB");
        assert_eq!(mb(9_121_310), "9.1 MB");
        assert!(before_notice("transcript is 400 MB").contains("next turn ends"));
    }

    /// EP's fork (2026-11-07): metadata and four 34 KB file-history
    /// snapshots put its first message past 256 KB, so the old head check
    /// never saw it and a working fork was rolled back.
    #[test]
    fn a_fork_whose_first_message_is_past_256_kb_counts() {
        let dir = tempfile::tempdir().unwrap();
        let fork = dir.path().join("fork.jsonl");
        let mut text = String::new();
        for t in ["custom-title", "ai-title", "agent-name", "mode", "permission-mode"] {
            text += &format!("{}\n", json!({"type": t, "sessionId": "fork"}));
        }
        let blob = "x".repeat(60_000);
        for i in 0..5 {
            // Backups of other transcripts may mention user rows in content.
            text += &format!("{}\n", json!({"type": "file-history-snapshot", "messageId": i,
                "snapshot": {"backup": format!("{{\"type\":\"user\",\"parentUuid\":null}} {blob}")}}));
        }
        std::fs::write(&fork, &text).unwrap();
        assert!(text.len() > 256 * 1024);
        assert_eq!(fork_written(&fork), None, "metadata and snapshots alone are not a turn");
        let row = json!({"type": "user", "parentUuid": "p", "message": {"role": "user", "content": "rotation done"}});
        // A row still being written (no newline yet) does not count yet.
        std::fs::write(&fork, format!("{text}{row}")).unwrap();
        assert_eq!(fork_written(&fork), None);
        std::fs::write(&fork, format!("{text}{row}\n")).unwrap();
        assert_eq!(fork_written(&fork), Some(std::fs::metadata(&fork).unwrap().len()));
        assert!(fork_took(&fork, "[cm-rotation zz]", true, 0));
        let at = first_message_after(&fork, 0).unwrap();
        assert_eq!(first_message_after(&fork, at), None, "nothing new after the last row");
    }

    /// A report naming the transcript a rotation is moving the session off
    /// is ignored while its fork starts or is verified (#114).
    #[test]
    fn a_rotating_session_is_never_restamped_to_its_source() {
        let _serial = test_registry_lock();
        let old = "/p/650fb958-1e42-4428-b4aa-33e4f7061ba5.jsonl";
        let fork = "/p/9f6089a3-30f8-40ab-8194-05333e583b84.jsonl";
        for phase in ["forking", "notifying", "verifying"] {
            test_set_rotating("ts-restamp-0", "650fb958-1e42-4428-b4aa-33e4f7061ba5", phase);
            assert!(refuses_restamp("ts-restamp-0", old), "{phase}");
            assert!(!refuses_restamp("ts-restamp-0", fork), "{phase}: the fork itself is fine");
            assert!(!refuses_restamp("ts-other-0", old), "{phase}: only that session");
        }
        // The fallback resumes the original: that is not a re-stamp.
        test_set_rotating("ts-restamp-0", "650fb958-1e42-4428-b4aa-33e4f7061ba5", "falling back");
        assert!(!refuses_restamp("ts-restamp-0", old));
        test_set_rotating("ts-restamp-0", "650fb958-1e42-4428-b4aa-33e4f7061ba5", "verifying");
        test_finish("ts-restamp-0");
        assert!(!refuses_restamp("ts-restamp-0", old), "a finished rotation guards nothing");
        test_set_running("ts-restamp-0", false);
    }

    /// The verifier gave up while the fork it should have accepted was
    /// live and held Owner's turns (#114). A fork with turns is kept: the
    /// process is not stopped, the lineage is recorded, nothing is resumed.
    #[test]
    fn a_fork_with_turns_is_never_rolled_back() {
        let _serial = test_registry_lock();
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old-id.jsonl");
        let fork = dir.path().join("new-id.jsonl");
        std::fs::write(&old, format!("{}\n", json!({"type":"user","parentUuid":null}))).unwrap();
        std::fs::write(&fork, format!("{}\n{}\n",
            json!({"type":"file-history-snapshot","messageId":"m"}),
            json!({"type":"user","parentUuid":"p","message":{"role":"user","content":"rotation done"}}))).unwrap();
        let mut state = DaemonState::new();
        state.daemon_sessions_path = Some(dir.path().join("daemon-sessions.json"));
        let mut sess = idle_session("ts-keep-0", 0);
        sess.transcript_path = Some(fork.to_string_lossy().into_owned());
        let pid = sess.pid;
        state.sessions.insert("ts-keep-0".into(), sess);
        let entry: crate::manifest::ManifestEntry = serde_json::from_value(json!({
            "uid": "ts-keep-0", "label": "r", "session_type": "claude-code",
            "transcript_id": "old-id", "transcript_path": old})).unwrap();
        let snap = Snapshot { entry, workspace_id: "ws".into(), worktree: dir.path().into(),
                              from_path: old.clone(), from_id: "old-id".into(), from_bytes: 0 };
        let state = Arc::new(Mutex::new(state));
        test_set_rotating("ts-keep-0", "old-id", "verifying");
        resume_old(&state, "ts-keep-0", &snap, "the fork wrote no transcript in time", Some(&fork));
        let r = status("ts-keep-0").expect("recorded");
        assert_eq!(r["phase"], "done", "{r}");
        assert_eq!(r["to_transcript_id"], "new-id", "{r}");
        assert!(r["outcome"].as_str().unwrap().contains("kept the fork"), "{r}");
        {
            let st = state.lock().unwrap();
            let s = st.sessions.get("ts-keep-0").expect("the fork was not stopped");
            assert_eq!(s.pid, pid);
            assert_eq!(s.transcript_path.as_deref(), Some(fork.to_string_lossy().as_ref()));
        }
        let archive = std::fs::read_to_string(dir.path().join("daemon-resume-identities.json")).unwrap();
        assert!(archive.contains("\"old-id\": \"new-id\""), "{archive}");
        test_set_running("ts-keep-0", false);
        for s in state.lock().unwrap().sessions.values_mut() {
            let _ = s.kill();
        }
    }

    /// At the deadline, a fork with no turns is still kept when Owner typed
    /// in it or its file exists; only an empty, untouched fork is rolled back.
    #[test]
    fn an_unconfirmed_fork_is_kept_when_rolling_back_could_lose_anything() {
        let dir = tempfile::tempdir().unwrap();
        let fork = dir.path().join("new-id.jsonl");
        let mut state = DaemonState::new();
        state.sessions.insert("ts-unsure-0".into(), idle_session("ts-unsure-0", 0));
        let state = Arc::new(Mutex::new(state));
        let spawned = Instant::now() - Duration::from_secs(60);
        let none: Vec<PathBuf> = vec![fork.clone()];
        assert_eq!(keep_reason(&state, "ts-unsure-0", spawned, false, &none), None, "no file, no input: roll back");
        assert!(keep_reason(&state, "ts-unsure-0", spawned, true, &none).unwrap().contains("Owner typed"));
        *state.lock().unwrap().sessions["ts-unsure-0"].last_operator_input_at.lock().unwrap() =
            Some(Instant::now() - Duration::from_secs(5));
        assert!(keep_reason(&state, "ts-unsure-0", spawned, false, &none).unwrap().contains("Owner typed"));
        *state.lock().unwrap().sessions["ts-unsure-0"].last_operator_input_at.lock().unwrap() = None;
        std::fs::write(&fork, "{\"type\":\"mode\"}\n").unwrap();
        assert!(keep_reason(&state, "ts-unsure-0", spawned, false, &none).unwrap().contains("exists"));
        for s in state.lock().unwrap().sessions.values_mut() {
            let _ = s.kill();
        }
    }

    /// A fallback re-sends the restored binding, so a viewer told about the
    /// fork follows the session back (#114: the A-e picker still marked the
    /// rolled-back fork "this").
    #[test]
    fn a_fallback_rebroadcasts_the_restored_binding() {
        let mut state = DaemonState::new();
        let mut sess = idle_session("ts-rebind-0", 0);
        sess.transcript_path = Some("/p/650fb958-1e42-4428-b4aa-33e4f7061ba5.jsonl".into());
        state.sessions.insert("ts-rebind-0".into(), sess);
        let (rx, _guard) = state.manifest_watcher.subscribe();
        let state = Arc::new(Mutex::new(state));
        rebroadcast_binding(&state, "ts-rebind-0");
        match rx.recv_timeout(Duration::from_millis(500)).expect("a fallback must broadcast") {
            crate::manifest::ManifestDiff::Updated { uid, entry } => {
                assert_eq!(uid, "ts-rebind-0");
                assert_eq!(entry["transcript_id"], "650fb958-1e42-4428-b4aa-33e4f7061ba5");
            }
            other => panic!("expected Updated, got {other:?}"),
        }
        for s in state.lock().unwrap().sessions.values_mut() {
            let _ = s.kill();
        }
    }

    /// A restart request during a rotation names the phase and a bound.
    #[test]
    fn a_busy_message_names_phase_and_time_left() {
        let _serial = test_registry_lock();
        test_set_rotating("ts-busy-0", "old", "notifying");
        set_phase("ts-busy-0", "notifying", Some(Duration::from_secs(300)));
        let m = busy_message("ts-busy-0").unwrap();
        assert!(m.starts_with(BUSY) && m.contains("delivering the fork's first message"), "{m}");
        assert!(m.contains("left") && m.contains("comes straight back"), "{m}");
        test_finish("ts-busy-0");
        assert_eq!(busy_message("ts-busy-0"), None);
        test_set_running("ts-busy-0", false);
    }
}
