//! Daemon-side activation finalization + delivery drainer (Phase 3 of
//! `doc/daemon-side-workflow-orchestration.md`).
//!
//! `workflow_transition` records a durable [`PendingActivation`] alongside the
//! close/iteration/active_role mutation, but it does NOT append history and
//! does NOT render. This module is the drainer that completes the hand-off,
//! keyed off run state (never events), idempotent, and resumable after a daemon
//! restart from the persisted [`ActivationPhase`].
//!
//! ## Finalization order (load-bearing — see design doc §A)
//!
//!   1. **Render** the prompt via [`DaemonWorkflowResolver`] from the PRE-reset,
//!      PRE-append run snapshot, and freeze it on the record. `template_source`
//!      = the supplied raw prompt if non-empty, else the target role's
//!      `subsequent_activation_prompt`/`activation_prompt` (chosen by
//!      `prior_activations`, which is 0 on a first activation because the
//!      history entry isn't appended until step 4). Rendering BEFORE the append
//!      keeps `prior_activations`/`this_turn` at pre-activation state; rendering
//!      BEFORE `/clear` lets a fresh role's template see its pre-`/clear`
//!      transcript.
//!   2. **Fresh reset** (fresh roles only): `/clear` body, reset baseline to
//!      zero, clear `current_session_id`, snapshot the transcript dir. Does NOT
//!      block on the post-delivery rebind.
//!   3. **Baselines**: FRESH -> both 0 (post-`/clear` transcript empty);
//!      PERSISTENT -> `assistant_count_at_start` = `count_messages` (turn count)
//!      and `text_messages_at_start` = `list_messages(Assistant).len()` (text
//!      count) on the stable sid.
//!   4. **Append** the incoming role's history entry (counts from step 3;
//!      `session_id` = stable sid for persistent, pending for fresh).
//!   5. **Deliver** the frozen prompt body, then the Enter (after the deferred
//!      gap, encoded for the LIVE terminal mode).
//!   6. **Rebind** (fresh only): AFTER delivery, run sid discovery to rebind
//!      `current_session_id` and patch the entry's `session_id`. The idle gate
//!      stays inert (`NoTranscriptId`) until then.
//!
//! The state machine advances one [`ActivationPhase`] per [`advance_finalization`]
//! call, persisting each step, so a restart mid-finalization resumes exactly
//! once: the body/Enter are guarded by the phase, and the history append's
//! per-iteration idempotency guard (`run.rs`) absorbs a re-append.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use crate::workflow::fresh_reset;
use crate::workflow::poller::DaemonWorkflowResolver;
use crate::workflow::pty_tracker::{self, PtyModeTracker};
use crate::workflow::run::{self, ActivationPhase, PersistError, WorkflowRun};
use crate::workflow::toml_schema::{Engine, Workflow};
use crate::workflow::transcript::{self, MessageKind};

/// Default deferred gap between writing a body and firing its Enter. Mirrors the
/// TUI's `ENTER_GAP` (`tui/src/app.rs`): codex treats body+Enter arriving
/// together as a paste, so the `\r` must arrive as a separate later keystroke.
pub const DEFAULT_ENTER_GAP_MS: u64 = 10_000;

/// Inputs the drainer needs that don't live on the run: the workflow definition
/// (templates + role context), the worktree, the per-role engine map, and the
/// two clocks (`now_ms` drives the persisted Enter gap; `now_instant` drives the
/// PTY-quiet check against the live tracker).
pub struct FinalizeCtx<'a> {
    pub run_id: &'a str,
    pub workflow: &'a Workflow,
    pub worktree: &'a Path,
    pub role_engines: BTreeMap<String, Engine>,
    pub now_ms: u64,
    pub now_instant: Instant,
    pub gap_ms: u64,
    pub quiet_window: std::time::Duration,
}

/// Outcome of one [`advance_finalization`] step.
#[derive(Debug, PartialEq, Eq)]
pub enum FinalizeStep {
    /// No pending activation on this run.
    NoPending,
    /// Waiting on a precondition (PTY quiet, or the deferred Enter gap). The
    /// caller should retry on a later tick.
    Blocked,
    /// Advanced to the given phase this call.
    Advanced(ActivationPhase),
    /// Finalization complete (record cleared).
    Done,
}

/// Engine for `role`, defaulting to Claude when unmapped (mirrors the poller's
/// `engine_for_session_type` default).
fn engine_for(ctx: &FinalizeCtx, role: &str) -> Engine {
    ctx.role_engines.get(role).cloned().unwrap_or(Engine::ClaudeCode)
}

/// Advance the run's pending activation by one phase, persisting the step.
/// `tracker` supplies PTY-quiet timing + live terminal mode; `write` is the
/// owned session's PTY sink (body + Enter).
pub fn advance_finalization<W>(
    ctx: &FinalizeCtx,
    tracker: &PtyModeTracker,
    mut write: W,
) -> Result<FinalizeStep, PersistError>
where
    W: FnMut(&[u8]) -> std::io::Result<()>,
{
    let run = match run::load_one(ctx.run_id) {
        Some(r) => r,
        None => return Ok(FinalizeStep::NoPending),
    };
    let pa = match run.pending_activation.clone() {
        Some(pa) => pa,
        None => return Ok(FinalizeStep::NoPending),
    };
    let role = pa.target_role.clone();
    let engine = engine_for(ctx, &role);

    match pa.phase {
        ActivationPhase::Queued => {
            // Step 1: render from the PRE-reset, PRE-append snapshot and freeze.
            let rendered = render_activation(ctx, &run, &pa);
            run::modify(ctx.run_id, |r| {
                if let Some(pa) = r.pending_activation.as_mut() {
                    pa.rendered_prompt = Some(rendered.clone());
                    pa.phase = ActivationPhase::Rendered;
                }
            })?;
            Ok(FinalizeStep::Advanced(ActivationPhase::Rendered))
        }

        ActivationPhase::Rendered => {
            if pa.is_initial {
                // Phase 4: the run's initial activation. `WorkflowRun::new`
                // already seeded the worker's `HistoryEntry` (iteration 1,
                // `TriggerKind::Initial`), so this is DELIVERY-ONLY: never
                // append a second worker row (acceptance #2). Best-effort patch
                // the existing entry's `session_id` from the (possibly
                // now-known) binding, then advance straight to delivery.
                run::modify(ctx.run_id, |r| {
                    let sid = r
                        .role_sessions
                        .get(&role)
                        .and_then(|b| b.current_session_id.clone());
                    if let Some(h) = r.history.iter_mut().rev().find(|h| h.role == role) {
                        if h.session_id.is_none() {
                            h.session_id = sid;
                        }
                    }
                    if let Some(pa) = r.pending_activation.as_mut() {
                        pa.phase = ActivationPhase::Appended;
                    }
                })?;
                Ok(FinalizeStep::Advanced(ActivationPhase::Appended))
            } else if pa.needs_fresh_reset {
                // Step 2: fresh reset, gated on PTY-quiet. Snapshot BEFORE
                // /clear, send the /clear body, then reset baseline + clear sid
                // + persist the snapshot/phase in one flock modify.
                if !tracker.quiet_for(ctx.quiet_window, ctx.now_instant) {
                    return Ok(FinalizeStep::Blocked);
                }
                let snapshot = fresh_reset::snapshot_pre_clear(engine, ctx.worktree);
                // Body only; the /clear Enter fires later (recomputed for the
                // live mode). The returned Enter bytes are intentionally unused.
                let _ = fresh_reset::send_clear_body(tracker, &mut write)
                    .map_err(PersistError::Io)?;
                let enter_at = ctx.now_ms + ctx.gap_ms;
                run::modify(ctx.run_id, |r| {
                    r.role_baselines.insert(
                        role.clone(),
                        crate::workflow::run::MessageBaseline::default(),
                    );
                    if let Some(b) = r.role_sessions.get_mut(&role) {
                        b.current_session_id = None;
                    }
                    if let Some(pa) = r.pending_activation.as_mut() {
                        pa.pre_clear_snapshot = Some(snapshot.clone());
                        pa.enter_fire_at_ms = Some(enter_at);
                        pa.phase = ActivationPhase::ClearBodySent;
                    }
                })?;
                Ok(FinalizeStep::Advanced(ActivationPhase::ClearBodySent))
            } else {
                // Persistent: no /clear. Steps 3+4 — compute stable-sid
                // baselines and append the incoming role's history entry.
                let (assistant_count, text_count) =
                    persistent_baselines(&run, &role, engine, ctx.worktree);
                append_history(ctx.run_id, &pa, assistant_count, text_count)?;
                Ok(FinalizeStep::Advanced(ActivationPhase::Appended))
            }
        }

        ActivationPhase::ClearBodySent => {
            // Fire the /clear Enter once the deferred gap elapses, then steps
            // 3+4 with FRESH baselines (both 0 — post-/clear transcript empty).
            if ctx.now_ms < pa.enter_fire_at_ms.unwrap_or(0) {
                return Ok(FinalizeStep::Blocked);
            }
            write(pty_tracker::enter_bytes_for_mode(tracker.term_mode()))
                .map_err(PersistError::Io)?;
            // FRESH baselines are both 0 (post-/clear transcript empty).
            // append_history advances the phase to Appended and clears the
            // (now-fired) /clear Enter deadline in the same modify.
            append_history(ctx.run_id, &pa, 0, 0)?;
            Ok(FinalizeStep::Advanced(ActivationPhase::Appended))
        }

        ActivationPhase::Appended => {
            // Step 5 (body): deliver the frozen prompt body, gated on PTY-quiet,
            // framed for the live mode. Persist the Enter deadline + phase.
            if !tracker.quiet_for(ctx.quiet_window, ctx.now_instant) {
                return Ok(FinalizeStep::Blocked);
            }
            let body = pa.rendered_prompt.clone().unwrap_or_default();
            // Trim trailing line endings BEFORE framing — mirrors the TUI's
            // `deliver_pending_write` (`tui/src/app.rs`:
            // `trim_end_matches(['\r', '\n'])`). A feedback.toml activation
            // prompt rendered from a TOML multiline string commonly ends in
            // `\n`; with the body/Enter split that trailing newline would ride
            // into the body write and the separately-deferred Enter would then
            // submit on a continuation line (or submit early) — the same
            // codex non-submission failure the body/Enter split exists to avoid.
            let body = body.trim_end_matches(['\r', '\n']);
            let payload =
                pty_tracker::format_body_for_delivery(body, tracker.term_mode());
            write(&payload).map_err(PersistError::Io)?;
            let enter_at = ctx.now_ms + ctx.gap_ms;
            run::modify(ctx.run_id, |r| {
                if let Some(pa) = r.pending_activation.as_mut() {
                    pa.enter_fire_at_ms = Some(enter_at);
                    pa.phase = ActivationPhase::BodySent;
                }
            })?;
            Ok(FinalizeStep::Advanced(ActivationPhase::BodySent))
        }

        ActivationPhase::BodySent => {
            // Step 5 (Enter): fire after the gap, encoded for the LIVE mode.
            if ctx.now_ms < pa.enter_fire_at_ms.unwrap_or(0) {
                return Ok(FinalizeStep::Blocked);
            }
            write(pty_tracker::enter_bytes_for_mode(tracker.term_mode()))
                .map_err(PersistError::Io)?;
            if pa.needs_fresh_reset {
                // Fresh: delivery done, but the record stays until the sid
                // rebinds (step 6). The idle gate is inert (NoTranscriptId).
                run::modify(ctx.run_id, |r| {
                    if let Some(pa) = r.pending_activation.as_mut() {
                        pa.enter_fire_at_ms = None;
                        pa.phase = ActivationPhase::RebindPending;
                    }
                })?;
                Ok(FinalizeStep::Advanced(ActivationPhase::RebindPending))
            } else {
                // Persistent: finalization complete — clear the record.
                run::modify(ctx.run_id, |r| {
                    r.pending_activation = None;
                })?;
                Ok(FinalizeStep::Done)
            }
        }

        ActivationPhase::RebindPending => {
            // Step 6 (fresh): discovery rebinds current_session_id, then patch
            // the appended entry's session_id and clear the record.
            let snapshot = pa.pre_clear_snapshot.clone().unwrap_or_default();
            match fresh_reset::try_rebind(ctx.run_id, &role, engine, ctx.worktree, &snapshot)? {
                Some(sid) => {
                    run::modify(ctx.run_id, |r| {
                        if let Some(h) = r
                            .history
                            .iter_mut()
                            .rev()
                            .find(|h| h.role == role)
                        {
                            h.session_id = Some(sid.clone());
                        }
                        r.pending_activation = None;
                    })?;
                    Ok(FinalizeStep::Done)
                }
                None => Ok(FinalizeStep::Blocked),
            }
        }

        ActivationPhase::Done => {
            // Terminal sentinel; the record is normally cleared instead.
            run::modify(ctx.run_id, |r| r.pending_activation = None)?;
            Ok(FinalizeStep::Done)
        }
    }
}

/// Render the activation prompt for `pa`, applying the empty -> template
/// fallback. `template_source` = the supplied raw prompt if non-empty, else the
/// target role's `subsequent_activation_prompt`/`activation_prompt` chosen by
/// `prior_activations` (0 = first activation -> `activation_prompt`). Rendered
/// against the PRE-reset, PRE-append run snapshot via `DaemonWorkflowResolver`.
fn render_activation(ctx: &FinalizeCtx, run: &WorkflowRun, pa: &run::PendingActivation) -> String {
    // P5: a goal-only initial prompt is delivered verbatim (template braces in
    // the goal must NOT be expanded/mangled), matching the TUI.
    if pa.verbatim {
        return pa.raw_prompt.clone();
    }
    let prior_activations = run
        .history
        .iter()
        .filter(|h| h.role == pa.target_role)
        .count();
    let template_source: String = if !pa.raw_prompt.is_empty() {
        pa.raw_prompt.clone()
    } else {
        ctx.workflow
            .roles
            .get(&pa.target_role)
            .and_then(|r| {
                if prior_activations > 0 {
                    r.subsequent_activation_prompt
                        .as_deref()
                        .or(r.activation_prompt.as_deref())
                } else {
                    r.activation_prompt.as_deref()
                }
            })
            .unwrap_or("")
            .to_string()
    };
    let resolver = DaemonWorkflowResolver {
        run,
        worktree_path: Some(ctx.worktree),
        role_engines: ctx.role_engines.clone(),
    };
    crate::workflow::template::render(&template_source, &resolver)
}

/// Stable-sid baselines for a persistent role: (turn count, text count). Returns
/// (0, 0) if the role has no bound sid.
fn persistent_baselines(
    run: &WorkflowRun,
    role: &str,
    engine: Engine,
    worktree: &Path,
) -> (usize, usize) {
    let Some(sid) = run
        .role_sessions
        .get(role)
        .and_then(|b| b.current_session_id.as_deref())
    else {
        return (0, 0);
    };
    let assistant_count = transcript::count_messages(&engine, worktree, sid, MessageKind::Assistant);
    let text_count = transcript::list_messages(&engine, worktree, sid, MessageKind::Assistant).len();
    (assistant_count, text_count)
}

/// Append the incoming role's history entry with the given counts and the
/// persisted `TriggerKind`. Reuses the run's per-iteration idempotency guard, so
/// a re-append on restart is a no-op.
fn append_history(
    run_id: &str,
    pa: &run::PendingActivation,
    assistant_count: usize,
    text_count: usize,
) -> Result<(), PersistError> {
    let target = pa.target_role.clone();
    let iteration = pa.iteration;
    let trigger = pa.trigger.clone();
    run::modify(run_id, move |r| {
        r.append_history_entry_for_event_target_role(
            &target,
            iteration,
            trigger,
            assistant_count,
            text_count,
        );
        // Advance the phase in the SAME flock modify as the append, so the
        // step is one durable checkpoint (and the drainer doesn't re-enter
        // the append arm — the per-iteration guard would no-op it, but the
        // phase must move forward). Any pending Enter (the /clear's) is
        // consumed by the time we append.
        if let Some(pa) = r.pending_activation.as_mut() {
            pa.phase = ActivationPhase::Appended;
            pa.enter_fire_at_ms = None;
        }
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::run::{
        ActivationPhase, PendingActivation, RoleBinding, TriggerKind, WorkflowRun,
    };
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::Duration;

    const WF_TOML: &str = r#"
name = "test-wf"
description = "phase-3 finalize tests"
role_order = ["worker", "reviewer", "manager"]

[roles.worker]
engine = "claude-code"
context = "persistent"

[roles.reviewer]
engine = "claude-code"
context = "fresh"
activation_prompt = "Review. Your prior note was: {{ roles.reviewer.last_message }}. {{ rejected_findings }}"
subsequent_activation_prompt = "Subsequent reviewer activation."

[roles.manager]
engine = "claude-code"
context = "persistent"
activation_prompt = "Manager FIRST. Worker this_turn: {{ roles.worker.this_turn }}"
subsequent_activation_prompt = "Manager SUBSEQUENT. Worker this_turn: {{ roles.worker.this_turn }}"

[[transitions]]
from = "worker"
on = "idle"
to = "reviewer"
[[transitions]]
from = "reviewer"
on = "idle"
to = "manager"
"#;

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        crate::test_support::env_lock()
    }

    fn workflow() -> Workflow {
        Workflow::from_toml_str(WF_TOML).expect("parse test workflow")
    }

    fn role_engines() -> BTreeMap<String, Engine> {
        let mut m = BTreeMap::new();
        m.insert("worker".into(), Engine::ClaudeCode);
        m.insert("reviewer".into(), Engine::ClaudeCode);
        m.insert("manager".into(), Engine::ClaudeCode);
        m
    }

    fn claude_dir(home: &Path, wt: &Path) -> PathBuf {
        let enc = wt.to_str().unwrap().replace('/', "-").replace('.', "-");
        home.join(format!(".claude/projects/{}", enc))
    }

    /// Each tuple is (is_text, text). A non-text ("tool") line is an assistant
    /// turn with only a tool_use block — counted by `count_messages` (turn) but
    /// NOT by `list_messages(Assistant)` (text), so the two diverge.
    fn write_transcript(dir: &Path, sid: &str, lines: &[(bool, &str)]) {
        std::fs::create_dir_all(dir).unwrap();
        let mut body = String::new();
        for (is_text, text) in lines {
            if *is_text {
                body.push_str(&format!(
                    r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":{}}}]}}}}"#,
                    serde_json::to_string(text).unwrap()
                ));
            } else {
                body.push_str(
                    r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{}}]}}"#,
                );
            }
            body.push('\n');
        }
        std::fs::write(dir.join(format!("{}.jsonl", sid)), body).unwrap();
    }

    fn ctx<'a>(
        run_id: &'a str,
        wf: &'a Workflow,
        wt: &'a Path,
        now_ms: u64,
        gap_ms: u64,
    ) -> FinalizeCtx<'a> {
        FinalizeCtx {
            run_id,
            workflow: wf,
            worktree: wt,
            role_engines: role_engines(),
            now_ms,
            now_instant: Instant::now(),
            gap_ms,
            quiet_window: Duration::from_secs(2),
        }
    }

    /// Build a run mid-hand-off: `active_role` already advanced to `target`,
    /// a `pending_activation` in `Queued`, and the given role bindings/history.
    #[allow(clippy::too_many_arguments)]
    fn seed_run(
        run_id: &str,
        wt: &Path,
        target: &str,
        iteration: u32,
        trigger: TriggerKind,
        raw_prompt: &str,
        needs_fresh_reset: bool,
        bindings: &[(&str, Option<&str>)],
        history: Vec<crate::workflow::run::HistoryEntry>,
    ) {
        let mut roles = BTreeMap::new();
        for (role, sid) in bindings {
            roles.insert(
                role.to_string(),
                RoleBinding {
                    session_label: role.to_string(),
                    current_session_id: sid.map(|s| s.to_string()),
                    daemon_session_uid: Some(format!("uid-{role}")),
                },
            );
        }
        let mut run = WorkflowRun::new(
            run_id.to_string(),
            "test-wf".to_string(),
            wt.to_str().unwrap().to_string(),
            roles,
            target.to_string(),
            BTreeMap::new(),
            Some("the goal".to_string()),
            BTreeMap::new(),
            0,
        );
        run.iteration = iteration;
        run.active_role = Some(target.to_string());
        run.history = history;
        run.pending_activation = Some(PendingActivation {
            activation_id: iteration as u64,
            target_role: target.to_string(),
            iteration,
            trigger,
            raw_prompt: raw_prompt.to_string(),
            verbatim: false,
            needs_fresh_reset,
            is_initial: false,
            phase: ActivationPhase::Queued,
            rendered_prompt: None,
            pre_clear_snapshot: None,
            enter_fire_at_ms: None,
        });
        run::save(&run).unwrap();
    }

    fn hist(role: &str, iteration: u32, sid: Option<&str>, text_at_start: usize) -> crate::workflow::run::HistoryEntry {
        crate::workflow::run::HistoryEntry {
            iteration,
            role: role.to_string(),
            session_id: sid.map(|s| s.to_string()),
            last_message: None,
            activated_at: 0,
            deactivated_at: Some(1),
            trigger: TriggerKind::Initial,
            assistant_count_at_start: 0,
            text_messages_at_start: text_at_start,
        }
    }

    /// Drive `advance` until Done/NoPending or it Blocks, collecting writes.
    fn drive(ctx: &FinalizeCtx, tracker: &PtyModeTracker, writes: &mut Vec<Vec<u8>>) -> FinalizeStep {
        loop {
            let step = advance_finalization(ctx, tracker, |b| {
                writes.push(b.to_vec());
                Ok(())
            })
            .unwrap();
            match step {
                FinalizeStep::Advanced(_) => continue,
                other => return other,
            }
        }
    }

    // ---- Persistent role: counts, delivery, trigger ----------------------

    #[test]
    fn persistent_finalization_appends_distinct_counts_and_delivers() {
        let _g = env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());
        let wt = home.path().join("wt");
        let dir = claude_dir(home.path(), &wt);
        // worker transcript: 2 text turns (for this_turn).
        write_transcript(&dir, "wkr", &[(true, "did step one"), (true, "did step two")]);
        // manager (stable) transcript: text, tool-only, text -> count=3, text=2.
        write_transcript(&dir, "mgr", &[(true, "ack"), (false, ""), (true, "ok")]);

        let wf = workflow();
        seed_run(
            "wf-persist",
            &wt,
            "manager",
            2,
            TriggerKind::StaticIdle { from_role: "reviewer".into() },
            "", // static -> template fallback
            false,
            &[("worker", Some("wkr")), ("reviewer", Some("rev")), ("manager", Some("mgr"))],
            vec![hist("worker", 1, Some("wkr"), 0)],
        );

        let mut writes = Vec::new();
        let step = drive(&ctx("wf-persist", &wf, &wt, 1000, 0), &PtyModeTracker::new(), &mut writes);
        assert_eq!(step, FinalizeStep::Done);

        let run = run::load_one("wf-persist").unwrap();
        assert!(run.pending_activation.is_none(), "record cleared at Done");
        // History entry appended for manager iter 2.
        let entry = run.history.iter().rev().find(|h| h.role == "manager").unwrap();
        assert_eq!(entry.iteration, 2);
        // Persistent counts: turn count (3) != text count (2).
        assert_eq!(entry.assistant_count_at_start, 3, "count_messages turn count");
        assert_eq!(entry.text_messages_at_start, 2, "list_messages text count");
        assert_ne!(entry.assistant_count_at_start, entry.text_messages_at_start);
        assert_eq!(entry.session_id.as_deref(), Some("mgr"), "persistent keeps stable sid");
        // TriggerKind reconstructed from the persisted record (criterion #8).
        assert_eq!(entry.trigger, TriggerKind::StaticIdle { from_role: "reviewer".into() });

        // Delivery: body (rendered, first activation template, with this_turn)
        // then a raw \r Enter.
        let body = String::from_utf8(writes[0].clone()).unwrap();
        assert!(body.starts_with("Manager FIRST."), "first activation uses activation_prompt: {body:?}");
        assert!(body.contains("did step one"));
        assert!(body.contains("did step two"));
        assert_eq!(writes[1], b"\r".to_vec(), "raw-mode Enter");

        // Gate no longer wedges: a history entry now exists for the active role.
        assert!(run.active_assistant_start_count().is_some(), "NoHistoryEntry resolved");
        std::env::remove_var("HOME");
    }

    // ---- Trailing-newline trim parity with the TUI ----------------------

    #[test]
    fn body_trailing_newline_trimmed_before_delivery() {
        let _g = env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());
        let wt = home.path().join("wt");
        let dir = claude_dir(home.path(), &wt);
        write_transcript(&dir, "mgr", &[(true, "ok")]);

        let wf = workflow();
        // Raw MCP prompt that ends in a newline (as a TOML multiline template
        // commonly would after rendering).
        seed_run(
            "wf-trim",
            &wt,
            "manager",
            2,
            TriggerKind::McpTransition {
                from_role: "reviewer".into(),
                prompt: "do this\nand that\n".into(),
                event_id: "e".into(),
            },
            "do this\nand that\n",
            false,
            &[("manager", Some("mgr"))],
            vec![hist("worker", 1, Some("mgr"), 0)],
        );

        let mut writes = Vec::new();
        let step = drive(&ctx("wf-trim", &wf, &wt, 1, 0), &PtyModeTracker::new(), &mut writes);
        assert_eq!(step, FinalizeStep::Done);

        // Body written first, with the trailing newline stripped; Enter separate.
        let body = String::from_utf8(writes[0].clone()).unwrap();
        assert_eq!(body, "do this\nand that", "trailing newline stripped before framing");
        assert!(!body.ends_with('\n'));
        assert_eq!(writes[1], b"\r".to_vec(), "Enter is a separate keystroke");
    }

    // ---- P5: goal-only initial prompt delivered verbatim -----------------

    #[test]
    fn verbatim_initial_prompt_skips_templating() {
        let _g = env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());
        let wt = home.path().join("wt");
        let wf = workflow();
        let mut roles = BTreeMap::new();
        roles.insert("worker".to_string(), RoleBinding {
            session_label: "worker".into(),
            current_session_id: None,
            daemon_session_uid: Some("uid-worker".into()),
        });
        let mut run = WorkflowRun::new(
            "wf-verbatim".into(), "test-wf".into(), wt.to_str().unwrap().into(),
            roles, "worker".into(), BTreeMap::new(), Some("g".into()), BTreeMap::new(), 0,
        );
        // A goal containing literal template braces — must NOT be expanded.
        run.pending_activation = Some(PendingActivation {
            activation_id: 1, target_role: "worker".into(), iteration: 1,
            trigger: TriggerKind::Initial,
            raw_prompt: "implement {{ roles.worker.last_message }} literally".into(),
            verbatim: true, needs_fresh_reset: false, is_initial: true,
            phase: ActivationPhase::Queued, rendered_prompt: None,
            pre_clear_snapshot: None, enter_fire_at_ms: None,
        });
        run::save(&run).unwrap();
        let _ = advance_finalization(&ctx("wf-verbatim", &wf, &wt, 1, 0), &PtyModeTracker::new(), |_| Ok(())).unwrap();
        let rendered = run::load_one("wf-verbatim").unwrap().pending_activation.unwrap().rendered_prompt.unwrap();
        assert_eq!(
            rendered, "implement {{ roles.worker.last_message }} literally",
            "verbatim goal: template braces left untouched"
        );
        std::env::remove_var("HOME");
    }

    // ---- Render-before-append: first vs subsequent template --------------

    #[test]
    fn render_picks_subsequent_template_on_second_activation() {
        let _g = env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());
        let wt = home.path().join("wt");
        let dir = claude_dir(home.path(), &wt);
        write_transcript(&dir, "wkr", &[(true, "worker turn")]);
        write_transcript(&dir, "mgr", &[(true, "mgr earlier")]);

        let wf = workflow();
        // Manager already has a prior activation entry -> prior_activations == 1.
        seed_run(
            "wf-subseq",
            &wt,
            "manager",
            4,
            TriggerKind::StaticIdle { from_role: "reviewer".into() },
            "",
            false,
            &[("worker", Some("wkr")), ("manager", Some("mgr"))],
            vec![hist("worker", 1, Some("wkr"), 0), hist("manager", 2, Some("mgr"), 0)],
        );
        // Only render.
        let _ = advance_finalization(&ctx("wf-subseq", &wf, &wt, 1, 0), &PtyModeTracker::new(), |_| Ok(())).unwrap();
        let run = run::load_one("wf-subseq").unwrap();
        let rendered = run.pending_activation.unwrap().rendered_prompt.unwrap();
        assert!(rendered.starts_with("Manager SUBSEQUENT."), "2nd activation uses subsequent_activation_prompt: {rendered:?}");
        std::env::remove_var("HOME");
    }

    // ---- MCP raw prompt rendered; empty falls back -----------------------

    #[test]
    fn mcp_raw_prompt_is_rendered_then_empty_falls_back() {
        let _g = env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());
        let wt = home.path().join("wt");
        let dir = claude_dir(home.path(), &wt);
        write_transcript(&dir, "wkr", &[(true, "step A"), (true, "step B")]);

        let wf = workflow();
        // MCP transition carrying a raw prompt WITH a template var.
        seed_run(
            "wf-mcp",
            &wt,
            "worker",
            3,
            TriggerKind::McpTransition { from_role: "manager".into(), prompt: "fix it".into(), event_id: "ev1".into() },
            "Manager says fix: {{ roles.worker.this_turn }}",
            false,
            &[("worker", Some("wkr")), ("manager", Some("mgr"))],
            vec![hist("worker", 1, Some("wkr"), 1)],
        );
        let _ = advance_finalization(&ctx("wf-mcp", &wf, &wt, 1, 0), &PtyModeTracker::new(), |_| Ok(())).unwrap();
        let rendered = run::load_one("wf-mcp").unwrap().pending_activation.unwrap().rendered_prompt.unwrap();
        assert!(rendered.starts_with("Manager says fix:"), "raw MCP prompt rendered: {rendered:?}");
        // this_turn from worker history entry text_messages_at_start=1 -> only "step B".
        assert!(rendered.contains("step B"));
        assert!(!rendered.contains("step A"), "this_turn sliced from offset 1");

        // Empty MCP prompt -> falls back to the worker's template... worker has
        // none, so empty. Use the reviewer (fresh, persistent-free) to show the
        // fallback chooses activation_prompt. Build a fresh run.
        seed_run(
            "wf-mcp-empty",
            &wt,
            "manager",
            2,
            TriggerKind::McpTransition { from_role: "reviewer".into(), prompt: "".into(), event_id: "ev2".into() },
            "", // empty -> fallback
            false,
            &[("worker", Some("wkr")), ("manager", Some("mgr"))],
            vec![hist("worker", 1, Some("wkr"), 0)],
        );
        let _ = advance_finalization(&ctx("wf-mcp-empty", &wf, &wt, 1, 0), &PtyModeTracker::new(), |_| Ok(())).unwrap();
        let rendered2 = run::load_one("wf-mcp-empty").unwrap().pending_activation.unwrap().rendered_prompt.unwrap();
        assert!(rendered2.starts_with("Manager FIRST."), "empty prompt falls back to activation_prompt: {rendered2:?}");
        std::env::remove_var("HOME");
    }

    // ---- Fresh role: counts 0, render-before-/clear, deliver-before-rebind,
    //      rebind durability -------------------------------------------------

    #[test]
    fn fresh_finalization_counts_zero_delivers_before_rebind_then_rebinds() {
        let _g = env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());
        let wt = home.path().join("wt");
        let dir = claude_dir(home.path(), &wt);
        // Reviewer's PRE-/clear transcript has content its template references.
        write_transcript(&dir, "rev-old", &[(true, "previous review note")]);

        let wf = workflow();
        seed_run(
            "wf-fresh",
            &wt,
            "reviewer",
            2,
            TriggerKind::StaticIdle { from_role: "worker".into() },
            "",
            true, // fresh
            &[("worker", Some("wkr")), ("reviewer", Some("rev-old")), ("manager", Some("mgr"))],
            vec![hist("worker", 1, Some("wkr"), 0)],
        );

        let gap = 10_000u64;
        let tracker = PtyModeTracker::new();
        let mut writes = Vec::new();

        // 1) Render — must see PRE-/clear reviewer content (render-before-/clear).
        let s = advance_finalization(&ctx("wf-fresh", &wf, &wt, 1000, gap), &tracker, |b| { writes.push(b.to_vec()); Ok(()) }).unwrap();
        assert_eq!(s, FinalizeStep::Advanced(ActivationPhase::Rendered));
        let rendered = run::load_one("wf-fresh").unwrap().pending_activation.unwrap().rendered_prompt.unwrap();
        assert!(rendered.contains("previous review note"), "render-before-/clear sees old transcript: {rendered:?}");

        // 2) Rendered -> ClearBodySent: /clear body written, baseline reset, sid cleared.
        let s = advance_finalization(&ctx("wf-fresh", &wf, &wt, 1000, gap), &tracker, |b| { writes.push(b.to_vec()); Ok(()) }).unwrap();
        assert_eq!(s, FinalizeStep::Advanced(ActivationPhase::ClearBodySent));
        assert_eq!(String::from_utf8(writes.last().unwrap().clone()).unwrap(), "/clear");
        let run = run::load_one("wf-fresh").unwrap();
        assert!(run.role_sessions["reviewer"].current_session_id.is_none(), "sid cleared");
        assert_eq!(run.role_baselines.get("reviewer").map(|b| b.assistant_count), Some(0));

        // /clear Enter is gated on the gap: not yet.
        assert_eq!(
            advance_finalization(&ctx("wf-fresh", &wf, &wt, 1000, gap), &tracker, |_| Ok(())).unwrap(),
            FinalizeStep::Blocked
        );

        // 3) Gap elapsed: fire /clear Enter + append (FRESH counts both 0).
        let s = advance_finalization(&ctx("wf-fresh", &wf, &wt, 1000 + gap, gap), &tracker, |b| { writes.push(b.to_vec()); Ok(()) }).unwrap();
        assert_eq!(s, FinalizeStep::Advanced(ActivationPhase::Appended));
        let run = run::load_one("wf-fresh").unwrap();
        let entry = run.history.iter().rev().find(|h| h.role == "reviewer").unwrap();
        assert_eq!(entry.assistant_count_at_start, 0, "fresh: count 0, NOT old transcript");
        assert_eq!(entry.text_messages_at_start, 0, "fresh: text 0");
        assert!(entry.session_id.is_none(), "session_id pending for fresh");
        // No NoHistoryEntry now; but the gate stays inert via NoTranscriptId
        // because current_session_id is still None.
        assert!(run.role_sessions["reviewer"].current_session_id.is_none());

        // 4) Deliver prompt body, then Enter (after gap).
        let s = advance_finalization(&ctx("wf-fresh", &wf, &wt, 1000 + gap, gap), &tracker, |b| { writes.push(b.to_vec()); Ok(()) }).unwrap();
        assert_eq!(s, FinalizeStep::Advanced(ActivationPhase::BodySent));
        let body = String::from_utf8(writes.last().unwrap().clone()).unwrap();
        assert!(body.contains("previous review note"), "delivered the frozen rendered prompt");

        // Enter gated; then fire -> RebindPending (delivery done, NOT rebound yet).
        assert_eq!(
            advance_finalization(&ctx("wf-fresh", &wf, &wt, 1000 + gap, gap), &tracker, |_| Ok(())).unwrap(),
            FinalizeStep::Blocked
        );
        let s = advance_finalization(&ctx("wf-fresh", &wf, &wt, 1000 + 2 * gap, gap), &tracker, |b| { writes.push(b.to_vec()); Ok(()) }).unwrap();
        assert_eq!(s, FinalizeStep::Advanced(ActivationPhase::RebindPending));

        // DELIVER-BEFORE-REBIND: prompt delivered + entry appended, but sid
        // still pending across several ticks (no new transcript yet).
        for _ in 0..3 {
            assert_eq!(
                advance_finalization(&ctx("wf-fresh", &wf, &wt, 1000 + 3 * gap, gap), &tracker, |_| Ok(())).unwrap(),
                FinalizeStep::Blocked,
                "rebind stays pending while no new transcript exists"
            );
            let run = run::load_one("wf-fresh").unwrap();
            assert!(run.role_sessions["reviewer"].current_session_id.is_none(), "no false rebind value");
            assert!(run.history.iter().any(|h| h.role == "reviewer" && h.session_id.is_none()), "entry stays appended with pending sid");
        }

        // 5) Agent clears + writes a NEW transcript; discovery rebinds.
        write_transcript(&dir, "rev-new", &[(true, "fresh review reply")]);
        let s = advance_finalization(&ctx("wf-fresh", &wf, &wt, 1000 + 4 * gap, gap), &tracker, |_| Ok(())).unwrap();
        assert_eq!(s, FinalizeStep::Done);
        let run = run::load_one("wf-fresh").unwrap();
        assert!(run.pending_activation.is_none());
        assert_eq!(run.role_sessions["reviewer"].current_session_id.as_deref(), Some("rev-new"), "rebound to NEW transcript");
        let entry = run.history.iter().rev().find(|h| h.role == "reviewer").unwrap();
        assert_eq!(entry.session_id.as_deref(), Some("rev-new"), "entry session_id patched");

        // /clear body delivered exactly once; prompt body exactly once.
        let clears = writes.iter().filter(|w| w.as_slice() == b"/clear").count();
        assert_eq!(clears, 1, "exactly one /clear body");
        std::env::remove_var("HOME");
    }

    // ---- Codex kitty Enter encoding from live mode -----------------------

    #[test]
    fn enter_uses_kitty_encoding_when_mode_is_kitty() {
        let _g = env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());
        let wt = home.path().join("wt");
        let dir = claude_dir(home.path(), &wt);
        write_transcript(&dir, "mgr", &[(true, "ok")]);

        let wf = workflow();
        seed_run(
            "wf-kitty",
            &wt,
            "manager",
            2,
            TriggerKind::McpTransition { from_role: "reviewer".into(), prompt: "go".into(), event_id: "e".into() },
            "go",
            false,
            &[("manager", Some("mgr"))],
            vec![hist("worker", 1, Some("wkr"), 0)],
        );

        // Tracker observed the kitty keyboard push -> Enter must be CSI 13 u.
        // Feed at `base`, then advance `now_instant` past the quiet window so the
        // body-delivery gate sees the PTY as quiet (the feed records a wakeup).
        let base = Instant::now();
        let mut tracker = PtyModeTracker::new();
        tracker.feed(b"\x1b[>1u", base);
        let kitty_ctx = FinalizeCtx {
            now_instant: base + Duration::from_secs(3),
            ..ctx("wf-kitty", &wf, &wt, 1, 0)
        };

        let mut writes = Vec::new();
        let step = drive(&kitty_ctx, &tracker, &mut writes);
        assert_eq!(step, FinalizeStep::Done);
        // Body, then kitty Enter.
        assert_eq!(writes[0], b"go".to_vec());
        assert_eq!(writes[1], b"\x1b[13u".to_vec(), "kitty Enter encoding from live mode");
        std::env::remove_var("HOME");
    }

    // ---- Crash-resume mid-finalization: exactly once ---------------------

    #[test]
    fn restart_at_body_sent_resumes_without_duplicate_body() {
        let _g = env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());
        let wt = home.path().join("wt");
        let dir = claude_dir(home.path(), &wt);
        write_transcript(&dir, "mgr", &[(true, "ok")]);

        let wf = workflow();
        seed_run(
            "wf-resume",
            &wt,
            "manager",
            2,
            TriggerKind::McpTransition { from_role: "reviewer".into(), prompt: "deliver me".into(), event_id: "e".into() },
            "deliver me",
            false,
            &[("manager", Some("mgr"))],
            vec![hist("worker", 1, Some("wkr"), 0)],
        );

        let gap = 10_000u64;
        let tracker = PtyModeTracker::new();
        let mut writes = Vec::new();
        // Drive up to BodySent (body written, Enter pending due to gap).
        loop {
            let s = advance_finalization(&ctx("wf-resume", &wf, &wt, 5_000, gap), &tracker, |b| { writes.push(b.to_vec()); Ok(()) }).unwrap();
            if s == FinalizeStep::Advanced(ActivationPhase::BodySent) { break; }
            assert!(matches!(s, FinalizeStep::Advanced(_)));
        }
        let body_writes = writes.iter().filter(|w| w.as_slice() == b"deliver me").count();
        assert_eq!(body_writes, 1);
        assert_eq!(run::load_one("wf-resume").unwrap().pending_activation.unwrap().phase, ActivationPhase::BodySent);

        // "Restart": a fresh advance reloads from disk at BodySent. With the gap
        // elapsed it fires ONLY the Enter — the body is NOT re-written, and the
        // per-iteration guard prevents a duplicate history entry.
        let mut writes2 = Vec::new();
        let s = advance_finalization(&ctx("wf-resume", &wf, &wt, 5_000 + gap, gap), &tracker, |b| { writes2.push(b.to_vec()); Ok(()) }).unwrap();
        assert_eq!(s, FinalizeStep::Done);
        assert_eq!(writes2, vec![b"\r".to_vec()], "resume fires only the Enter, no duplicate body");
        let run = run::load_one("wf-resume").unwrap();
        assert_eq!(run.history.iter().filter(|h| h.role == "manager" && h.iteration == 2).count(), 1, "exactly one history entry");
        std::env::remove_var("HOME");
    }

    // ---- TriggerKind survives a reload (no events re-read) ----------------

    #[test]
    fn trigger_kind_reconstructed_from_record_after_reload() {
        let _g = env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());
        let wt = home.path().join("wt");
        let dir = claude_dir(home.path(), &wt);
        write_transcript(&dir, "mgr", &[(true, "ok")]);

        let wf = workflow();
        seed_run(
            "wf-trig",
            &wt,
            "worker",
            3,
            TriggerKind::McpTransition { from_role: "manager".into(), prompt: "p".into(), event_id: "ev-xyz".into() },
            "p",
            false,
            &[("worker", Some("mgr"))],
            vec![hist("worker", 1, Some("mgr"), 0)],
        );
        // Render + append only.
        let _ = advance_finalization(&ctx("wf-trig", &wf, &wt, 1, 0), &PtyModeTracker::new(), |_| Ok(())).unwrap(); // render
        let _ = advance_finalization(&ctx("wf-trig", &wf, &wt, 1, 0), &PtyModeTracker::new(), |_| Ok(())).unwrap(); // append
        // Reload purely from disk (no events): the appended entry's trigger is
        // the McpTransition reconstructed from the persisted record.
        let run = run::load_one("wf-trig").unwrap();
        let entry = run.history.iter().rev().find(|h| h.role == "worker" && h.iteration == 3).unwrap();
        assert_eq!(
            entry.trigger,
            TriggerKind::McpTransition { from_role: "manager".into(), prompt: "p".into(), event_id: "ev-xyz".into() }
        );
        std::env::remove_var("HOME");
    }
}
