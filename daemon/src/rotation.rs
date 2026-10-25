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
    fn as_str(self) -> &'static str {
        match self {
            Origin::Operator => "operator",
            Origin::Agent => "agent",
            Origin::Policy => "policy",
        }
    }
}

/// One rotation's record, kept for `status` / `list_sessions`.
#[derive(Clone, Debug)]
struct Rotation {
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

/// Tests: mark `uid` as mid-rotation (`done` finishes it).
#[cfg(test)]
pub(crate) fn test_set_running(uid: &str, running: bool) {
    let mut reg = registry().lock().unwrap_or_else(|p| p.into_inner());
    if running {
        reg.insert(uid.into(), Rotation {
            phase: "forking", origin: Origin::Operator, reason: String::new(), started_at: now(),
            from_id: String::new(), from_bytes: 0, to_id: None, to_bytes: None, path: "fork",
            outcome: None, finished_at: None,
        });
    } else {
        reg.remove(uid);
    }
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

/// Why `uid` cannot rotate right now, if it cannot. The safe boundary:
/// a live Claude session with a transcript, idle for `idle` with nothing in
/// flight (no open turn, no background jobs), an empty native notification
/// queue (the rotation's own notice aside), Owner not viewing or typing in
/// it for `owner_quiet`, not a
/// workflow participant, not a continuous tick mid-run, and not already
/// rotating.
pub fn boundary(state: &DaemonState, uid: &str, idle: Duration, owner_quiet: Duration) -> Result<(), String> {
    let sess = state.sessions.get(uid).ok_or_else(|| format!("session '{uid}' is not live on this host"))?;
    if sess.session_type != "claude-code" {
        return Err(format!("only Claude sessions rotate (this is {})", sess.session_type));
    }
    if sess.transcript_path.is_none() {
        return Err("the session has no bound transcript".into());
    }
    if in_progress(uid) {
        return Err("a rotation is already running".into());
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
    if !agent.background.jobs.is_empty() {
        return Err(format!("{} background job(s) running", agent.background.jobs.len()));
    }
    // A scheduled wakeup lives in the process: a restart would lose it (CM
    // does not carry session crons across a fork).
    if !agent.background.crons.is_empty() {
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
        // an unsent draft may sit in its composer. An operator forcing it
        // (no quiet window) has decided.
        let typed_unix = now() - at.elapsed().as_secs_f64();
        if !owner_quiet.is_zero() && typed_unix > agent.since {
            return Err("Owner typed in it since its last turn (a draft may be unsent)".into());
        }
    }
    let pending = crate::notifications::snapshot(&state.messaging_root, uid)
        .map(|(events, _)| {
            events
                .iter()
                .filter(|e| matches!(e["status"].as_str(), Some("pending" | "submitted" | "uncertain")))
                // The rotation's own before-notice never holds it back (a
                // stuck consumer would otherwise block rotation forever).
                .filter(|e| e["source"].as_str() != Some("rotation"))
                .count()
        })
        .unwrap_or(0);
    if pending > 0 {
        return Err(format!("{pending} notification(s) not yet delivered"));
    }
    Ok(())
}

/// What a rotation needs from the live session, captured under the lock.
struct Snapshot {
    entry: crate::manifest::ManifestEntry,
    workspace_id: String,
    worktree: PathBuf,
    from_path: PathBuf,
    from_id: String,
    from_bytes: u64,
}

fn snapshot(state: &DaemonState, uid: &str) -> Result<Snapshot, String> {
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
    let (snap, rotation) = {
        let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        let (idle, quiet) = {
            let c = &state.config.transcript_rotation;
            (Duration::from_secs(c.idle_secs), Duration::from_secs(c.owner_quiet_secs))
        };
        // `force` (an operator's explicit request) waives only the idle and
        // Owner-quiet waits: a tool call, background job, undelivered notice,
        // workflow transition or continuous run still refuses.
        let (idle, quiet) = if force { (Duration::ZERO, Duration::ZERO) } else { (idle, quiet) };
        boundary(&state, uid, idle, quiet).map_err(|why| (ErrorCode::Conflict, format!("not at a safe boundary: {why}")))?;
        // One rotation at a time on a host, with a pause between them (a
        // fork reads its whole source; a deploy may find many big sessions).
        if let Some(other) = running_any() {
            return Err((ErrorCode::Conflict, format!("another rotation is running ({other}); one at a time per host")));
        }
        if origin != Origin::Operator {
            if let Some(left) = pause_left() {
                return Err((ErrorCode::Conflict, format!("pausing {}s after the last rotation", left.as_secs())));
            }
        }
        let snap = snapshot(&state, uid).map_err(|e| (ErrorCode::Conflict, e))?;
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
        };
        (snap, rotation)
    };
    let record = rotation.to_json();
    registry().lock().unwrap_or_else(|p| p.into_inner()).insert(uid.to_string(), rotation);
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
    Duration::from_secs((source_bytes / 1_000_000_000).saturating_mul(60).saturating_add(180).min(1200))
}

/// The pause between two rotations on a host (not for an operator's own).
const PAUSE_BETWEEN: Duration = Duration::from_secs(120);

fn wait_gone(state_arc: &Arc<Mutex<DaemonState>>, uid: &str) -> bool {
    let deadline = Instant::now() + STOP_DEADLINE;
    while Instant::now() < deadline {
        if !state_arc.lock().unwrap_or_else(|p| p.into_inner()).sessions.contains_key(uid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

/// A fork's transcript is in place: the file exists and holds message rows.
fn fork_written(path: &Path) -> Option<u64> {
    let len = std::fs::metadata(path).ok()?.len();
    let mut head = vec![0u8; len.min(256 * 1024) as usize];
    use std::io::Read;
    std::fs::File::open(path).ok()?.read_exact(&mut head).ok()?;
    let text = String::from_utf8_lossy(&head);
    (text.contains("\"parentUuid\":") && (text.contains("\"type\":\"user\"") || text.contains("\"type\":\"assistant\"")))
        .then_some(len)
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
    format!(
        "[cm-rotation] CM moved this conversation to a fresh transcript: the old one was {}, \
         and Claude Code slows down badly past a few hundred MB. You continue from your latest \
         compaction summary. Nothing else changed: same CM session, chat name, task and workspace. \
         Before you continue, re-read anything handoff-critical from CM rather than assuming it: \
         your board items, standing rules and Owner rulings, open monitors, and your chat inbox. \
         Then carry on with what you were doing.",
        mb(from_bytes)
    )
}

fn fallback_notice(why: &str) -> String {
    format!(
        "[cm-rotation] CM tried to move this conversation to a fresh transcript, but it did not \
         take ({why}). You are back on your original transcript; nothing was lost. Carry on."
    )
}

fn deliver(state_arc: &Arc<Mutex<DaemonState>>, uid: &str, text: String) -> bool {
    let io = {
        let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        state.sessions.get(uid).map(|s| (s.input_handle(), s.fanout.clone()))
    };
    let Some((handle, fanout)) = io else { return false };
    crate::control::methods::deliver_agent_prompt(handle, fanout, uid.to_string(), text, true, Arc::clone(state_arc));
    true
}

fn finish(uid: &str, phase: &'static str, outcome: String) {
    eprintln!("cm-daemon: rotation of {uid}: {outcome}");
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
    } else {
        f.remove(uid);
    }
}

/// A failed rotation: recorded, and Owner is alerted from the second
/// consecutive failure on (the policy backs off exponentially meanwhile).
fn fail(state_arc: &Arc<Mutex<DaemonState>>, uid: &str, outcome: String) {
    finish(uid, "failed", outcome.clone());
    let count = failures().lock().unwrap_or_else(|p| p.into_inner()).get(uid).map_or(0, |e| e.0);
    if count >= 2 {
        let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        let message = format!(
            "Transcript rotation of this session failed {count} times in a row (latest: {outcome}). \
             CM backs off and retries later; Alt+T retries now."
        );
        if let Err(e) = crate::owner_attention::raise_system(&state, uid, &message) {
            eprintln!("cm-daemon: rotation alert for {uid}: {e}");
        }
    }
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

/// The session being rotated now, if any (one at a time per host).
fn running_any() -> Option<String> {
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

fn inflight_put(state: &DaemonState, uid: &str, snap: &Snapshot) -> std::io::Result<()> {
    let Some(path) = inflight_path(state) else { return Ok(()) };
    let mut all = inflight_read(&path);
    all.insert(
        uid.to_string(),
        json!({
            "workspace_id": snap.workspace_id,
            "worktree": snap.worktree,
            "from_id": snap.from_id,
            "entry": snap.entry,
            "at": now(),
        }),
    );
    crate::state::write_json_atomic(&path, &Value::Object(all).to_string(), true)
}

fn inflight_remove(state: &DaemonState, uid: &str) {
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
        let Some(path) = inflight_path(&state) else { return 0 };
        let all = inflight_read(&path);
        (path, all)
    };
    let mut resumed = 0;
    for (uid, v) in &all {
        if state_arc.lock().unwrap_or_else(|p| p.into_inner()).sessions.contains_key(uid) {
            eprintln!("cm-daemon: rotation of {uid} was interrupted by a restart; the session is live, leaving it");
            continue;
        }
        let entry: Option<crate::manifest::ManifestEntry> = serde_json::from_value(v["entry"].clone()).ok();
        let (Some(entry), Some(ws), Some(wt)) = (entry, v["workspace_id"].as_str(), v["worktree"].as_str()) else {
            eprintln!("cm-daemon: rotation marker for {uid} is unreadable; dropped");
            continue;
        };
        match crate::control::methods::restore_one_session_launch(state_arc, ws, Path::new(wt), &entry, None, false) {
            Ok(_) => {
                resumed += 1;
                deliver(state_arc, uid, fallback_notice("a daemon restart interrupted it"));
                eprintln!("cm-daemon: rotation of {uid} was interrupted by a restart; resumed its original transcript");
            }
            Err((_, e)) => eprintln!("cm-daemon: rotation of {uid} was interrupted; resuming it failed: {e}"),
        }
    }
    if !all.is_empty() {
        let _ = std::fs::remove_file(&path);
    }
    resumed
}

fn run(state_arc: &Arc<Mutex<DaemonState>>, uid: &str, snap: Snapshot) {
    // 1. Stop the process. Viewers and monitors see killed_by
    //    `transcript-rotation`: the uid comes straight back.
    crate::control::methods::kill_with_attribution(state_arc, uid, ROTATION_KILLER);
    if !wait_gone(state_arc, uid) {
        fail(state_arc, uid, "the session did not stop; left as it was".into());
        return;
    }
    // 2. Fork at the same uid.
    update(uid, |r| r.phase = "forking");
    if let Err((_, e)) = crate::control::methods::restore_one_session_launch(
        state_arc,
        &snap.workspace_id,
        &snap.worktree,
        &snap.entry,
        None,
        true,
    ) {
        resume_old(state_arc, uid, &snap, &format!("the fork did not start: {e}"));
        return;
    }
    let to_path = state_arc
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .sessions
        .get(uid)
        .and_then(|s| s.transcript_path.clone())
        .map(PathBuf::from);
    let Some(to_path) = to_path.filter(|p| p != &snap.from_path) else {
        resume_old(state_arc, uid, &snap, "the fork has no new transcript");
        return;
    };
    let to_id = crate::session::transcript_id_from_path(&to_path.to_string_lossy());
    update(uid, |r| {
        r.phase = "notifying";
        r.to_id = to_id.clone();
    });
    // 3. The first message writes the fork's file.
    if !deliver(state_arc, uid, after_notice(snap.from_bytes)) {
        resume_old(state_arc, uid, &snap, "the forked session vanished before its first message");
        return;
    }
    update(uid, |r| r.phase = "verifying");
    let deadline = Instant::now() + verify_deadline(snap.from_bytes);
    while Instant::now() < deadline {
        if let Some(bytes) = fork_written(&to_path) {
            update(uid, |r| r.to_bytes = Some(bytes));
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
                    "rotated {} ({}) to {} ({})",
                    snap.from_id,
                    mb(snap.from_bytes),
                    to_id.as_deref().unwrap_or("?"),
                    mb(bytes)
                ),
            );
            return;
        }
        if !state_arc.lock().unwrap_or_else(|p| p.into_inner()).sessions.contains_key(uid) {
            resume_old(state_arc, uid, &snap, "the forked session exited before writing its transcript");
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    resume_old(state_arc, uid, &snap, "the fork wrote no transcript (API error or a menu?)");
}

/// The fork did not take: stop whatever runs at the uid and resume the old
/// transcript, as it was.
fn resume_old(state_arc: &Arc<Mutex<DaemonState>>, uid: &str, snap: &Snapshot, why: &str) {
    update(uid, |r| r.phase = "falling back");
    let live = state_arc.lock().unwrap_or_else(|p| p.into_inner()).sessions.contains_key(uid);
    if live {
        crate::control::methods::kill_with_attribution(state_arc, uid, ROTATION_KILLER);
        if !wait_gone(state_arc, uid) {
            fail(state_arc, uid, format!("{why}; the forked process did not stop either"));
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
            deliver(state_arc, uid, fallback_notice(why));
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

fn before_notice(why: &str, quiet_secs: u64) -> String {
    format!(
        "[cm-rotation] Heads-up: your {why}. Claude Code slows down badly on transcripts this \
         large, so CM will move this conversation to a fresh transcript at your next idle point \
         ({} min with nothing running). You will continue from your latest compaction summary \
         at the same CM session, name, task and workspace. Keep anything a successor would need \
         in files, board items or notes, as usual; nothing to do otherwise.",
        (quiet_secs + 59) / 60
    )
}

/// Agents' own rotation requests (uid → reason), carried out by the policy
/// at the session's next safe boundary (an agent asks mid-turn, so it is
/// never at one when it asks).
fn requests() -> &'static Mutex<HashMap<String, String>> {
    static R: std::sync::OnceLock<Mutex<HashMap<String, String>>> = std::sync::OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Queue `uid`'s own request. Returns what it will wait for.
pub fn request(state: &DaemonState, uid: &str, reason: &str) -> Result<Value, (ErrorCode, String)> {
    match state.sessions.get(uid) {
        Some(s) if s.session_type == "claude-code" && s.transcript_path.is_some() => {}
        Some(s) if s.session_type != "claude-code" => {
            return Err((ErrorCode::InvalidParams, format!("only Claude sessions rotate (this is {})", s.session_type)))
        }
        Some(_) => return Err((ErrorCode::Conflict, "the session has no bound transcript yet".into())),
        None => return Err((ErrorCode::NotFound, format!("session '{uid}' is not live on this host"))),
    }
    requests().lock().unwrap_or_else(|p| p.into_inner()).insert(uid.to_string(), reason.to_string());
    let c = &state.config.transcript_rotation;
    Ok(json!({
        "ok": true,
        "queued": true,
        "note": format!(
            "CM will move this conversation to a fresh transcript once you have been idle {} min \
             with nothing running (checked every {} min). End your turn normally.",
            (c.idle_secs + 59) / 60,
            (c.check_secs + 59) / 60,
        ),
    }))
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
            .filter_map(|(uid, s)| Some((uid.clone(), PathBuf::from(s.transcript_path.clone()?))))
            .collect();
        (cfg, candidates)
    };
    // Agents' own requests first: no threshold, no before-notice (they
    // asked), only the boundary.
    let asked: Vec<(String, String)> =
        requests().lock().unwrap_or_else(|p| p.into_inner()).iter().map(|(u, r)| (u.clone(), r.clone())).collect();
    for (uid, reason) in asked {
        match start(state_arc, &uid, &format!("requested: {reason}"), Origin::Agent, false) {
            Ok(_) => {
                requests().lock().unwrap_or_else(|p| p.into_inner()).remove(&uid);
            }
            Err((ErrorCode::NotFound | ErrorCode::InvalidParams, _)) => {
                requests().lock().unwrap_or_else(|p| p.into_inner()).remove(&uid);
            }
            Err((_, refusal)) => eprintln!("cm-daemon: requested rotation of {uid} deferred: {refusal}"),
        }
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
        if requests().lock().unwrap_or_else(|p| p.into_inner()).contains_key(&uid) {
            continue;
        }
        let why = over_threshold(&cfg, bytes, counted.get(&path).copied().unwrap_or(0));
        let Some(why) = why else { continue };
        if recently_failed(&uid) {
            continue;
        }
        let id = crate::session::transcript_id_from_path(&path.to_string_lossy()).unwrap_or_default();
        let first_notice = {
            let mut n = noticed().lock().unwrap_or_else(|p| p.into_inner());
            n.get(&uid) != Some(&id) && {
                n.insert(uid.clone(), id.clone());
                true
            }
        };
        if first_notice {
            let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
            let marker = format!("[cm-rotation-notice {id}]");
            let text = format!("{marker} {}", before_notice(&why, cfg.idle_secs));
            if let Err(e) = crate::notifications::publish(
                &state.messaging_root,
                &uid,
                &format!("rotation-notice:{id}"),
                "rotation",
                &text,
                &marker,
            ) {
                eprintln!("cm-daemon: rotation notice for {uid}: {e}");
            }
            continue; // rotate on a later pass, after the notice settles
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
        assert_eq!(verify_deadline(9_000_000), Duration::from_secs(180));
        assert_eq!(verify_deadline(1_700_000_000), Duration::from_secs(240));
        assert_eq!(verify_deadline(u64::MAX), Duration::from_secs(1200));
        assert_eq!(backoff_secs(1), 6.0 * 3600.0);
        assert_eq!(backoff_secs(3), 24.0 * 3600.0);
        assert_eq!(backoff_secs(30), 7.0 * 86400.0);
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
        assert!(before_notice("transcript is 400 MB", 300).contains("5 min"));
    }
}
