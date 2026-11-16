//! Bounded same-pane recovery for a lost Codex provider continuation.
//! The daemon is the sole owner (no viewer is required). Persist the attempt
//! BEFORE typing native /fork; an uncertain write is never replayed on restart.
use crate::agent_state::{AgentState, Source, State, TurnStatus};
use crate::state::DaemonState;
use crate::workflow::pty_tracker::PtyModeTracker;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Instant;

pub(crate) fn failed_boundary(agent: &AgentState) -> bool {
    agent.source == Source::Relay
        && agent.state == State::Errored
        && agent.last_turn.status == Some(TurnStatus::Failed)
        && agent.detail.retrying != Some(true)
        && agent.detail.input_pending != Some(true)
        && agent.background.complete
        && agent.background.jobs.is_empty()
        && agent.background.crons.is_empty()
}

fn recoverable(agent: &AgentState) -> bool {
    failed_boundary(agent)
        && agent.detail.error_kind.as_deref() == Some("previous_response_not_found")
}

fn active() -> &'static Mutex<HashSet<String>> {
    static ACTIVE: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    ACTIVE.get_or_init(Default::default)
}

pub(crate) fn in_progress(uid: &str) -> bool {
    active().lock().unwrap_or_else(|p| p.into_inner()).contains(uid)
}

struct Permit { uid: String, held: bool }
impl Drop for Permit {
    fn drop(&mut self) {
        if self.held { crate::rotation::unhold(&self.uid, crate::rotation::HoldOwner::CodexRecovery); }
        active().lock().unwrap_or_else(|p| p.into_inner()).remove(&self.uid);
    }
}

fn fork_of(source: &str, target: &str) -> bool {
    fn metadata(path: &str) -> Option<serde_json::Value> {
        let file = std::fs::File::open(path).ok()?;
        let mut row = String::new();
        std::io::BufReader::new(file.take(1024 * 1024)).read_line(&mut row).ok()?;
        let value: serde_json::Value = serde_json::from_str(&row).ok()?;
        (value["type"] == "session_meta").then(|| value["payload"].clone())
    }
    let (Some(from), Some(to)) = (metadata(source), metadata(target)) else { return false; };
    from["id"].as_str().is_some_and(|id|
        to["forked_from_id"].as_str().or_else(|| to["forkedFromId"].as_str()) == Some(id))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Receipt {
    version: u32,
    source_transcript: String,
    claimed_at: f64,
    phase: String,
    target_transcript: Option<String>,
}

fn receipt_path(base: &Path, uid: &str) -> Option<PathBuf> {
    if uid.is_empty() || uid.len() > 128
        || !uid.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    Some(base.with_file_name(format!("daemon-codex-recovery-{uid}.json")))
}

fn load(path: &Path) -> std::io::Result<Option<Receipt>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let receipt: Receipt = serde_json::from_slice(&bytes)?;
            if receipt.version != 1 || !receipt.claimed_at.is_finite() {
                return Err(std::io::Error::other("unsupported Codex recovery receipt"));
            }
            Ok(Some(receipt))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn save(path: &Path, receipt: &Receipt) -> std::io::Result<()> {
    crate::state::write_json_atomic(path, &serde_json::to_string(receipt)?, true)
}

/// Only a completed, post-attempt turn rearms recovery. Merely changing thread,
/// retrying, reconnecting, or getting another error cannot fork repeatedly.
fn successful_since(agent: &AgentState, receipt: &Receipt) -> bool {
    agent.source == Source::Relay
        && agent.last_turn.status == Some(TurnStatus::Completed)
        && agent.last_turn.ended_at.is_some_and(|at| at > receipt.claimed_at)
}

/// Older live relays expose only `other`/`serverOverloaded`. For those,
/// require the exact terminal error immediately before the current composer,
/// in the rendered screen (not raw replay/history). Never match arbitrary prose.
fn legacy_error_screen(screen: &str) -> bool {
    let compact: String = screen.chars().filter(|c| !c.is_whitespace()).collect();
    let Some((before, _)) = compact.rsplit_once('›') else { return false; };
    let Some((_, error)) = before.rsplit_once('■') else { return false; };
    matches!(error,
        "streamdisconnectedbeforecompletion:Previousresponsewasnotfound;retrywithoutprevious_response_id."
        | "Previousresponsewasnotfound;retrywithoutprevious_response_id.")
}

struct Target {
    uid: String,
    receipt: PathBuf,
    transcript: String,
    agent: AgentState,
    input: crate::session::InputHandle,
    fanout: Arc<crate::session::PtyByteFanout>,
    cols: u16,
    rows: u16,
    captured: Instant,
    messaging_root: PathBuf,
    state: Weak<Mutex<DaemonState>>,
    _permit: Option<Permit>,
}

/// Called by the existing single agent-state ticker. No I/O or writes while
/// DaemonState is locked. Queued workers hold only handles to that exact process.
pub(crate) fn tick(state: &Arc<Mutex<DaemonState>>) {
    let targets: Vec<_> = {
        let st = state.lock().unwrap_or_else(|p| p.into_inner());
        if st.restarting { return; }
        let Some(base) = &st.daemon_sessions_path else { return; };
        st.sessions.iter().filter_map(|(uid, s)| {
            if s.session_type != "codex" || s.last_exit.kernel_set()
                || s.workflow_run_id.is_some() || s.continuous_task_id.is_some()
                || crate::rotation::in_progress(uid) || crate::handoff::in_progress(uid)
                || crate::claude_recovery::admission_blocked(uid)
                || in_progress(uid)
            { return None; }
            let agent = crate::agent_state::current(s);
            // Successful turns also visit the receipt to rearm it. Idle after
            // a fork visits it to verify the new binding and queue continuation.
            if agent.source != Source::Relay { return None; }
            // All lifecycle admissions share DaemonState -> registry order.
            // Reserve before dropping that mutex, so handoff/rotation cannot
            // replace this process while the recovery worker types into it.
            let receipt = receipt_path(base, uid)?;
            let transcript = s.transcript_path.clone()?;
            active().lock().unwrap_or_else(|p| p.into_inner()).insert(uid.clone());
            Some(Target {
                uid: uid.clone(), receipt,
                transcript, agent,
                input: s.input_handle(), fanout: Arc::clone(&s.fanout),
                cols: s.last_cols, rows: s.last_rows, captured: Instant::now(),
                messaging_root: st.messaging_root.clone(),
                state: Arc::downgrade(state), _permit: Some(Permit { uid: uid.clone(), held: false }),
            })
        }).collect()
    };
    for target in targets {
        if let Err(e) = step(target) {
            eprintln!("cm-daemon: Codex recovery skipped: {e}");
        }
    }
}

fn step(mut target: Target) -> std::io::Result<()> {
    if let Some(mut receipt) = load(&target.receipt)? {
        if matches!(receipt.phase.as_str(), "claimed" | "submitted" | "write_failed_or_uncertain")
            && target.transcript != receipt.source_transcript
            && fork_of(&receipt.source_transcript, &target.transcript)
        {
            // A fresh thread binding is the receipt, never PTY repaint alone.
            receipt.phase = "forked".into();
            receipt.target_transcript = Some(target.transcript.clone());
            save(&target.receipt, &receipt)?;
            eprintln!("cm-daemon: native Codex recovery fork verified for {}", target.uid);
        }
        if receipt.phase == "forked" && target.agent.state == State::Idle
            && target.agent.background.complete && target.agent.background.jobs.is_empty()
            && !successful_since(&target.agent, &receipt)
        {
            // Durable native notification, not a duplicate of the failed prompt.
            // The agent must reconcile side effects before continuing its task.
            let id = format!("codex-recovery-{}", (receipt.claimed_at * 1e6) as u64);
            let marker = format!("[cm-codex-recovery {id}]");
            let body = format!("{marker} CM automatically ran native /fork after a lost provider continuation. Your CM identity, task and workspace are unchanged. Reconcile the retained conversation, board, inbox and any in-flight work before continuing the interrupted request. Verify prior side effects before repeating them.");
            crate::notifications::publish(&target.messaging_root, &target.uid, &id,
                "codex-recovery", &body, &marker)?;
            receipt.phase = "resume_queued".into();
            save(&target.receipt, &receipt)?;
        }
        if successful_since(&target.agent, &receipt) {
            // Persist rearming before a later failure; preserve the receipt.
            if receipt.phase != "rearmed" {
                receipt.phase = "rearmed".into();
                save(&target.receipt, &receipt)?;
            }
        }
        if receipt.phase != "rearmed" { return Ok(()); }
    }
    if !failed_boundary(&target.agent) { return Ok(()); }
    if !matches!(target.agent.detail.error_kind.as_deref(),
        Some("previous_response_not_found" | "other" | "serverOverloaded")) { return Ok(()); }
    // Unchanged errors can sit for hours. Parse a replay only on new output or
    // a new failed turn, not a MiB per errored session on every one-second tick.
    static SCANNED: OnceLock<Mutex<HashMap<String, (u64, u64, String, Option<String>)>>> = OnceLock::new();
    let cursor = target.fanout.snapshot_since(Some(u64::MAX)).cursor;
    let key = (cursor, target.agent.turn_seq, target.transcript.clone(), target.agent.detail.error_kind.clone());
    {
        let scanned = SCANNED.get_or_init(Default::default).lock().unwrap_or_else(|p| p.into_inner());
        if scanned.get(&target.uid) == Some(&key) { return Ok(()); }
    }
    let snapshot = target.fanout.snapshot_since(None);
    if snapshot.closed { return Ok(()); }
    // Startup keyboard escapes can be older than the retained replay ring.
    // The fanout tracks their live state independently and restores it across
    // brain adoption. A later output change still cancels the guarded write.
    let keyboard = target.fanout.keyboard().0;
    let mut screen = PtyModeTracker::with_size(target.cols.max(1) as usize, target.rows.max(1) as usize);
    screen.feed(&snapshot.bytes, Instant::now());
    let mode = keyboard.or_else(|| screen.composer_ready().then(|| screen.term_mode()));
    if mode.is_none() || !screen.codex_recovery_composer_ready()
        || (!recoverable(&target.agent)
            && !(matches!(target.agent.detail.error_kind.as_deref(), Some("other" | "serverOverloaded"))
                && legacy_error_screen(&screen.visible_text())))
    {
        // Cache only screen-dependent refusals. Quiet windows and filesystem
        // failures can clear without new output and must remain eligible.
        let mut scanned = SCANNED.get_or_init(Default::default).lock().unwrap_or_else(|p| p.into_inner());
        if scanned.len() >= 512 { scanned.clear(); }
        scanned.insert(target.uid.clone(), key);
        return Ok(());
    }
    // Do not wait out a draft: leave the error visible for the operator.
    if target.input.operator_quiet_for().is_some_and(|gap| gap.as_secs() < 2) { return Ok(()); }
    let mut receipt = Receipt { version: 1, source_transcript: target.transcript,
        claimed_at: crate::agent_state::unix_now(), phase: "claimed".into(), target_transcript: None };
    if let Some(state) = target.state.upgrade() {
        let st = state.lock().unwrap_or_else(|p| p.into_inner());
        if st.restarting || !crate::rotation::hold(&st, &target.uid, crate::rotation::HoldOwner::CodexRecovery) {
            return Ok(());
        }
        if let Some(permit) = target._permit.as_mut() { permit.held = true; }
    }
    save(&target.receipt, &receipt)?;
    let path = target.receipt.clone();
    let uid = target.uid.clone();
    let enter = crate::workflow::pty_tracker::enter_bytes_for_mode(mode.unwrap()).to_vec();
    let spawn_path = path.clone();
    let mut spawn_failure = receipt.clone();
    let spawned = std::thread::Builder::new().name(format!("cm-codex-recovery-{uid}")).spawn(move || {
        let _permit = target._permit;
        eprintln!("cm-daemon: Codex continuation lost; trying native /fork once for {uid}");
        let result = (|| {
            let Some(_native_guard) = crate::notifications::recovery_input_guard(&target.messaging_root,
                &uid, target.agent.last_turn.ended_at.unwrap_or(0.0))? else { return Ok(false); };
            target.input.write_codex_recovery_fork(target.captured,
                target.agent.turn_seq, target.agent.last_turn.ended_at,
                target.agent.detail.error_kind.as_deref(), &enter, || {
                    let now = target.fanout.snapshot_since(Some(snapshot.cursor));
                    !now.closed && now.cursor == snapshot.cursor
                })
        })();
        receipt.phase = match result {
            Ok(true) => "submitted",
            Ok(false) => "cancelled_input_or_state_changed",
            Err(_) => "write_failed_or_uncertain",
        }.into();
        if let Err(e) = save(&path, &receipt) {
            eprintln!("cm-daemon: Codex recovery receipt update failed for {uid}: {e}");
        }
        eprintln!("cm-daemon: Codex recovery {uid}: {} (no automatic command retry)", receipt.phase);
        // Retain the lifecycle reservation until the fork binds, or a bounded
        // timeout. A restart can still proceed; the durable receipt forbids replay.
        let until = Instant::now() + std::time::Duration::from_secs(30);
        while receipt.phase == "submitted" && Instant::now() < until {
            let Some(state) = target.state.upgrade() else { break; };
            let st = state.lock().unwrap_or_else(|p| p.into_inner());
            if st.restarting || st.sessions.get(&uid).is_none_or(|s|
                s.last_exit.kernel_set() || s.transcript_path.as_deref() != Some(&receipt.source_transcript))
            { break; }
            drop(st);
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    });
    if let Err(e) = spawned {
        spawn_failure.phase = "worker_unavailable".into();
        save(&spawn_path, &spawn_failure)?;
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn failed() -> AgentState {
        serde_json::from_value(json!({"state":"errored","since":100.0,
            "detail":{"error_kind":"previous_response_not_found"}, "source":"relay",
            "observed_at":100.0,"turn_seq":2,"last_turn":{"ended_at":100.0,"status":"failed"},
            "background":{"complete":true}})).unwrap()
    }

    #[test]
    fn only_exact_terminal_error_without_background_work_is_recoverable() {
        let expected = failed();
        assert!(recoverable(&expected));
        for kind in ["serverOverloaded", "other", "usageLimitExceeded", "rate_limit", "api_error"] {
            let mut a = expected.clone(); a.detail.error_kind = Some(kind.into());
            assert!(!recoverable(&a), "{kind}");
        }
        let mut a = expected.clone(); a.source = Source::Hooks; assert!(!recoverable(&a));
        let mut a = expected.clone(); a.state = State::Working; assert!(!recoverable(&a));
        let mut a = expected.clone(); a.detail.retrying = Some(true); assert!(!recoverable(&a));
        let mut a = expected.clone(); a.detail.input_pending = Some(true); assert!(!recoverable(&a));
        let mut a = expected.clone(); a.background.complete = false; assert!(!recoverable(&a));
        let mut a = expected; a.last_turn.status = Some(TurnStatus::Completed); assert!(!recoverable(&a));
    }

    #[test]
    fn receipt_survives_restart_and_only_new_success_rearms() {
        let d = tempfile::tempdir().unwrap(); let p = d.path().join("receipt.json");
        let r = Receipt { version:1, source_transcript:"old".into(), claimed_at:101.0,
            phase:"claimed".into(), target_transcript:None };
        save(&p, &r).unwrap();
        let loaded = load(&p).unwrap().unwrap();
        let mut a = failed(); assert!(!successful_since(&a, &loaded));
        a.last_turn.ended_at = Some(102.0); assert!(!successful_since(&a, &loaded));
        a.last_turn.status = Some(TurnStatus::Completed); assert!(successful_since(&a, &loaded));
        a.last_turn.ended_at = Some(100.0); assert!(!successful_since(&a, &loaded));
        std::fs::write(&p, "broken").unwrap(); assert!(load(&p).is_err());
        assert!(receipt_path(&p, "../../bad").is_none());
    }

    #[test]
    fn old_relays_require_exact_rendered_error_at_the_composer() {
        let error = "■ stream disconnected before completion: Previous response was not found; retry without previous_response_id.";
        assert!(legacy_error_screen(&format!("{error}\n› Ask Codex to do anything")));
        for screen in [format!("{error}\n■ server overloaded\n›"),
            format!("quoted {error}\nassistant response\n›"),
            "previous_response_not_found\n›".into(),
            "■ Rate limit exceeded\n›".into()] {
            assert!(!legacy_error_screen(&screen), "{screen}");
        }
    }

    fn fixture(dir: &Path) -> (Target, Arc<Mutex<Vec<(Instant, Vec<u8>)>>>) {
        let now = crate::agent_state::unix_now();
        let (input, writes) = crate::session::InputHandle::test_handle_capturing();
        let cell = input.test_agent_state();
        {
            let mut c = cell.lock().unwrap();
            c.inputs.relay = Some(serde_json::from_value(json!({
                "backend_connected":true,"foreground":"idle","observed_at":now,
                "turn_seq":1,"turn_started_at":now-10.0,
                "last_turn":{"ended_at":now-2.0,"status":"failed"},
                "error_kind":"other","background":{"complete":true}
            })).unwrap());
            c.inputs.background.complete = true;
        }
        let agent = cell.lock().unwrap().recompute(now);
        assert!(failed_boundary(&agent));
        let fanout = Arc::new(crate::session::PtyByteFanout::new(4096));
        fanout.push("\x1b[>1u\x1b[?25h\x1b[H■ stream disconnected before completion: Previous response was not found; retry without previous_response_id.\r\n› Ask Codex to do anything\x1b[2;3H".as_bytes());
        (Target { uid:"test-1".into(), receipt:dir.join("receipt.json"),
            transcript:"old.jsonl".into(), agent, input, fanout,
            cols:160, rows:24,captured:Instant::now(),messaging_root:dir.into(),
            state:Weak::new(),_permit:None }, writes)
    }

    #[test]
    fn exact_legacy_error_forks_once_and_survives_duplicate_ticks() {
        let d = tempfile::tempdir().unwrap();
        let (target, writes) = fixture(d.path());
        step(target).unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while load(&d.path().join("receipt.json")).unwrap().unwrap().phase == "claimed" {
            assert!(Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(load(&d.path().join("receipt.json")).unwrap().unwrap().phase, "submitted");
        let got: Vec<u8> = writes.lock().unwrap().iter().flat_map(|(_, b)| b.clone()).collect();
        assert_eq!(got, b"/fork\x1b[13u");
        let (again, duplicate_writes) = fixture(d.path());
        step(again).unwrap();
        assert!(duplicate_writes.lock().unwrap().is_empty());
        // Fork identity confirmed -> one native resume envelope, preserving uid.
        let (mut forked, _) = fixture(d.path());
        let old = d.path().join("old.jsonl"); let fresh = d.path().join("fresh.jsonl");
        std::fs::write(&old, json!({"type":"session_meta","payload":{"id":"source"}}).to_string()).unwrap();
        std::fs::write(&fresh,json!({"type":"session_meta","payload":{"id":"new","forked_from_id":"source"}}).to_string()).unwrap();
        let mut r = load(&d.path().join("receipt.json")).unwrap().unwrap();
        r.source_transcript = old.to_string_lossy().into_owned();
        r.phase = "claimed".into(); // crash after command accepted, before submitted was saved
        save(&d.path().join("receipt.json"), &r).unwrap();
        forked.transcript = fresh.to_string_lossy().into_owned(); forked.agent.state = State::Idle;
        step(forked).unwrap();
        let r = load(&d.path().join("receipt.json")).unwrap().unwrap();
        assert_eq!(r.phase, "resume_queued");
        assert_eq!(r.target_transcript.as_deref(), Some(fresh.to_str().unwrap()));
        let (events, _) = crate::notifications::snapshot(d.path(), "test-1").unwrap();
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn evicted_keyboard_startup_survives_brain_adoption() {
        for adopted in [false, true] {
            let d = tempfile::tempdir().unwrap();
            let (mut target, writes) = fixture(d.path());
            target.uid = format!("evicted-keyboard-{adopted}");
            let screen = b"\x1b[2J\x1b[H\x1b[?25h\xe2\x96\xa0 Previous response was not found; retry without previous_response_id.\r\n\xe2\x80\xba Ask Codex to do anything\x1b[2;3H";
            target.fanout.push(&vec![b' '; 4096]);
            target.fanout.push(screen);
            let ring = target.fanout.snapshot_since(None).bytes;
            let mut replay = PtyModeTracker::with_size(160, 24);
            replay.feed(&ring, Instant::now());
            assert!(!replay.composer_ready(), "startup mode was evicted");
            assert!(replay.codex_recovery_composer_ready());
            if adopted {
                let (_, keyboard) = target.fanout.keyboard();
                target.fanout = Arc::new(crate::session::PtyByteFanout::new(4096));
                target.fanout.seed_adopted(Some(&ring), Some(keyboard));
            }
            step(target).unwrap();
            let deadline = Instant::now() + std::time::Duration::from_secs(5);
            while load(&d.path().join("receipt.json")).unwrap().unwrap().phase == "claimed" {
                assert!(Instant::now() < deadline);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            let got: Vec<u8> = writes.lock().unwrap().iter().flat_map(|(_, b)| b.clone()).collect();
            assert_eq!(got, b"/fork\x1b[13u");
        }
    }

    #[test]
    fn unknown_keyboard_without_startup_evidence_is_skipped() {
        let d = tempfile::tempdir().unwrap();
        let (mut target, writes) = fixture(d.path());
        target.uid = "unknown-keyboard".into();
        let ring = target.fanout.snapshot_since(None).bytes;
        target.fanout = Arc::new(crate::session::PtyByteFanout::new(4096));
        target.fanout.seed_adopted(Some(&ring[b"\x1b[>1u".len()..]), None);
        assert!(target.fanout.keyboard().0.is_none());
        step(target).unwrap();
        assert!(writes.lock().unwrap().is_empty());
        assert!(!d.path().join("receipt.json").exists());
    }

    #[test]
    fn human_drafts_changed_screen_and_new_turn_block_the_writer() {
        let d = tempfile::tempdir().unwrap();
        for condition in 0..3 {
            let (target, writes) = fixture(d.path());
            if condition == 0 { target.input.stamp_operator_input_at(Instant::now()); }
            if condition == 2 { target.input.test_agent_state().lock().unwrap().note_input(crate::agent_state::unix_now()); }
            assert!(!target.input.write_codex_recovery_fork(target.captured, target.agent.turn_seq,
                target.agent.last_turn.ended_at, Some("other"), b"\r", || condition != 1).unwrap());
            assert!(writes.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn cursor_composer_check_rejects_real_drafts_and_old_prompt_rows() {
        for (row, col, expected) in [("› Ask Codex to do anything",3,true),
            ("› ",3,true),("› continue",3,false),("› Ask Codex to do anything",8,false)] {
            let mut screen = PtyModeTracker::with_size(80,24);
            screen.feed(format!("\x1b[?25h\x1b[H{row}\x1b[1;{col}H").as_bytes(),Instant::now());
            assert_eq!(screen.codex_recovery_composer_ready(),expected,"{row}, {col}");
        }
    }

    #[test]
    fn native_delivery_already_in_flight_is_not_forked() {
        let d = tempfile::tempdir().unwrap();
        crate::notifications::publish(d.path(), "test-guard", "notice", "chat", "[notice]", "[notice]").unwrap();
        let dir = crate::notifications::directory(d.path(), "test-guard");
        let path = std::fs::read_dir(dir).unwrap().flatten().map(|e|e.path())
            .find(|p|p.extension().is_some_and(|s|s == "json")).unwrap();
        let mut event: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        event["status"] = json!("submitting");
        std::fs::write(&path,event.to_string()).unwrap();
        assert!(crate::notifications::recovery_input_guard(d.path(), "test-guard",f64::MAX).unwrap().is_none());
        event["status"] = json!("observed");
        std::fs::write(&path,event.to_string()).unwrap();
        assert!(crate::notifications::recovery_input_guard(d.path(), "test-guard",0.0).unwrap().is_some());
    }

    #[test]
    fn persistence_failure_does_not_cache_an_eligible_screen() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let (mut target, _) = fixture(d.path()); target.uid = "persist-retry".into();
        std::fs::set_permissions(d.path(),std::fs::Permissions::from_mode(0o500)).unwrap();
        assert!(step(target).is_err());
        std::fs::set_permissions(d.path(),std::fs::Permissions::from_mode(0o700)).unwrap();
        let (mut target, _) = fixture(d.path()); target.uid = "persist-retry".into();
        step(target).unwrap();
        assert!(load(&d.path().join("receipt.json")).unwrap().is_some());
        let until = Instant::now() + std::time::Duration::from_secs(5);
        while load(&d.path().join("receipt.json")).unwrap().unwrap().phase == "claimed" {
            assert!(Instant::now() < until);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

}
