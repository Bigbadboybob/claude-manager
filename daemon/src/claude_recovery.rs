//! Explicit, operator-confirmed recovery for Claude native background work.
//!
//! The daemon owns all host-local evidence.  Read-only Claude roster and
//! `/proc` probes run outside the global daemon mutex; an effect is admitted
//! only after a durable identity capture, a second identical probe, and a
//! durable checkpoint immediately before each external effect.  Unknown or
//! corrupt evidence is never treated as absence.

use crate::control::protocol::ErrorCode;
use crate::state::DaemonState;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::HashSet;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_COMMAND_OUTPUT: u64 = 1024 * 1024;
const MAX_ANCESTRY: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub uid: String,
    pub task_id: Option<String>,
    pub workspace_id: String,
    pub canonical_cwd: String,
    pub daemon_id: String,
    pub native_session_id: String,
    pub transcript_generation: u64,
    pub holder_incarnation: Option<u64>,
    pub child_pid: u32,
    pub child_start_time: u64,
    pub background_pid: u32,
    pub background_start_time: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Receipt {
    version: u32,
    identity: Identity,
    phase: String,
    updated_at: f64,
}

#[derive(Clone, Debug)]
struct Snapshot {
    identity: Identity,
    worktree: PathBuf,
    background_jobs: usize,
}

#[derive(Clone, Debug)]
struct BackgroundHost {
    pid: u32,
    start: u64,
}

#[derive(Clone, Debug)]
struct Roster {
    rows: Value,
}

fn active() -> &'static Mutex<HashSet<String>> {
    static ACTIVE: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    ACTIVE.get_or_init(Default::default)
}

thread_local! {
    static INTERNAL: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

pub(crate) struct Reservation {
    uid: String,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        active().lock().unwrap_or_else(|p| p.into_inner()).remove(&self.uid);
    }
}

pub(crate) struct InternalGuard {
    uid: String,
}

impl Drop for InternalGuard {
    fn drop(&mut self) {
        INTERNAL.with(|set| {
            set.borrow_mut().remove(&self.uid);
        });
    }
}

pub(crate) fn internal(uid: &str) -> InternalGuard {
    INTERNAL.with(|set| {
        set.borrow_mut().insert(uid.to_string());
    });
    InternalGuard { uid: uid.to_string() }
}

fn internal_for(uid: &str) -> bool {
    INTERNAL.with(|set| set.borrow().contains(uid))
}

pub(crate) fn in_progress(uid: &str) -> bool {
    active().lock().unwrap_or_else(|p| p.into_inner()).contains(uid)
}

/// Lifecycle admissions call this before they mint/rebind a UID.  A durable
/// offered/confirmed/unknown receipt fences the UID after a daemon restart;
/// only an explicit operator reconciliation path may clear it later.
pub(crate) fn admission_blocked(uid: &str) -> bool {
    if in_progress(uid) {
        return true;
    }
    load_receipt(uid)
        .ok()
        .flatten()
        .is_some_and(|r| !matches!(r.phase.as_str(), "revived" | "failed"))
}

pub(crate) fn admission_refusal(uid: &str) -> Option<String> {
    if internal_for(uid) {
        return None;
    }
    admission_blocked(uid).then(|| {
        format!(
            "session '{}' has a pending or unknown native background recovery; reconcile it before lifecycle changes",
            uid
        )
    })
}

fn receipt_path(uid: &str) -> Option<PathBuf> {
    if uid.is_empty()
        || uid.len() > 128
        || !uid
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    Some(crate::path::dot_cm_dir().join(format!("daemon-claude-background-recovery-{uid}.json")))
}

fn load_receipt(uid: &str) -> io::Result<Option<Receipt>> {
    let Some(path) = receipt_path(uid) else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid session uid"));
    };
    match fs::read(path) {
        Ok(bytes) => {
            let receipt: Receipt = serde_json::from_slice(&bytes)
                .map_err(|e| io::Error::other(format!("invalid recovery receipt: {e}")))?;
            if receipt.version != 1 || !receipt.updated_at.is_finite() {
                return Err(io::Error::other("unsupported recovery receipt"));
            }
            Ok(Some(receipt))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn save_receipt(uid: &str, receipt: &Receipt) -> io::Result<()> {
    let path = receipt_path(uid).ok_or_else(|| io::Error::other("invalid session uid"))?;
    crate::state::write_json_atomic(&path, &serde_json::to_string(receipt)?, true)
}

fn now() -> f64 {
    crate::control::methods::now_unix_f64()
}

fn exact_session_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id).ok().is_some_and(|u| u.to_string() == id)
}

fn canonical(path: &Path) -> io::Result<PathBuf> {
    fs::canonicalize(path)
}

fn proc_parts(pid: u32) -> io::Result<(u32, u64)> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let tail = text
        .rsplit_once(") ")
        .ok_or_else(|| io::Error::other("invalid process stat"))?
        .1;
    let fields: Vec<_> = tail.split_whitespace().collect();
    let parent = fields
        .get(1)
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| io::Error::other("invalid process parent"))?;
    let start = fields
        .get(19)
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| io::Error::other("invalid process start"))?;
    Ok((parent, start))
}

fn proc_argv(pid: u32) -> io::Result<Vec<String>> {
    let file = fs::File::open(format!("/proc/{pid}/cmdline"))?;
    let mut bytes = Vec::new();
    file.take(65537).read_to_end(&mut bytes)?;
    if bytes.len() > 65536 {
        return Err(io::Error::other("process argv too large"));
    }
    bytes
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| {
            String::from_utf8(s.to_vec()).map_err(|_| io::Error::other("invalid process argv"))
        })
        .collect()
}

fn host_argv_matches(argv: &[String], id: &str) -> bool {
    if !exact_session_id(id) || argv.len() < 3 {
        return false;
    }
    let exe = Path::new(&argv[0])
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if !(exe == "claude" || exe.starts_with("claude-"))
        || argv[1] != "bg-pty-host"
    {
        return false;
    }
    let mut ids = Vec::new();
    for (n, arg) in argv.iter().enumerate() {
        if arg == "--session-id" {
            let Some(value) = argv.get(n + 1) else { return false };
            ids.push(value.as_str());
        }
        if let Some(value) = arg.strip_prefix("--session-id=") {
            ids.push(value);
        }
    }
    ids == [id]
}

fn host_proof(id: &str, holder_pid: Option<u32>) -> io::Result<BackgroundHost> {
    if !exact_session_id(id) {
        return Err(io::Error::other("invalid native session ID"));
    }
    let uid = unsafe { libc::geteuid() };
    let mut matches = Vec::new();
    for item in fs::read_dir("/proc")? {
        let item = item?;
        let Ok(pid) = item.file_name().to_string_lossy().parse::<u32>() else { continue };
        let metadata = match fs::metadata(item.path()) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        if metadata.uid() != uid {
            continue;
        }
        let argv = match proc_argv(pid) {
            Ok(argv) => argv,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        if !host_argv_matches(&argv, id) {
            continue;
        }
        let (mut parent, start) = proc_parts(pid)?;
        let mut daemon_ancestor = false;
        let mut inside_holder = holder_pid == Some(pid);
        for _ in 0..MAX_ANCESTRY {
            if parent <= 1 {
                break;
            }
            if holder_pid == Some(parent) {
                inside_holder = true;
            }
            let parent_argv = proc_argv(parent)?;
            let exe = parent_argv
                .first()
                .and_then(|s| Path::new(s).file_name())
                .and_then(|s| s.to_str())
                .unwrap_or("");
            if (exe == "claude" || exe.starts_with("claude-"))
                && parent_argv.get(1).map(String::as_str) == Some("daemon")
                && parent_argv.get(2).map(String::as_str) == Some("run")
            {
                daemon_ancestor = true;
            }
            parent = proc_parts(parent)?.0;
        }
        if daemon_ancestor && !inside_holder {
            matches.push(BackgroundHost { pid, start });
        }
    }
    if matches.len() != 1 {
        return Err(io::Error::other("background host identity unavailable or ambiguous"));
    }
    Ok(matches.remove(0))
}

fn run_command(cwd: &Path, args: &[&str]) -> io::Result<Vec<u8>> {
    let mut file = tempfile::tempfile()?;
    let mut child = Command::new("claude")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(file.try_clone()?))
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    let status = loop {
        if file.metadata()?.len() > MAX_COMMAND_OUTPUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other("Claude command output too large"));
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(io::ErrorKind::TimedOut, "Claude command timed out"));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if !status.success() {
        return Err(io::Error::other("Claude command failed"));
    }
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.take(MAX_COMMAND_OUTPUT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_COMMAND_OUTPUT {
        return Err(io::Error::other("Claude command output too large"));
    }
    Ok(bytes)
}

fn roster(cwd: &Path) -> io::Result<Roster> {
    let bytes = run_command(cwd, &["agents", "--json"])?;
    let rows: Value = serde_json::from_slice(&bytes)
        .map_err(|e| io::Error::other(format!("invalid Claude roster: {e}")))?;
    if !rows.is_array() {
        return Err(io::Error::other("invalid Claude roster"));
    }
    Ok(Roster { rows })
}

fn roster_match(roster: &Roster, id: &str, cwd: &Path) -> io::Result<bool> {
    if !exact_session_id(id) {
        return Err(io::Error::other("invalid native session ID"));
    }
    let expected = canonical(cwd)?;
    let rows = roster
        .rows
        .as_array()
        .ok_or_else(|| io::Error::other("invalid Claude roster"))?;
    let mut found = 0;
    for row in rows {
        if row["kind"] != "background" || row["sessionId"] != id {
            continue;
        }
        let path = row["cwd"]
            .as_str()
            .ok_or_else(|| io::Error::other("background roster omitted cwd"))?;
        if canonical(Path::new(path))? == expected {
            found += 1;
        } else {
            return Err(io::Error::other("native ID belongs to another workspace"));
        }
    }
    match found {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(io::Error::other("ambiguous background roster")),
    }
}

fn snapshot(state: &DaemonState, uid: &str) -> Result<Snapshot, String> {
    let Some(session) = state.sessions.get(uid) else {
        let tomb = state
            .recently_exited
            .iter()
            .find(|t| t.session_uid == uid)
            .ok_or_else(|| format!("session '{uid}' is not known on this daemon"))?;
        if tomb.session_type != "claude-code" {
            return Err("background recovery is only supported for Claude sessions".into());
        }
        let worktree = tomb
            .worktree_path
            .as_deref()
            .map(PathBuf::from)
            .ok_or_else(|| "exited session has no worktree".to_string())?;
        let cwd = canonical(&worktree).map_err(|e| format!("canonical cwd: {e}"))?;
        let native_session_id = tomb
            .transcript_path
            .as_deref()
            .and_then(crate::session::transcript_id_from_path)
            .ok_or_else(|| "exited session has no bound native transcript identity".to_string())?;
        return Ok(Snapshot {
            identity: Identity {
                uid: uid.to_string(),
                task_id: tomb.task_id.clone(),
                workspace_id: tomb.workspace_id.clone(),
                canonical_cwd: cwd.to_string_lossy().into_owned(),
                daemon_id: crate::host_id::HostId::local().as_str().to_string(),
                native_session_id,
                transcript_generation: tomb.generation,
                holder_incarnation: tomb.incarnation,
                child_pid: 0,
                child_start_time: 0,
                background_pid: 0,
                background_start_time: 0,
            },
            worktree: cwd,
            background_jobs: 0,
        });
    };
    if session.session_type != "claude-code" {
        return Err("background recovery is only supported for Claude sessions".into());
    }
    let worktree = state
        .workspaces
        .get(&session.workspace_id)
        .and_then(|w| w.worktree_path.clone())
        .ok_or_else(|| "session workspace has no worktree".to_string())?;
    let cwd = canonical(&worktree).map_err(|e| format!("canonical cwd: {e}"))?;
    let native_session_id = session
        .transcript_path
        .as_deref()
        .and_then(crate::session::transcript_id_from_path)
        .ok_or_else(|| "session has no bound native transcript identity".to_string())?;
    let child_start_time = crate::adopt::proc_starttime(session.pid as libc::pid_t)
        .map_err(|e| format!("child start time: {e}"))?;
    let agent = crate::agent_state::current(session);
    let identity = Identity {
        uid: uid.to_string(),
        task_id: session.task_id.clone(),
        workspace_id: session.workspace_id.clone(),
        canonical_cwd: cwd.to_string_lossy().into_owned(),
        daemon_id: crate::host_id::HostId::local().as_str().to_string(),
        native_session_id,
        transcript_generation: session.generation,
        holder_incarnation: session.holder_incarnation,
        child_pid: session.pid as u32,
        child_start_time,
        background_pid: 0,
        background_start_time: 0,
    };
    Ok(Snapshot { identity, worktree: cwd, background_jobs: agent.background.jobs.len() })
}

fn lifecycle_refusal(state: &DaemonState, uid: &str) -> Option<String> {
    if state.draining || state.restarting {
        return Some("daemon is draining or restarting".into());
    }
    if crate::rotation::in_progress(uid) {
        return Some("transcript rotation is already running".into());
    }
    if crate::handoff::in_progress(uid) {
        return Some("session handoff is already running".into());
    }
    state.sessions.get(uid).and_then(|s| {
        if s.workflow_run_id.is_some() {
            Some("workflow participant owns this lifecycle".into())
        } else if s.continuous_task_id.is_some() {
            Some("continuous-task scheduler owns this lifecycle".into())
        } else {
            None
        }
    })
}

pub(crate) fn preflight(state_arc: &Arc<Mutex<DaemonState>>, uid: &str) -> Value {
    let uid = uid.trim();
    if uid.is_empty() {
        return json!({"status":"unavailable","identity":Value::Null,"message":"uid is required"});
    }
    let snap = {
        let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(reason) = lifecycle_refusal(&state, uid) {
            return json!({"status":"unavailable","identity":Value::Null,"message":reason});
        }
        if let Some(tomb) = state.recently_exited.iter().find(|t| t.session_uid == uid) {
            if tomb.workflow_run_id.is_some() {
                return json!({"status":"unavailable","identity":Value::Null,"message":"workflow participant owns this lifecycle"});
            }
            if tomb.continuous_task_id.is_some() {
                return json!({"status":"unavailable","identity":Value::Null,"message":"continuous-task scheduler owns this lifecycle"});
            }
        }
        if admission_blocked(uid) {
            return json!({"status":"unavailable","identity":Value::Null,"message":"a recovery receipt is already pending or unknown; reconcile it first"});
        }
        match snapshot(&state, uid) {
            Ok(snap) => snap,
            Err(message) => return json!({"status":"unavailable","identity":Value::Null,"message":message}),
        }
    };
    let roster = match roster(&snap.worktree) {
        Ok(roster) => roster,
        Err(e) => return json!({"status":"unavailable","identity":Value::Null,"message":format!("Claude roster unavailable: {e}")}),
    };
    let listed = match roster_match(&roster, &snap.identity.native_session_id, &snap.worktree) {
        Ok(value) => value,
        Err(e) => return json!({"status":"unavailable","identity":Value::Null,"message":format!("Claude roster evidence unavailable: {e}")}),
    };
    if !listed {
        if snap.background_jobs != 0 {
            return json!({"status":"unavailable","identity":Value::Null,"message":"daemon reports background work but Claude roster does not prove its identity"});
        }
        return json!({"status":"normal","identity":Value::Null,"message":"no exact native background session is present"});
    }
    let host = match host_proof(
        &snap.identity.native_session_id,
        (snap.identity.child_pid != 0).then_some(snap.identity.child_pid),
    ) {
        Ok(host) => host,
        Err(e) => return json!({"status":"unavailable","identity":Value::Null,"message":format!("background host evidence unavailable: {e}")}),
    };
    let mut identity = snap.identity;
    identity.background_pid = host.pid;
    identity.background_start_time = host.start;
    let receipt = Receipt { version: 1, identity: identity.clone(), phase: "offered".into(), updated_at: now() };
    if let Err(e) = save_receipt(uid, &receipt) {
        return json!({"status":"unavailable","identity":Value::Null,"message":format!("could not persist recovery intent: {e}")});
    }
    json!({"status":"offer","identity":identity,"message":"Claude native background work is active; explicit confirmation is required"})
}

fn identity_from_value(value: &Value) -> Result<Identity, String> {
    serde_json::from_value(value.clone()).map_err(|e| format!("invalid recovery identity: {e}"))
}

fn mark(uid: &str, identity: &Identity, phase: &str) -> Result<(), String> {
    save_receipt(uid, &Receipt { version: 1, identity: identity.clone(), phase: phase.into(), updated_at: now() })
        .map_err(|e| format!("persist recovery checkpoint: {e}"))
}

fn wait_gone(state_arc: &Arc<Mutex<DaemonState>>, uid: &str) -> Result<(), String> {
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    loop {
        if !state_arc.lock().unwrap_or_else(|p| p.into_inner()).sessions.contains_key(uid) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("managed Claude holder did not exit before timeout".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub(crate) fn confirm(
    state_arc: &Arc<Mutex<DaemonState>>,
    identity_value: &Value,
    confirmed: bool,
) -> Result<Value, (ErrorCode, String)> {
    if !confirmed {
        return Err((ErrorCode::InvalidParams, "explicit confirmation is required".into()));
    }
    let identity = identity_from_value(identity_value)
        .map_err(|e| (ErrorCode::InvalidParams, e))?;
    let uid = identity.uid.clone();
    let receipt = load_receipt(&uid)
        .map_err(|e| (ErrorCode::Conflict, format!("recovery receipt unavailable: {e}")))?
        .ok_or_else(|| (ErrorCode::Conflict, "no durable recovery offer exists".into()))?;
    if receipt.phase != "offered" || receipt.identity != identity {
        return Err((ErrorCode::Conflict, "recovery identity is stale or already consumed".into()));
    }
    let reservation = {
        let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(reason) = lifecycle_refusal(&state, &uid) {
            return Err((ErrorCode::Conflict, reason));
        }
        let snap = snapshot(&state, &uid).map_err(|e| (ErrorCode::Conflict, e))?;
        if snap.identity != identity {
            return Err((ErrorCode::Conflict, "session identity changed since preflight".into()));
        }
        if in_progress(&uid) {
            return Err((ErrorCode::Conflict, "recovery is already in progress".into()));
        }
        active().lock().unwrap_or_else(|p| p.into_inner()).insert(uid.clone());
        Reservation { uid: uid.clone() }
    };
    let _reservation = reservation;
    let roster = roster(Path::new(&identity.canonical_cwd))
        .map_err(|e| (ErrorCode::Conflict, format!("Claude roster unavailable before effect: {e}")))?;
    if !roster_match(&roster, &identity.native_session_id, Path::new(&identity.canonical_cwd))
        .map_err(|e| (ErrorCode::Conflict, format!("native identity unavailable before effect: {e}")))?
    {
        return Err((ErrorCode::Conflict, "native background identity disappeared before effect".into()));
    }
    let host = host_proof(
        &identity.native_session_id,
        (identity.child_pid != 0).then_some(identity.child_pid),
    )
        .map_err(|e| (ErrorCode::Conflict, format!("background host changed before effect: {e}")))?;
    if host.pid != identity.background_pid || host.start != identity.background_start_time {
        return Err((ErrorCode::Conflict, "background host identity changed before effect".into()));
    }
    mark(&uid, &identity, "confirmed").map_err(|e| (ErrorCode::Conflict, e))?;
    {
        let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        if state.sessions.contains_key(&uid) {
            drop(state);
            crate::control::methods::kill_with_attribution(state_arc, &uid, "background-recovery");
            wait_gone(state_arc, &uid).map_err(|e| (ErrorCode::Conflict, e))?;
        }
    }
    mark(&uid, &identity, "holder_stopped").map_err(|e| (ErrorCode::Conflict, e))?;
    if let Err(e) = run_command(Path::new(&identity.canonical_cwd), &["stop", &identity.native_session_id]) {
        let _ = mark(&uid, &identity, "unknown");
        return Ok(json!({"status":"unknown","message":format!("native stop outcome is unknown: {e}")}));
    }
    mark(&uid, &identity, "native_stopped").map_err(|e| (ErrorCode::Conflict, e))?;
    let _internal = internal(&uid);
    let revive_params = json!({
        "uid": uid,
        "workspace_id": identity.workspace_id,
        "worktree_path": identity.canonical_cwd,
        "session_type": "claude-code",
        "task_id": identity.task_id,
        "transcript_id": identity.native_session_id,
    });
    match crate::control::methods::revive_session(state_arc, &revive_params) {
        Ok(_) => {
            mark(&identity.uid, &identity, "revived").map_err(|e| (ErrorCode::Conflict, e))?;
            Ok(json!({"status":"revived","message":"native background stopped and session revived"}))
        }
        Err((_, e)) => {
            let _ = mark(&identity.uid, &identity, "partial");
            Ok(json!({"status":"partial","message":format!("native background stopped but revive failed: {e}")}))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_argv_requires_exact_single_session_id() {
        let id = "123e4567-e89b-12d3-a456-426614174000";
        assert!(host_argv_matches(&vec!["claude".into(), "bg-pty-host".into(), "--session-id".into(), id.into()], id));
        assert!(!host_argv_matches(&vec!["claude".into(), "bg-pty-host".into(), "--session-id".into(), id.into(), "--session-id".into(), id.into()], id));
        assert!(!host_argv_matches(&vec!["claude".into(), "bg-pty-host".into(), "--session-id".into(), "other".into()], id));
    }

    #[test]
    fn roster_match_rejects_other_workspace_and_accepts_one_exact_row() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("cwd");
        fs::create_dir_all(&cwd).unwrap();
        let id = "123e4567-e89b-12d3-a456-426614174000";
        let roster = Roster { rows: json!([{"kind":"background","sessionId":id,"cwd":cwd}]) };
        assert!(roster_match(&roster, id, &cwd).unwrap());
        let other = dir.path().join("other");
        fs::create_dir_all(&other).unwrap();
        let roster = Roster { rows: json!([{"kind":"background","sessionId":id,"cwd":other}]) };
        assert!(roster_match(&roster, id, &cwd).is_err());
    }

    #[test]
    fn corrupt_or_pending_receipt_blocks_admission() {
        let uid = "ts-recovery-receipt-test";
        let path = receipt_path(uid).unwrap();
        let _ = fs::remove_file(&path);
        let identity = Identity { uid: uid.into(), task_id: None, workspace_id: "w".into(), canonical_cwd: "/tmp".into(), daemon_id: "d".into(), native_session_id: "123e4567-e89b-12d3-a456-426614174000".into(), transcript_generation: 0, holder_incarnation: None, child_pid: 1, child_start_time: 1, background_pid: 2, background_start_time: 2 };
        save_receipt(uid, &Receipt { version: 1, identity, phase: "unknown".into(), updated_at: 1.0 }).unwrap();
        assert!(admission_blocked(uid));
        fs::write(&path, b"not-json").unwrap();
        assert!(load_receipt(uid).is_err());
        let _ = fs::remove_file(path);
    }
}
