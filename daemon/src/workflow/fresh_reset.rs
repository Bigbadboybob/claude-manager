//! Daemon-side fresh-context respawn primitive (Phase 2 of
//! `doc/daemon-side-workflow-orchestration.md`).
//!
//! A `Context::Fresh` role (e.g. the feedback reviewer) is reset between
//! activations so its agent starts from a clean transcript. The former TUI
//! controller did this in `reset_fresh_session`: queue `/clear`
//! (gated on PTY-quiet), reset the role's `MessageBaseline`, clear the bound
//! `current_session_id`, and snapshot the transcript dir so the new file
//! `/clear` rotates to can be picked up. The new sid is rebound LATER by the
//! TUI's content correlator (`resolve_pending_deliveries`) — which never runs
//! headlessly. This module ports both halves into the daemon:
//!
//!   1. [`reset_fresh_role`] — the reset op, gated on PTY-quiet via the Phase 1
//!      tracker: snapshot the pre-`/clear` transcript dir, send `/clear` with
//!      the live-mode Enter encoding, then (under flock) reset the role
//!      baseline to zero and clear `current_session_id`.
//!   2. [`try_rebind`] — daemon-side sid discovery: a listing diff of the
//!      transcript dir vs the pre-`/clear` snapshot finds the new file `/clear`
//!      rotated to and binds it to `current_session_id`. This is the headless
//!      replacement for the TUI content correlator.
//!
//! ## Ordering (why discovery runs AFTER delivery — see design doc §C)
//!
//! Claude rotates its transcript only once the cleared session produces a new
//! turn, which only happens after the activation prompt is delivered. So
//! discovery cannot run before delivery — there is no new file to find yet.
//! The reset therefore clears `current_session_id` and leaves it unbound; the
//! poller keeps the role's idle gate inert (`SkipReason::NoTranscriptId`) while
//! it is unbound, so it never counts turns against the stale pre-`/clear`
//! transcript. A fresh role's post-`/clear` baseline is just zero (the new
//! transcript is empty) and its activation prompt does not read its own new
//! transcript, so delivering with the sid still pending is safe.
//!
//! ## Why discovery never rebinds to the pre-`/clear` transcript
//!
//! The snapshot is taken BEFORE `/clear`, so the old transcript's stem is in
//! it. [`crate::transcript_detect::detect_new_claude_id`] /
//! `detect_new_codex_id` exclude every stem in the snapshot, so the old file
//! can never win the diff. Discovery is idempotent: re-running it with the same
//! snapshot re-finds the same new file, and [`try_rebind`] short-circuits once
//! `current_session_id` is already bound (restart-safe).
//!
//! Phase 2 is exercised directly (reset → simulate delivery+rotation →
//! discover); wiring it into the transition/delivery path is Phase 3.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::transcript_detect;
use crate::workflow::pty_tracker::{self, PtyModeTracker};
use crate::workflow::run::{self, MessageBaseline, PersistError, WorkflowRun};
use crate::workflow::toml_schema::Engine;

/// Default PTY-quiet window before `/clear` is sent. Matches the TUI's
/// `reset_fresh_session` `wait_for_quiet(... quiet = 2s ...)`.
pub const DEFAULT_QUIET_WINDOW: Duration = Duration::from_secs(2);

/// The pre-`/clear` transcript-dir snapshot — the rebind key listing-diff
/// discovery diffs against. The old (pre-`/clear`) transcript stem is included,
/// so discovery can never rebind to it. The caller holds this across the
/// reset → deliver → discover sequence; Phase 3 persists it on the durable
/// pending-activation record.
pub type PreClearSnapshot = Vec<String>;

/// Snapshot the existing transcript stems for `engine` under `worktree` BEFORE
/// `/clear` rotates the file. Engine-aware (Claude project dir vs Codex
/// date-bucketed tree).
pub fn snapshot_pre_clear(engine: Engine, worktree: &Path) -> PreClearSnapshot {
    match engine {
        Engine::ClaudeCode => transcript_detect::snapshot_claude_transcript_ids(worktree),
        Engine::Codex => transcript_detect::snapshot_codex_transcript_ids(worktree),
    }
}

/// Listing-diff discovery: the newest transcript stem under `worktree` that is
/// NOT in `snapshot`. Returns `None` until `/clear` has rotated and the new
/// turn's file appears (inherently post-delivery). Idempotent — re-finds the
/// same stem on a repeat call.
pub fn discover_rebind(engine: Engine, worktree: &Path, snapshot: &[String]) -> Option<String> {
    match engine {
        Engine::ClaudeCode => transcript_detect::detect_new_claude_id(worktree, snapshot),
        Engine::Codex => transcript_detect::detect_new_codex_id(worktree, snapshot),
    }
}

/// True once the role's `current_session_id` is bound, i.e. the rebind has
/// landed. While this is false the poller skips the role's idle gate with
/// `SkipReason::NoTranscriptId`, so it never fires against the stale
/// pre-`/clear` transcript.
pub fn rebind_landed(run: &WorkflowRun, role: &str) -> bool {
    run.role_sessions
        .get(role)
        .map_or(false, |b| b.current_session_id.is_some())
}

/// Apply the fresh-reset run-state mutation under flock: reset the role's
/// `MessageBaseline` to zero (so the resolver slices that role's messages from
/// index 0) and clear `current_session_id` (so the idle gate goes inert).
/// Mirrors the former TUI controller's `RoleResetMutations` / `reset_fresh_session`.
pub fn apply_reset_state(run_id: &str, role: &str) -> Result<WorkflowRun, PersistError> {
    let role = role.to_string();
    run::modify(run_id, move |r| {
        r.role_baselines
            .insert(role.clone(), MessageBaseline::default());
        if let Some(b) = r.role_sessions.get_mut(&role) {
            b.current_session_id = None;
        }
    })
}

/// Write the `/clear` BODY to the owned session's PTY via `write`, and RETURN
/// the mode-correct Enter keystroke bytes (Phase 1 encoding) WITHOUT sending
/// them. The body stays raw (single-line slash command — `format_body_for_delivery`
/// does not bracket-paste-wrap it).
///
/// IMPORTANT — this helper does NOT submit `/clear` on its own, and does NOT
/// reproduce the TUI's body/Enter split. The TUI writes the body, then fires
/// Enter only ~10s LATER via its drain loop (`ENTER_GAP`, `tui/src/app.rs:330`;
/// deferred through `pending_enter`, `app.rs:3869`). That gap is load-bearing:
/// codex treats a body immediately followed by `\r` as a single PASTE, so the
/// `\r` lands as a literal character and the command never executes. Sending
/// the returned Enter bytes back-to-back with the body here would therefore NOT
/// submit against codex. Enforcing the deferred gap before firing the Enter is
/// the Phase 3 drainer's job — this synchronous primitive only writes the body
/// and hands the Enter bytes back for the drainer to fire later.
pub fn send_clear_body<W>(tracker: &PtyModeTracker, mut write: W) -> std::io::Result<Vec<u8>>
where
    W: FnMut(&[u8]) -> std::io::Result<()>,
{
    let mode = tracker.term_mode();
    let body = pty_tracker::format_body_for_delivery("/clear", mode);
    write(&body)?;
    Ok(pty_tracker::enter_bytes_for_mode(mode).to_vec())
}

/// One step of the fresh reset, retry-friendly for a drainer that ticks.
pub enum ResetStep {
    /// The PTY has not been quiet for `quiet_window`; nothing was done. The
    /// caller should retry on a later tick.
    WaitingForQuiet,
    /// The reset was performed: the `/clear` body was written and run state
    /// mutated. `snapshot` is the pre-`/clear` listing the caller must hold for
    /// discovery. `pending_enter` is the mode-correct Enter keystroke that has
    /// NOT been sent: the caller (Phase 3 drainer) must fire it after the
    /// deferred gap so codex doesn't classify body+Enter as a paste — see
    /// [`send_clear_body`]. Until it is fired, `/clear` has not submitted.
    Done {
        snapshot: PreClearSnapshot,
        run: WorkflowRun,
        pending_enter: Vec<u8>,
    },
}

/// Perform a fresh reset for `role` on its owned session, gated on PTY-quiet
/// (Phase 1). Returns [`ResetStep::WaitingForQuiet`] until the PTY has been
/// quiet for `quiet_window` relative to `now`; once quiet, in order:
///   1. snapshot the pre-`/clear` transcript dir (taken just before `/clear`,
///      so it reflects the dir at rotation time),
///   2. write the `/clear` BODY via `write` and capture the mode-correct Enter
///      bytes — the Enter is NOT fired here (returned in `pending_enter`; the
///      Phase 3 drainer fires it after the deferred gap, see [`send_clear_body`]),
///   3. reset the role baseline to zero + clear `current_session_id`.
///
/// Resetting run state in step 3 before the `/clear` has actually submitted
/// (its Enter still pending) matches the TUI's `reset_fresh_session`, which
/// likewise resets the baseline/sid while the queued `/clear` is still waiting
/// to fire: the idle gate is inert while `current_session_id` is unbound
/// (`NoTranscriptId`), and discovery can't land until the new transcript
/// appears after the prompt is delivered.
///
/// `write` is the PTY sink — production passes a closure over the owned
/// session's `InputHandle::write_and_stamp`; tests can capture bytes.
pub fn reset_fresh_role<W>(
    run_id: &str,
    role: &str,
    engine: Engine,
    worktree: &Path,
    tracker: &PtyModeTracker,
    now: Instant,
    quiet_window: Duration,
    write: W,
) -> Result<ResetStep, PersistError>
where
    W: FnMut(&[u8]) -> std::io::Result<()>,
{
    if !tracker.quiet_for(quiet_window, now) {
        return Ok(ResetStep::WaitingForQuiet);
    }
    // Snapshot BEFORE sending /clear so the rotated-to file is excluded from
    // the diff baseline (and the old transcript stem stays in the snapshot,
    // guaranteeing discovery never rebinds back to it).
    let snapshot = snapshot_pre_clear(engine, worktree);
    // Body written now; Enter returned un-fired for the Phase 3 drainer to fire
    // after the gap. io::Error -> PersistError::Io via `?`.
    let pending_enter = send_clear_body(tracker, write)?;
    let run = apply_reset_state(run_id, role)?;
    Ok(ResetStep::Done {
        snapshot,
        run,
        pending_enter,
    })
}

/// Try to land the rebind. Runs listing-diff discovery against `snapshot`; on a
/// hit binds `current_session_id` under flock. Idempotent and restart-safe: if
/// the role is already bound (e.g. a prior tick, or a daemon restart that lost
/// no on-disk state) it returns the existing sid without rebinding. Returns the
/// bound sid (this call or already), or `None` if no new transcript exists yet.
///
/// The already-bound check runs INSIDE the `modify` flock closure (against the
/// reloaded on-disk run), so a concurrent binder can't be clobbered and a
/// restart re-finds the same file without double-binding.
pub fn try_rebind(
    run_id: &str,
    role: &str,
    engine: Engine,
    worktree: &Path,
    snapshot: &[String],
) -> Result<Option<String>, PersistError> {
    let discovered = discover_rebind(engine, worktree, snapshot);
    let mut bound: Option<String> = None;
    run::modify(run_id, |r| {
        if let Some(b) = r.role_sessions.get_mut(role) {
            if let Some(existing) = &b.current_session_id {
                // Already bound — idempotent no-op (restart / repeat tick).
                bound = Some(existing.clone());
            } else if let Some(sid) = &discovered {
                b.current_session_id = Some(sid.clone());
                bound = Some(sid.clone());
            }
        }
    })?;
    Ok(bound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::run::{RoleBinding, WorkflowRun};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    /// Serialize HOME-mutating tests (shared with the rest of the crate).
    fn home_guard() -> std::sync::MutexGuard<'static, ()> {
        crate::test_support::env_lock()
    }

    fn encoded_claude_dir(home: &Path, worktree: &Path) -> PathBuf {
        let encoded = worktree
            .to_str()
            .unwrap()
            .replace('/', "-")
            .replace('.', "-");
        home.join(format!(".claude/projects/{}", encoded))
    }

    /// One assistant text line per message.
    fn write_claude_transcript(dir: &Path, sid: &str, assistant_msgs: &[&str]) {
        std::fs::create_dir_all(dir).unwrap();
        let mut body = String::new();
        for m in assistant_msgs {
            body.push_str(&format!(
                r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":{}}}]}}}}"#,
                serde_json::to_string(m).unwrap()
            ));
            body.push('\n');
        }
        std::fs::write(dir.join(format!("{}.jsonl", sid)), body).unwrap();
    }

    fn make_fresh_run(run_id: &str, worktree: &Path, role: &str, uid: &str, old_sid: &str) {
        let mut roles = BTreeMap::new();
        roles.insert(
            role.to_string(),
            RoleBinding {
                session_label: role.to_string(),
                current_session_id: Some(old_sid.to_string()),
                daemon_session_uid: Some(uid.to_string()),
            },
        );
        // Non-zero baseline so the reset's "-> 0" is observable.
        let mut baselines = BTreeMap::new();
        baselines.insert(
            role.to_string(),
            MessageBaseline {
                user_count: 3,
                assistant_count: 3,
            },
        );
        let run = WorkflowRun::new(
            run_id.to_string(),
            "feedback".to_string(),
            worktree.to_str().unwrap().to_string(),
            roles,
            role.to_string(),
            baselines,
            None,
            BTreeMap::new(),
            0,
        );
        run::save(&run).unwrap();
    }

    #[test]
    fn rebind_landed_tracks_current_session_id() {
        let mut roles = BTreeMap::new();
        roles.insert(
            "reviewer".to_string(),
            RoleBinding {
                session_label: "reviewer".to_string(),
                current_session_id: None,
                daemon_session_uid: Some("uid".to_string()),
            },
        );
        let run = WorkflowRun::new(
            "wf-x".to_string(),
            "feedback".to_string(),
            "/wt".to_string(),
            roles,
            "reviewer".to_string(),
            BTreeMap::new(),
            None,
            BTreeMap::new(),
            0,
        );
        assert!(!rebind_landed(&run, "reviewer"));
        let mut bound = run.clone();
        bound
            .role_sessions
            .get_mut("reviewer")
            .unwrap()
            .current_session_id = Some("new".to_string());
        assert!(rebind_landed(&bound, "reviewer"));
    }

    #[test]
    fn discovery_never_returns_a_pre_clear_stem() {
        let _g = home_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());
        let worktree = home.path().join("wt");
        let dir = encoded_claude_dir(home.path(), &worktree);

        // Only the pre-/clear transcript exists; it's in the snapshot.
        write_claude_transcript(&dir, "old-sid", &["old turn"]);
        let snapshot = snapshot_pre_clear(Engine::ClaudeCode, &worktree);
        assert_eq!(snapshot, vec!["old-sid".to_string()]);

        // No new file yet -> no rebind candidate (NOT the old one).
        assert_eq!(
            discover_rebind(Engine::ClaudeCode, &worktree, &snapshot),
            None
        );

        // After /clear rotates to a new file, that one wins — never old-sid.
        write_claude_transcript(&dir, "new-sid", &["fresh turn"]);
        assert_eq!(
            discover_rebind(Engine::ClaudeCode, &worktree, &snapshot),
            Some("new-sid".to_string())
        );

        std::env::remove_var("HOME");
    }

    /// `send_clear_body` writes ONLY the body and hands back the un-fired
    /// Enter bytes — it must NOT submit `/clear` itself (the deferred gap before
    /// Enter is Phase 3's job; back-to-back body+Enter would paste against codex).
    #[test]
    fn send_clear_writes_body_only_and_returns_enter() {
        // Raw mode (unfed tracker): Enter is `\r`.
        let tracker = PtyModeTracker::new();
        let mut writes: Vec<Vec<u8>> = Vec::new();
        let enter = send_clear_body(&tracker, |b| {
            writes.push(b.to_vec());
            Ok(())
        })
        .unwrap();
        assert_eq!(
            writes,
            vec![b"/clear".to_vec()],
            "only the body is written; Enter is NOT sent here"
        );
        assert_eq!(enter, b"\r".to_vec());

        // Kitty keyboard mode: Enter is CSI 13 u.
        let mut tracker = PtyModeTracker::new();
        tracker.feed(b"\x1b[>1u", Instant::now());
        let mut writes2: Vec<Vec<u8>> = Vec::new();
        let enter2 = send_clear_body(&tracker, |b| {
            writes2.push(b.to_vec());
            Ok(())
        })
        .unwrap();
        assert_eq!(writes2, vec![b"/clear".to_vec()]);
        assert_eq!(enter2, b"\x1b[13u".to_vec());
    }

    #[test]
    fn reset_waits_for_pty_quiet() {
        let _g = home_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());
        let worktree = home.path().join("wt");
        make_fresh_run("wf-quiet", &worktree, "reviewer", "uid-q", "old-sid");

        // A tracker with very recent output -> NOT quiet.
        let mut tracker = PtyModeTracker::new();
        let now = Instant::now();
        tracker.feed(b"streaming output", now);
        let step = reset_fresh_role(
            "wf-quiet",
            "reviewer",
            Engine::ClaudeCode,
            &worktree,
            &tracker,
            now + Duration::from_millis(100),
            DEFAULT_QUIET_WINDOW,
            |_b| Ok(()),
        )
        .unwrap();
        assert!(matches!(step, ResetStep::WaitingForQuiet));
        // State untouched: still bound to the old sid.
        let run = run::modify("wf-quiet", |_| {}).unwrap();
        assert_eq!(
            run.role_sessions["reviewer"].current_session_id.as_deref(),
            Some("old-sid")
        );

        std::env::remove_var("HOME");
    }

    /// Acceptance #1 + #2 + #3 end-to-end with a real owned session, exercised
    /// directly (reset -> simulate delivery+rotation -> discover).
    #[test]
    fn fresh_reset_then_discovery_rebinds_to_new_transcript() {
        use crate::session::{DaemonSession, SpawnParams};

        let _g = home_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());

        let worktree = home.path().join("wt");
        let dir = encoded_claude_dir(home.path(), &worktree);
        write_claude_transcript(&dir, "old-sid", &["pre-clear turn one", "pre-clear turn two"]);

        let run_id = "wf-reset-e2e";
        let role = "reviewer";
        let uid = "sess-reviewer";
        make_fresh_run(run_id, &worktree, role, uid, "old-sid");

        // A real owned session stands in for the sidebar slot. `/bin/cat`
        // stays alive reading its PTY and exits cleanly when the master closes
        // on drop, so there's no orphan child.
        let session =
            DaemonSession::spawn(SpawnParams::new(uid, "reviewer", "/bin/cat")).unwrap();
        let handle = session.input_handle();
        // Hold the session in a map to assert the slot survives the reset.
        let mut sessions: std::collections::HashMap<String, DaemonSession> =
            std::collections::HashMap::new();
        sessions.insert(uid.to_string(), session);
        assert!(sessions.contains_key(uid), "slot present before reset");

        // Unfed tracker -> quiet -> the reset proceeds.
        let tracker = PtyModeTracker::new();
        let step = reset_fresh_role(
            run_id,
            role,
            Engine::ClaudeCode,
            &worktree,
            &tracker,
            Instant::now(),
            DEFAULT_QUIET_WINDOW,
            |b| handle.write_and_stamp(b),
        )
        .unwrap();
        let snapshot = match step {
            ResetStep::Done {
                snapshot,
                run,
                pending_enter,
            } => {
                // Baseline reset to zero; current_session_id cleared.
                assert_eq!(run.role_baselines[role].user_count, 0);
                assert_eq!(run.role_baselines[role].assistant_count, 0);
                assert!(run.role_sessions[role].current_session_id.is_none());
                assert!(!rebind_landed(&run, role), "predicate false right after reset");
                // Enter is returned un-fired (raw \r for an unfed/non-kitty
                // tracker) for the Phase 3 drainer to fire after the gap.
                assert_eq!(pending_enter, b"\r".to_vec());
                snapshot
            }
            ResetStep::WaitingForQuiet => panic!("unfed tracker should be quiet"),
        };
        assert!(snapshot.contains(&"old-sid".to_string()));
        // Slot preserved — the reset never touches the session registry.
        assert!(sessions.contains_key(uid), "slot present after reset");

        // --- Before delivery / rotation: discovery must NOT bind anything ---
        let bound = try_rebind(run_id, role, Engine::ClaudeCode, &worktree, &snapshot).unwrap();
        assert_eq!(bound, None, "no new transcript yet -> no rebind");
        let run = run::modify(run_id, |_| {}).unwrap();
        assert!(
            run.role_sessions[role].current_session_id.is_none(),
            "current_session_id stays unbound before discovery (no false value)"
        );
        assert!(!rebind_landed(&run, role));

        // --- Simulate prompt delivery + claude rotating to a NEW transcript ---
        write_claude_transcript(&dir, "new-sid", &["fresh activation reply", "second reply"]);

        // Tick discovery until it lands.
        let bound = try_rebind(run_id, role, Engine::ClaudeCode, &worktree, &snapshot)
            .unwrap()
            .expect("discovery should land on the new transcript");
        assert_eq!(bound, "new-sid");

        let run = run::modify(run_id, |_| {}).unwrap();
        assert_eq!(
            run.role_sessions[role].current_session_id.as_deref(),
            Some("new-sid"),
            "rebound to the NEW transcript"
        );
        assert_ne!(
            run.role_sessions[role].current_session_id.as_deref(),
            Some("old-sid"),
            "never rebinds to the pre-/clear transcript"
        );
        assert!(rebind_landed(&run, role));

        // Idempotent: a repeat tick returns the same sid, doesn't rebind anew.
        let again = try_rebind(run_id, role, Engine::ClaudeCode, &worktree, &snapshot).unwrap();
        assert_eq!(again.as_deref(), Some("new-sid"));

        // --- Acceptance #3: resolver slices that role from index 0 ---
        let mut role_engines = BTreeMap::new();
        role_engines.insert(role.to_string(), Engine::ClaudeCode);
        let resolver = crate::workflow::poller::DaemonWorkflowResolver {
            run: &run,
            worktree_path: Some(worktree.as_path()),
            role_engines,
        };
        use crate::workflow::template::RoleResolver;
        let msgs = resolver.assistant_messages(role);
        assert_eq!(
            msgs,
            vec![
                "fresh activation reply".to_string(),
                "second reply".to_string()
            ],
            "post-reset resolver slices from index 0 (baseline reset), not from the old offset"
        );

        // Slot still present at the end.
        assert!(sessions.contains_key(uid));
        std::env::remove_var("HOME");
    }
}
