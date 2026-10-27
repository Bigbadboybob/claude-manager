//! A-R restart/revive, A-O remote restore and remote A-s, off the input
//! thread.
//!
//! All three used to run their daemon RPCs on the UI thread: an A-R on a live
//! session was a kill (5 s budget), then up to 40 registry polls (5 s budget
//! each) waiting for the reaper, then the revive. Over a slow link (Owner's,
//! under 1 Mbps on 2026-10-16) that froze keyboard input to every pane for
//! minutes. The RPCs now run on a worker; the UI thread only applies the
//! outcome in `drain_restarts`.
use super::*;
use cm_daemon::host_id::HostId;
use cm_daemon::manifest::ManifestEntry;
use std::sync::mpsc::{self, Receiver, TryRecvError};

#[cfg(test)]
mod recovery_tests {
    use super::*;

    #[test]
    fn recovery_offer_requires_exact_displayable_identity_and_space() {
        let mut offer = serde_json::json!({"status":"offer", "identity":{
            "uid":"ts-x", "native_session_id":"123e4567-e89b-12d3-a456-426614174000",
            "background_pid":42, "background_start_time":77, "workspace_id":"ws", "canonical_cwd":"/workspace", "task_id":null
        }});
        assert!(valid_background_offer(&offer, "ts-x"));
        assert!(!valid_background_offer(&offer, "another"));
        offer["identity"]["native_session_id"] = serde_json::json!("123e4567");
        assert!(!valid_background_offer(&offer, "ts-x"));
        offer["identity"]["native_session_id"] = serde_json::json!("123e4567-e89b-12d3-a456-426614174000");
        offer["identity"]["canonical_cwd"] = serde_json::json!("/work\nspace");
        assert!(!valid_background_offer(&offer, "ts-x"));
        offer["identity"]["canonical_cwd"] = serde_json::json!("/workspace");
        offer["identity"]["native_session_id"] = serde_json::Value::Null;
        assert!(!valid_background_offer(&offer, "ts-x"));
        assert!(valid_recovery_identity(&offer["identity"], "ts-x", true), "explicit fresh identity is valid for normal restart");
        offer["identity"].as_object_mut().unwrap().remove("native_session_id");
        assert!(!valid_recovery_identity(&offer["identity"], "ts-x", true), "missing is not proof of a fresh identity");
        assert!(!background_prompt_fits("details", (15, 8)));
        assert!(background_prompt_fits("Native ID: exact\nPID: 42\nWorkspace: /work", (100, 30)));
    }
}

/// How long a forced restart waits for the killed child to leave the
/// host's registry. Each poll is its own RPC.
const REAP_WAIT: Duration = Duration::from_secs(20);
const REAP_POLL: Duration = Duration::from_millis(50);

pub(super) enum RestartOutcome {
    Unreachable,
    KillFailed(String),
    /// Killed, but the uid did not clear within [`REAP_WAIT`].
    StillRunning,
    ReviveFailed(String),
    /// Remote: the host respawned the session at the same uid.
    Revived,
    /// A rotation or handoff is bringing it back at the same uid.
    ComingBack,
    /// Local live session killed and gone; the UI thread respawns it.
    Killed,
    /// Read-only preflight found a specific external Claude background host.
    BackgroundOffer(serde_json::Value),
    RecoveryRecord(serde_json::Value),
    RecoveryCleared(String),
    /// Recovery was not attempted or its durable result needs inspection.
    RecoveryNeedsAttention(String),
    /// An effect may have stopped the holder; keep its row until reconciled.
    RecoveryUncertain(String),
}

pub(super) struct RestartFlight {
    pub uid: String,
    pub host_id: HostId,
    pub was_live: bool,
    /// The daemon uid marked in `restart_suppressed_exit_uids` for this
    /// restart's own kill.
    pub daemon_uid: Option<String>,
    result: Receiver<RestartOutcome>,
}

/// What a refused `session.revive` means for the row.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReviveRefusal {
    /// The daemon runs it: reattach.
    Running,
    /// A rotation or handoff is bringing it back: wait for that respawn.
    ComingBack,
    /// A real failure, shown as one.
    Failed,
}

/// Classify a `session.revive` error. `killed`: this A-R killed the session
/// first, so "still running" means the kill did not land (the old process),
/// which is a failure to show, not a session to reattach to.
pub(crate) fn classify_revive_refusal(error: &str, killed: bool) -> ReviveRefusal {
    if error.contains("comes straight back") {
        ReviveRefusal::ComingBack
    } else if error.contains("is still running") && !killed {
        ReviveRefusal::Running
    } else {
        ReviveRefusal::Failed
    }
}

/// One remote revive: `(entry, workspace_id, worktree)`.
type ReviveSpec = (ManifestEntry, String, String);

fn valid_recovery_identity(id: &serde_json::Value, uid: &str, allow_fresh: bool) -> bool {
    let canonical = match id.get("native_session_id") {
        Some(serde_json::Value::Null) if allow_fresh => true,
        Some(serde_json::Value::String(native)) => uuid::Uuid::parse_str(native).ok().is_some_and(|v| v.to_string() == *native),
        _ => false,
    };
    id["uid"] == uid && canonical
        && id["workspace_id"].as_str().is_some_and(|s| !s.is_empty())
        && id["canonical_cwd"].as_str().is_some_and(|s| !s.is_empty() && !s.chars().any(char::is_control))
        && match id.get("task_id") {
            Some(serde_json::Value::Null) => true,
            Some(serde_json::Value::String(s)) => !s.chars().any(char::is_control),
            _ => false,
        }
}

fn valid_background_offer(offer: &serde_json::Value, uid: &str) -> bool {
    let id = &offer["identity"];
    offer["status"] == "offer" && valid_recovery_identity(id, uid, false)
        && id["background_pid"].as_u64().is_some_and(|v| v > 0)
        && id["background_start_time"].as_u64().is_some_and(|v| v > 0)
}

fn valid_recovery_record(record: &serde_json::Value, uid: &str) -> bool {
    let id = &record["identity"];
    match (id["background_pid"].as_u64(), id["background_start_time"].as_u64()) {
        (Some(0), Some(0)) => valid_recovery_identity(id, uid, true),
        (Some(1..), Some(1..)) => valid_recovery_identity(id, uid, false),
        _ => false,
    }
}

fn background_prompt_fits(prompt: &str, size: (u16, u16)) -> bool {
    let width = 100u16.min(size.0.saturating_sub(4)).saturating_sub(2) as usize;
    if width < 20 { return false; }
    // Reserve an extra row per logical line for word wrapping, plus borders
    // and the confirmation instructions. If it cannot fit, refuse to offer
    // a destructive action whose identifying details would be hidden.
    let rows = prompt.lines().map(|s| 1 + unicode_width::UnicodeWidthStr::width(s).div_ceil(width)).sum::<usize>() + 6;
    rows <= usize::from(size.1.saturating_sub(2))
}

fn run_restart(
    pool: &crate::host_pool::HostPool,
    host: &HostId,
    kill_uid: Option<&str>,
    revive: Option<&ReviveSpec>,
    probe_background: bool,
) -> RestartOutcome {
    // Resolving the socket can warm a cold tunnel, so it happens here too.
    let Some(socket) = pool.for_host(host).ok().and_then(|h| h.socket_path()) else {
        return RestartOutcome::Unreachable;
    };
    let token = pool.operator_token_for(host);
    if probe_background {
        let uid = kill_uid.or_else(|| revive.map(|(entry, _, _)| entry.uid.as_str()));
        let Some(uid) = uid else {
            return RestartOutcome::RecoveryNeedsAttention("Cannot verify the session identity for background recovery".into());
        };
        let result = crate::client_session::rpc_messaging(
            &socket, &token, "session.background_recovery_preflight", serde_json::json!({"uid": uid}));
        match result {
            Ok(value) if value["status"] == "normal" => {
                let identity = &value["identity"];
                let valid = valid_recovery_identity(identity, uid, true)
                    && identity["background_pid"].as_u64() == Some(0)
                    && identity["background_start_time"].as_u64() == Some(0);
                if !valid {
                    return RestartOutcome::RecoveryNeedsAttention("Normal restart identity was incomplete; no stop was attempted".into());
                }
                let result = crate::client_session::rpc_messaging_board(&socket, &token,
                    "session.background_recovery_confirm", serde_json::json!({"identity":identity, "confirmed":true}));
                return match result {
                    Ok(reply) if reply["status"] == "revived" => RestartOutcome::Revived,
                    Ok(reply) => RestartOutcome::RecoveryUncertain(format!("{}; inspect recovery status before retrying", reply["message"].as_str().unwrap_or("Restart did not complete"))),
                    Err(e) => RestartOutcome::RecoveryUncertain(format!("Restart result is unknown: {e}; inspect recovery status before retrying")),
                };
            }
            Ok(value) if valid_background_offer(&value, uid) => {
                return RestartOutcome::BackgroundOffer(value);
            }
            Ok(value) => {
                if value["status"] == "unavailable" {
                    if let Ok(record) = crate::client_session::rpc_messaging(&socket, &token,
                        "session.background_recovery_status", serde_json::json!({"uid":uid})) {
                        if valid_recovery_record(&record, uid)
                            && matches!(record["status"].as_str(), Some("confirmed" | "holder_stopped" | "native_stop_ready" | "native_stopped" | "unknown" | "partial")) {
                            return RestartOutcome::RecoveryRecord(record);
                        }
                    }
                }
                return RestartOutcome::RecoveryNeedsAttention(
                    value["message"].as_str().unwrap_or("Background-session identity is unavailable").into());
            }
            Err(e) => return RestartOutcome::RecoveryNeedsAttention(format!(
                "Background preflight failed; session was not stopped: {e}")),
        }
    }
    if let Some(uid) = kill_uid {
        // A failed kill is not the last word: over a slow link the kill can
        // land with its reply lost, and a second A-R then finds the session
        // already exited (2026-10-18, Swarm-Coord). The registry decides.
        let kill_error = crate::client_session::rpc_kill_session(&socket, &token, uid).err();
        // `kill_session` SIGKILLs and returns before the reaper runs; the
        // revive is refused while the uid is still registered.
        let deadline = Instant::now() + REAP_WAIT;
        loop {
            match crate::client_session::rpc_list_session_uids(&socket, &token) {
                Ok(uids) if !uids.contains(uid) => break,
                _ if Instant::now() >= deadline => {
                    return match kill_error {
                        Some(e) => RestartOutcome::KillFailed(e.to_string()),
                        None => RestartOutcome::StillRunning,
                    };
                }
                _ => std::thread::sleep(REAP_POLL),
            }
        }
    }
    match revive {
        None => RestartOutcome::Killed,
        Some((entry, ws_id, worktree)) => {
            match crate::client_session::rpc_session_revive(&socket, &token, entry, ws_id, worktree) {
                Ok(()) => RestartOutcome::Revived,
                Err(e) => match classify_revive_refusal(&e.to_string(), kill_uid.is_some()) {
                    ReviveRefusal::Running => RestartOutcome::Revived,
                    ReviveRefusal::ComingBack => RestartOutcome::ComingBack,
                    ReviveRefusal::Failed => RestartOutcome::ReviveFailed(e.to_string()),
                },
            }
        }
    }
}

pub(super) struct RestoreJob {
    pub uid: String,
    pub label: String,
    pub entry: ManifestEntry,
    pub worktree: String,
}

pub(super) struct RestoreFlight {
    pub ws_id: String,
    /// The uids being revived, so the adopt pass leaves them alone.
    pub uids: Vec<String>,
    pub host_id: HostId,
    pub total: usize,
    /// Tombstones that could not even be attempted (no worktree).
    pub errors: Vec<String>,
    result: Receiver<Result<Vec<(RestoreJob, Result<(), String>)>, String>>,
}

/// Remote A-s: everything the worker needs, captured at keypress time.
pub(super) struct RemoteAdd {
    pub host: HostId,
    pub workspace_id: String,
    pub uid: String,
    pub session_type: String,
    pub task_id: Option<String>,
    pub seed_from: Option<String>,
    pub resume_from: Option<String>,
    /// `None` = the host resolves it (task default, else engine default).
    pub model: Option<String>,
    pub effort: Option<String>,
    pub cols: u16,
    pub rows: u16,
}

type AddOutcome = Result<
    (crate::client_session::AddSessionResult, Option<ManifestEntry>, Result<Session, String>),
    String,
>;

pub(super) struct AddFlight {
    job: RemoteAdd,
    result: Receiver<AddOutcome>,
}

/// `add_session`, then attach. A fresh session whose attach fails is killed
/// (an unattached session would run unseen); a resumed one is handed back
/// for the deferred reattach.
fn run_remote_add(pool: &crate::host_pool::HostPool, job: &RemoteAdd) -> AddOutcome {
    let host = &job.host;
    let Some(socket) = pool.for_host(host).ok().and_then(|h| h.socket_path()) else {
        return Err(format!("Remote host `{}` not reachable (no live socket)", host.as_str()));
    };
    let token = pool.operator_token_for(host);
    // Daemon WIRE engine ("claude" → "claude-code").
    let wire_engine = match job.session_type.as_str() {
        "claude" => "claude-code",
        other => other,
    };
    let res = crate::client_session::rpc_add_session_with_resume(
        &socket,
        &token,
        &job.uid,
        &job.workspace_id,
        &job.session_type,
        wire_engine,
        job.task_id.as_deref(),
        job.cols,
        job.rows,
        job.resume_from.as_deref(),
        job.seed_from.as_deref(),
        job.model.as_deref(),
        job.effort.as_deref(),
    )
    .map_err(|e| format!("Remote add_session failed: {e}"))?;
    let workspace_id = res.workspace_id.clone().unwrap_or_else(|| job.workspace_id.clone());
    let mut restored_entry = res.entry.clone();
    if let Some(e) = restored_entry.as_mut() {
        e.host_id = host.clone();
        e.session_type = normalize_session_type_to_internal(&e.session_type).to_string();
    }
    let task_id = restored_entry.as_ref().map(|e| e.task_id.clone()).unwrap_or(job.task_id.clone());
    let label = restored_entry.as_ref().map(|e| e.label.clone()).unwrap_or(job.session_type.clone());
    let attached = super::lifecycle::try_attach_via_daemon_with_deps(
        pool,
        &res.session_uid,
        &workspace_id,
        &PathBuf::from(&res.worktree_path),
        &job.session_type,
        &label,
        job.cols,
        job.rows,
        task_id.as_deref(),
        restored_entry.as_ref().and_then(|e| e.workflow_run_id.as_deref()),
        restored_entry.as_ref().and_then(|e| e.workflow_role.as_deref()),
        host,
        None,
    );
    match attached {
        Ok(session) => Ok((res, restored_entry, Ok(session))),
        Err(e) if job.resume_from.is_some() => Ok((res, restored_entry, Err(e.to_string()))),
        Err(e) => {
            // Mirror `ClientSession::new`'s cleanup contract; the attach
            // error wins.
            if let Err(cleanup_err) = crate::client_session::rpc_kill_session(&socket, &token, &res.session_uid) {
                eprintln!(
                    "add_remote_session cleanup: kill_session({}) failed after attach error: {}",
                    res.session_uid, cleanup_err,
                );
            }
            Err(format!("Remote attach failed: {e}"))
        }
    }
}

impl App {
    pub(super) fn start_background_reconcile(&mut self, uid: String, host: HostId, identity: serde_json::Value, prompt: String) {
        if !background_prompt_fits(&prompt, self.last_term_size) {
            self.set_status_msg("Terminal is too small to show the full recovery identity; enlarge it and press A-R again");
            return;
        }
        if self.restart_in_flight(&uid) || !self.workspaces.iter().flat_map(|w| &w.sessions).any(|s| s.uid == uid && s.host_id == host) {
            self.set_status_msg("Session or host changed; no reconciliation was attempted");
            return;
        }
        let pool = std::sync::Arc::clone(&self.host_pool);
        let job_host = host.clone();
        let daemon_uid = identity["uid"].as_str().unwrap_or("").to_owned();
        let (tx, result) = mpsc::channel();
        let spawned = std::thread::Builder::new().name("cm-background-reconcile".into()).spawn(move || {
            let outcome = (|| {
                let socket = pool.for_host(&job_host).ok().and_then(|h| h.socket_path())
                    .ok_or_else(|| "Host unreachable; reconciliation was not confirmed".to_string())?;
                let token = pool.operator_token_for(&job_host);
                let reply = crate::client_session::rpc_messaging_board(&socket, &token,
                    "session.background_recovery_reconcile", serde_json::json!({"uid": daemon_uid, "identity": identity, "action":"clear"}))
                    .map_err(|e| format!("Reconciliation result is unknown: {e}; inspect recovery status before retrying"))?;
                if reply["status"] == "cleared" {
                    Ok(reply["message"].as_str().unwrap_or("Recovery fence cleared").to_owned())
                } else { Err(reply["message"].as_str().unwrap_or("Reconciliation did not complete").to_owned()) }
            })();
            let _ = tx.send(match outcome {
                Ok(message) => RestartOutcome::RecoveryCleared(message),
                Err(message) => RestartOutcome::RecoveryNeedsAttention(message),
            });
        });
        if let Err(e) = spawned {
            self.set_status_msg(&format!("Could not start reconciliation worker: {e}"));
            return;
        }
        self.restarts.push(RestartFlight { uid, host_id: host, was_live: false, daemon_uid: None, result });
        self.set_status_msg("Checking the interrupted recovery…");
    }

    /// Execute only the exact offer that the operator explicitly confirmed.
    /// The daemon revalidates and journals before either stop effect.
    pub(super) fn start_background_recovery(&mut self, uid: String, host: HostId, identity: serde_json::Value, prompt: String) {
        if !background_prompt_fits(&prompt, self.last_term_size) {
            self.set_status_msg("Terminal is too small to show the full recovery identity; enlarge it and press A-R again");
            return;
        }
        if self.restart_in_flight(&uid) {
            self.set_status_msg("A restart of this session is already in progress");
            return;
        }
        let row = self.workspaces.iter().flat_map(|w| &w.sessions)
            .find(|s| s.uid == uid && s.host_id == host);
        let Some(row) = row else {
            self.set_status_msg("The selected session or host changed; run A-R again");
            return;
        };
        if row.workflow_run_id.is_some() || row.continuous_task_id.is_some() {
            self.set_status_msg("This session's lifecycle is owned by a workflow or scheduler");
            return;
        }
        let was_live = !row.session.exited;
        let daemon_uid = was_live.then(|| row.session.daemon_session_uid.clone()).flatten();
        if was_live && daemon_uid.is_none() {
            self.set_status_msg("The holder identity is unavailable; no stop was attempted");
            return;
        }
        if let Some(d) = &daemon_uid {
            self.restart_suppressed_exit_uids.insert(d.clone());
        }
        let pool = std::sync::Arc::clone(&self.host_pool);
        let job_host = host.clone();
        let (tx, result) = mpsc::channel();
        let spawned = std::thread::Builder::new().name("cm-background-recovery".into()).spawn(move || {
            let outcome = (|| {
                let socket = pool.for_host(&job_host).ok().and_then(|h| h.socket_path())
                    .ok_or_else(|| "Host unreachable; recovery was not confirmed".to_string())?;
                let token = pool.operator_token_for(&job_host);
                let reply = crate::client_session::rpc_messaging_board(&socket, &token,
                    "session.background_recovery_confirm", serde_json::json!({"identity": identity, "confirmed": true}))
                    .map_err(|e| format!("Recovery result is unknown: {e}; inspect the durable recovery record before retrying"))?;
                if reply["status"] == "revived" { Ok(()) } else {
                    Err(format!("{}; inspect the durable recovery record before retrying",
                        reply["message"].as_str().unwrap_or("Recovery did not complete")))
                }
            })();
            let _ = tx.send(match outcome {
                Ok(()) => RestartOutcome::Revived,
                Err(e) => RestartOutcome::RecoveryUncertain(e),
            });
        });
        if let Err(e) = spawned {
            if let Some(d) = &daemon_uid { self.restart_suppressed_exit_uids.remove(d); }
            self.set_status_msg(&format!("Could not start recovery worker: {e}"));
            return;
        }
        self.restarts.push(RestartFlight { uid, host_id: host, was_live, daemon_uid, result });
        self.set_status_msg("Checking and recovering the confirmed background session…");
    }

    pub(super) fn restart_in_flight(&self, uid: &str) -> bool {
        self.restarts.iter().any(|f| f.uid == uid)
    }

    /// Uids a worker is spawning, reviving or restarting right now; their own
    /// result binds them, so the adopt pass must not.
    pub(super) fn session_uids_in_flight(&self) -> HashSet<String> {
        self.restarts
            .iter()
            .map(|f| f.uid.clone())
            .chain(self.restores.iter().flat_map(|f| f.uids.iter().cloned()))
            .chain(self.remote_adds.iter().map(|f| f.job.uid.clone()))
            .chain(self.remote_creates.iter().map(|f| f.spec.uid.clone()))
            .chain(self.plan_launches.iter().map(|f| f.spec.uid.clone()))
            .collect()
    }

    /// Start A-R's daemon half on a worker: kill a live session and wait
    /// for it to clear (when `was_live`), then revive it on its host (when
    /// remote). Guards and the local dead-session revive stay with the
    /// caller.
    pub(super) fn start_restart(&mut self, wi: usize, si: usize, was_live: bool) {
        let Some(ws) = self.workspaces.get(wi) else { return };
        let Some(ts) = ws.sessions.get(si) else { return };
        let uid = ts.uid.clone();
        if self.restart_in_flight(&uid) {
            self.set_status_msg("A restart of this session is already in progress");
            return;
        }
        let daemon_uid = if was_live {
            let Some(daemon_uid) = ts.session.daemon_session_uid.clone() else {
                self.set_status_msg("Not a daemon-owned session — A-R can't force-restart it");
                return;
            };
            Some(daemon_uid)
        } else {
            None
        };
        let host_id = ts.host_id.clone();
        let probe_background = matches!(ts.session_type.as_str(), "claude" | "claude-code");
        let revive = (host_id != HostId::local() || (probe_background && !was_live)).then(|| {
            let mut entry = ts.to_manifest_entry();
            entry.last_exit = None;
            let worktree = ws
                .worktree_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            (entry, ws.id.clone(), worktree)
        });
        let label = ts.label.clone();
        // Marked before the kill can land: its own exit (diff or attach
        // stream) must not stamp or prune the row the revive will reuse.
        if let Some(d) = &daemon_uid {
            self.restart_suppressed_exit_uids.insert(d.clone());
        }
        let pool = std::sync::Arc::clone(&self.host_pool);
        let (tx, result) = mpsc::channel();
        let job_host = host_id.clone();
        let job_kill = daemon_uid.clone();
        let spawned = std::thread::Builder::new()
            .name("cm-restart".into())
            .spawn(move || {
                let _ = tx.send(run_restart(&pool, &job_host, job_kill.as_deref(), revive.as_ref(), probe_background));
            });
        if let Err(e) = spawned {
            if let Some(d) = &daemon_uid {
                self.restart_suppressed_exit_uids.remove(d);
            }
            self.set_status_msg(&format!("Could not start the restart worker: {e}"));
            return;
        }
        self.restarts.push(RestartFlight { uid, host_id, was_live, daemon_uid, result });
        self.set_status_msg(&format!(
            "{} {label}…",
            if probe_background { "Checking" } else if was_live { "Restarting" } else { "Reviving" }
        ));
        self.needs_redraw = true;
    }

    pub(super) fn start_remote_add(&mut self, job: RemoteAdd) {
        let pool = std::sync::Arc::clone(&self.host_pool);
        let (tx, result) = mpsc::channel();
        let request = RemoteAdd {
            host: job.host.clone(),
            workspace_id: job.workspace_id.clone(),
            uid: job.uid.clone(),
            session_type: job.session_type.clone(),
            task_id: job.task_id.clone(),
            seed_from: job.seed_from.clone(),
            resume_from: job.resume_from.clone(),
            model: job.model.clone(), effort: job.effort.clone(),
            cols: job.cols,
            rows: job.rows,
        };
        let spawned = std::thread::Builder::new()
            .name("cm-remote-add".into())
            .spawn(move || {
                let _ = tx.send(run_remote_add(&pool, &request));
            });
        if let Err(e) = spawned {
            self.set_status_msg(&format!("Could not start the add-session worker: {e}"));
            return;
        }
        self.set_status_msg(&format!(
            "{} {} on `{}`…",
            if job.resume_from.is_some() { "Resuming" } else { "Starting" },
            job.session_type,
            job.host.as_str()
        ));
        self.remote_adds.push(AddFlight { job, result });
    }

    fn drain_remote_adds(&mut self) {
        let mut i = 0;
        while i < self.remote_adds.len() {
            let outcome = match self.remote_adds[i].result.try_recv() {
                Ok(outcome) => outcome,
                Err(TryRecvError::Empty) => {
                    i += 1;
                    continue;
                }
                Err(TryRecvError::Disconnected) => Err("add-session worker stopped".into()),
            };
            let flight = self.remote_adds.remove(i);
            match outcome {
                Ok((res, entry, session)) => self.finish_remote_add(flight.job, res, entry, session),
                Err(e) => self.set_status_msg(&e),
            }
            self.needs_redraw = true;
        }
    }

    pub(crate) fn drain_restarts(&mut self) {
        let mut i = 0;
        while i < self.restarts.len() {
            let outcome = match self.restarts[i].result.try_recv() {
                Ok(outcome) => outcome,
                Err(TryRecvError::Empty) => {
                    i += 1;
                    continue;
                }
                Err(TryRecvError::Disconnected) => RestartOutcome::KillFailed("restart worker stopped".into()),
            };
            let flight = self.restarts.remove(i);
            self.finish_restart(flight, outcome);
            self.needs_redraw = true;
        }
        self.drain_restores();
        self.drain_remote_adds();
    }

    fn finish_restart(&mut self, flight: RestartFlight, outcome: RestartOutcome) {
        // Drops this restart's exit marker. When the marker is already gone
        // the session's exit arrived and was suppressed, so a restart that
        // gives up must show the row as exited: it would otherwise look
        // live while nothing runs behind it.
        let unmark = |app: &mut App| {
            let Some(d) = &flight.daemon_uid else { return };
            if app.restart_suppressed_exit_uids.remove(d) {
                return;
            }
            for ws in &mut app.workspaces {
                for ts in &mut ws.sessions {
                    if ts.uid == flight.uid {
                        ts.session.exited = true;
                    }
                }
            }
        };
        let verb = if flight.was_live { "restart" } else { "revive" };
        let found = self.workspaces.iter().enumerate().find_map(|(wi, ws)| {
            ws.sessions.iter().position(|s| s.uid == flight.uid).map(|si| (wi, si))
        });
        match outcome {
            RestartOutcome::Unreachable => {
                unmark(self);
                self.set_status_msg(&format!(
                    "Host `{}` unreachable — can't {verb}",
                    flight.host_id.as_str()
                ));
            }
            RestartOutcome::KillFailed(e) => {
                unmark(self);
                self.set_status_msg(&format!("Restart kill failed: {e}"));
            }
            RestartOutcome::StillRunning => {
                // The kill may still land; that exit is a real one.
                unmark(self);
                self.set_status_msg("Killed the session but it hasn't exited yet — press A-R again");
            }
            RestartOutcome::ReviveFailed(e) => {
                unmark(self);
                self.note_revive_failure(&flight.uid, &e);
            }
            RestartOutcome::Revived => {
                self.revive_failed.remove(&flight.uid);
                let Some((wi, si)) = found else { return };
                {
                    let ts = &mut self.workspaces[wi].sessions[si];
                    ts.session.exited = false;
                    ts.set_status(SessionStatus::Idle);
                    ts.preserved_last_exit = None;
                }
                self.reattach_backoff.remove(&flight.uid);
                self.requeue_remote_reconnect(wi, si, "A-R revive");
                self.save_session_manifest();
                self.set_status_msg(&format!(
                    "Session {} on `{}` — reattaching…",
                    if flight.was_live { "restarted" } else { "revived" },
                    flight.host_id.as_str(),
                ));
            }
            RestartOutcome::ComingBack => {
                unmark(self);
                self.respawning.insert(flight.uid.clone(), Instant::now());
                self.expect_respawn(&flight.host_id, &flight.uid);
                self.set_status_msg(
                    "CM is still bringing this session back (transcript rotation or handoff) — it reattaches on its own",
                );
            }
            RestartOutcome::Killed => {
                let Some((wi, si)) = found else { return };
                self.respawn_local_session(wi, si, flight.was_live);
            }
            RestartOutcome::BackgroundOffer(offer) => {
                unmark(self);
                let Some((wi, si)) = found else { return };
                if !matches!(self.input_mode, InputMode::Normal) {
                    self.set_status_msg("Background recovery needs confirmation; close the current dialog and press A-R again");
                    return;
                }
                let identity = offer["identity"].clone();
                let label = sanitize_for_display(&self.workspaces[wi].sessions[si].label);
                let native = identity["native_session_id"].as_str().unwrap_or("not yet bound (fresh)");
                let path = sanitize_for_display(identity["canonical_cwd"].as_str().unwrap_or(""));
                let task = sanitize_for_display(identity["task_id"].as_str().unwrap_or("none"));
                let workspace = sanitize_for_display(identity["workspace_id"].as_str().unwrap_or(""));
                let pid = identity["background_pid"].as_u64().unwrap_or(0);
                let start = identity["background_start_time"].as_u64().unwrap_or(0);
                let prompt = format!("Stop Claude background work for {label} on `{}` and revive it? It may be active.\nCM UID: {}\nNative ID: {native}\nBackground PID: {pid}; start: {start}\nTask: {task}\nWorkspace: {workspace} ({path})", sanitize_for_display(flight.host_id.as_str()), sanitize_for_display(&flight.uid));
                if !background_prompt_fits(&prompt, self.last_term_size) {
                    self.set_status_msg("Terminal is too small to show the full recovery identity; enlarge it and press A-R again");
                    return;
                }
                self.input_mode = InputMode::Confirm {
                    prompt: prompt.clone(),
                    action: ConfirmAction::BackgroundRecovery { uid: flight.uid, host: flight.host_id, identity, prompt },
                };
            }
            RestartOutcome::RecoveryNeedsAttention(message) => {
                unmark(self);
                self.set_status_msg(&format!("Recovery needs attention: {message}"));
            }
            RestartOutcome::RecoveryUncertain(message) => {
                self.revive_failed.insert(flight.uid.clone(), message.clone());
                unmark(self);
                self.set_status_msg(&format!("Recovery needs attention: {message}"));
            }
            RestartOutcome::RecoveryCleared(message) => {
                unmark(self);
                self.set_status_msg(&format!("{message}; inspect the session before pressing A-R again"));
            }
            RestartOutcome::RecoveryRecord(record) => {
                unmark(self);
                let Some((wi, si)) = found else { return };
                if !matches!(self.input_mode, InputMode::Normal) {
                    self.set_status_msg("Recovery needs reconciliation; close the current dialog and press A-R again");
                    return;
                }
                let identity = record["identity"].clone();
                let label = sanitize_for_display(&self.workspaces[wi].sessions[si].label);
                let native = identity["native_session_id"].as_str().unwrap_or("not yet bound (fresh)");
                let path = sanitize_for_display(identity["canonical_cwd"].as_str().unwrap_or(""));
                let task = sanitize_for_display(identity["task_id"].as_str().unwrap_or("none"));
                let workspace = sanitize_for_display(identity["workspace_id"].as_str().unwrap_or(""));
                let phase = sanitize_for_display(record["status"].as_str().unwrap_or("unknown"));
                let pid = identity["background_pid"].as_u64().unwrap_or(0);
                let start = identity["background_start_time"].as_u64().unwrap_or(0);
                let prompt = format!("Reconcile interrupted recovery for {label} on `{}`? This checks whether background work stopped; it does not stop or revive it.\nCM UID: {}\nPhase: {phase}\nNative ID: {native}\nBackground PID: {pid}; start: {start}\nTask: {task}\nWorkspace: {workspace} ({path})", sanitize_for_display(flight.host_id.as_str()), sanitize_for_display(&flight.uid));
                if !background_prompt_fits(&prompt, self.last_term_size) {
                    self.set_status_msg("Terminal is too small to show the full recovery identity; enlarge it and press A-R again");
                    return;
                }
                self.input_mode = InputMode::Confirm {
                    prompt: prompt.clone(),
                    action: ConfirmAction::ReconcileBackground { uid: flight.uid, host: flight.host_id, identity, prompt },
                };
            }
        }
    }

    /// A-O on a remote workspace: revive every tombstone on its host from a
    /// worker. The tombstones stay listed until their revive answers.
    pub(super) fn start_remote_restore(&mut self, wi: usize) {
        let ws = &self.workspaces[wi];
        if self.restores.iter().any(|f| f.ws_id == ws.id) {
            self.set_status_msg("A restore of this workspace is already in progress");
            return;
        }
        let host_id = ws.host_id.clone();
        let mut jobs = Vec::new();
        let mut errors = Vec::new();
        for tomb in &ws.tombstones {
            match tomb.worktree_path.clone().or_else(|| ws.worktree_path.clone()) {
                Some(worktree) => jobs.push(RestoreJob {
                    uid: tomb.uid.clone(),
                    label: tomb.label.clone(),
                    entry: super::lifecycle::remote_revive_entry(tomb, &host_id),
                    worktree: worktree.to_string_lossy().into_owned(),
                }),
                None => errors.push(format!("{}: no worktree", tomb.label)),
            }
        }
        let total = ws.tombstones.len();
        let ws_id = ws.id.clone();
        let uids: Vec<String> = jobs.iter().map(|job| job.uid.clone()).collect();
        let pool = std::sync::Arc::clone(&self.host_pool);
        let (tx, result) = mpsc::channel();
        let job_host = host_id.clone();
        let job_ws = ws_id.clone();
        let spawned = std::thread::Builder::new()
            .name("cm-restore".into())
            .spawn(move || {
                let Some(socket) = pool.for_host(&job_host).ok().and_then(|h| h.socket_path()) else {
                    let _ = tx.send(Err("unreachable".into()));
                    return;
                };
                let token = pool.operator_token_for(&job_host);
                let done = jobs
                    .into_iter()
                    .map(|job| {
                        let r = crate::client_session::rpc_session_revive(
                            &socket, &token, &job.entry, &job_ws, &job.worktree,
                        )
                        .map_err(|e| {
                            eprintln!("cm-tui: A-O revive of {} on {} failed: {e:#}", job.uid, job_host.as_str());
                            e.to_string()
                        });
                        (job, r)
                    })
                    .collect();
                let _ = tx.send(Ok(done));
            });
        if let Err(e) = spawned {
            self.set_status_msg(&format!("Could not start the restore worker: {e}"));
            return;
        }
        self.restores.push(RestoreFlight { ws_id, uids, host_id: host_id.clone(), total, errors, result });
        self.set_status_msg(&format!(
            "Restoring {total} session{} on `{}`…",
            if total == 1 { "" } else { "s" },
            host_id.as_str()
        ));
    }

    fn drain_restores(&mut self) {
        let mut i = 0;
        while i < self.restores.len() {
            let result = match self.restores[i].result.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => {
                    i += 1;
                    continue;
                }
                Err(TryRecvError::Disconnected) => Err("restore worker stopped".into()),
            };
            let flight = self.restores.remove(i);
            self.finish_remote_restore(flight, result);
            self.needs_redraw = true;
        }
    }

    fn finish_remote_restore(
        &mut self,
        flight: RestoreFlight,
        result: Result<Vec<(RestoreJob, Result<(), String>)>, String>,
    ) {
        let host = flight.host_id.as_str().to_owned();
        let done = match result {
            Ok(done) => done,
            Err(_) => {
                self.set_status_msg(&format!(
                    "Host `{host}` unreachable — nothing restored; the closed sessions stay listed"
                ));
                return;
            }
        };
        let Some(wi) = self.workspaces.iter().position(|w| w.id == flight.ws_id) else { return };
        let mut errors = flight.errors;
        let mut restored = 0;
        let mut reopen_tasks: Vec<String> = Vec::new();
        for (job, r) in done {
            match r {
                Ok(()) => {
                    self.workspaces[wi].tombstones.retain(|t| t.uid != job.uid);
                    if let Some(task) = &job.entry.task_id {
                        reopen_tasks.push(task.clone());
                    }
                    self.pending_remote_reattach
                        .push(super::remote::PendingRemoteReattach::new(flight.ws_id.clone(), job.entry));
                    restored += 1;
                }
                Err(e) => errors.push(format!("{}: {e}", job.label)),
            }
        }
        // A restored worker's task is work in progress again; a done task
        // would have the terminal-task sweep close the session once more.
        // (`self.tasks` keeps only non-terminal tasks, so reopen's bound-done
        // flip cannot see these.)
        reopen_tasks.sort();
        reopen_tasks.dedup();
        for task in reopen_tasks {
            let active = self.tasks.iter().any(|t| {
                t.task_id.as_deref() == Some(task.as_str()) && !matches!(t.api_status, TaskStatus::Done)
            });
            if !active {
                let mut fields = HashMap::new();
                fields.insert("status".to_string(), serde_json::Value::String("running".into()));
                self.backend.update_task(task.clone(), fields);
                self.planning.mark_task_running_by_id(&task);
            }
        }
        self.save_session_manifest();
        let mut msg = format!(
            "Restored {}/{} session{} on `{host}` — reattaching…",
            restored,
            flight.total,
            if flight.total == 1 { "" } else { "s" },
        );
        if !errors.is_empty() {
            msg.push_str(&format!(" · failed (still listed, A-O retries): {}", errors.join("; ")));
        }
        self.set_status_msg(&msg);
    }
}
