//! Workflow tick / transition controller.
//!
//! Owns the per-iteration scheduling logic that drives each active
//! `WorkflowRun` forward: read new MCP events, detect static `on_idle`
//! transitions, fire role activations, render activation prompts, queue
//! `/clear` for fresh-context roles, mark runs done. Extracted out of
//! `App` so the scheduling state machine is testable without booting a
//! TUI and so `App` itself stops carrying ~500 lines of workflow-specific
//! logic.
//!
//! Pattern (mirrors the `input.rs` extraction): the controller takes a
//! [`WorkflowControllerCtx`] holding `&mut` references to just the App
//! state it needs (`workflow_runs`, `workspaces`) plus a `&` reference
//! to the workflow definitions. It mutates that state in place and emits
//! [`WorkflowAction`] values for App-level side effects (status bar,
//! manifest persistence) the dispatcher in `App` then applies. The
//! controller never touches `&mut App`.

use std::collections::BTreeMap;
use std::path::Path;

use crate::workflow::{self, toml_schema::Engine, WorkflowRun};


/// `RoleResolver` impl that templates use to expand
/// `{{ roles.<role>.user[N] }}` / `assistant[N]` / `plan` and
/// `{{ goal }}` references. Built fresh inside `fire_transition` and
/// `deliver_initial_workflow_prompt` from the live workflow run plus
/// per-role engines (derived from each role's bound session type).
pub(crate) struct WorkflowResolver<'a> {
    pub run: &'a WorkflowRun,
    pub worktree_path: Option<&'a Path>,
    /// Engine to use for each role, derived from the actual bound session's
    /// `session_type` at resolver construction time.
    pub role_engines: BTreeMap<String, Engine>,
}

impl<'a> WorkflowResolver<'a> {
    fn lookup(&self, role: &str) -> Option<(Engine, &'a Path, &'a str)> {
        let engine = self.role_engines.get(role).cloned()?;
        let binding = self.run.role_sessions.get(role)?;
        let session_id = binding.current_session_id.as_deref()?;
        let worktree = self.worktree_path?;
        Some((engine, worktree, session_id))
    }
}

impl<'a> workflow::template::RoleResolver for WorkflowResolver<'a> {
    fn user_messages(&self, role: &str) -> Vec<String> {
        let Some((engine, wt, sid)) = self.lookup(role) else {
            return Vec::new();
        };
        let offset = self
            .run
            .role_baselines
            .get(role)
            .map(|b| b.user_count)
            .unwrap_or(0);
        workflow::transcript::list_messages(&engine, wt, sid, workflow::transcript::MessageKind::User)
            .into_iter()
            .skip(offset)
            .collect()
    }

    fn assistant_messages(&self, role: &str) -> Vec<String> {
        let Some((engine, wt, sid)) = self.lookup(role) else {
            return Vec::new();
        };
        let offset = self
            .run
            .role_baselines
            .get(role)
            .map(|b| b.assistant_count)
            .unwrap_or(0);
        workflow::transcript::list_messages(
            &engine,
            wt,
            sid,
            workflow::transcript::MessageKind::Assistant,
        )
        .into_iter()
        .skip(offset)
        .collect()
    }

    fn assistant_since_activation(&self, role: &str) -> Vec<String> {
        let Some((engine, wt, sid)) = self.lookup(role) else {
            return Vec::new();
        };
        // The role's most recent activation history entry holds the full
        // list_messages count snapshotted at activation. Slicing the
        // current list_messages from that offset gives everything the
        // role has said during this activation.
        let offset = self
            .run
            .history
            .iter()
            .rev()
            .find(|h| h.role == role)
            .map(|h| h.text_messages_at_start)
            .unwrap_or(0);
        workflow::transcript::list_messages(
            &engine,
            wt,
            sid,
            workflow::transcript::MessageKind::Assistant,
        )
        .into_iter()
        .skip(offset)
        .collect()
    }

    fn prior_user_messages(&self, role: &str) -> Vec<String> {
        let Some((engine, wt, sid)) = self.lookup(role) else {
            return Vec::new();
        };
        let baseline = self
            .run
            .role_baselines
            .get(role)
            .map(|b| b.user_count)
            .unwrap_or(0);
        workflow::transcript::list_messages(&engine, wt, sid, workflow::transcript::MessageKind::User)
            .into_iter()
            .take(baseline)
            .collect()
    }

    fn prior_assistant_messages(&self, role: &str) -> Vec<String> {
        let Some((engine, wt, sid)) = self.lookup(role) else {
            return Vec::new();
        };
        let baseline = self
            .run
            .role_baselines
            .get(role)
            .map(|b| b.assistant_count)
            .unwrap_or(0);
        workflow::transcript::list_messages(
            &engine,
            wt,
            sid,
            workflow::transcript::MessageKind::Assistant,
        )
        .into_iter()
        .take(baseline)
        .collect()
    }

    fn latest_plan(&self, role: &str) -> Option<String> {
        // Prefer the launch-time snapshot. The live-transcript fallback only
        // returns Some when the role's last assistant line is still an
        // ExitPlanMode tool_use, which usually isn't true by the time
        // downstream roles activate.
        if let Some(plan) = self.run.role_plans.get(role) {
            if !plan.is_empty() {
                return Some(plan.clone());
            }
        }
        let (engine, wt, sid) = self.lookup(role)?;
        workflow::transcript::latest_plan(&engine, wt, sid)
    }

    fn goal(&self) -> Option<String> {
        self.run.goal.clone()
    }

    fn rejected_findings(&self) -> Vec<String> {
        self.run
            .rejected_findings
            .iter()
            .map(|r| r.text.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::run::{MessageBaseline, RoleBinding};
    use crate::workflow::toml_schema::Engine;
    use std::collections::BTreeMap;


    /// Run `f` against an isolated `HOME` so any `~/.cm/workflow-runs`
    /// reads or writes the controller does land in a temp dir, not the
    /// developer's real run directory. Mirrors the pattern used by
    /// `workflow::events::tests::with_temp_home` and
    /// `workflow::transcript::tests`.
    fn with_temp_home<F: FnOnce()>(f: F) {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let orig = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", tmp.path()); }
        f();
        if let Some(o) = orig {
            unsafe { std::env::set_var("HOME", o); }
        } else {
            unsafe { std::env::remove_var("HOME"); }
        }
    }


    /// F1/F4 gate-decision parity. Drive the TUI's
    /// static-idle gate path AND the daemon's `poll_once`
    /// gate path with the same inputs (same run, same workflow,
    /// same transcript). Both must agree on fire/skip.
    ///
    /// 2c-2-2-b's original parity test only covered resolver
    /// OUTPUT (template rendering). This expansion covers the
    /// gate's FIRE decision — exactly the surface where F1
    /// (different baseline source) and F4 (different empty-prompt
    /// behavior) silently diverged.
    #[test]
    fn gate_fire_decisions_parity_between_tui_and_daemon() {
        with_temp_home(|| {
            let home =
                std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME"));
            let wt = home.join("wt-gate-parity");
            std::fs::create_dir_all(&wt).expect("mkdir");
            let wt_str = wt.to_str().unwrap();
            let encoded = wt_str.replace('/', "-").replace('.', "-");
            let proj = home.join(format!(".claude/projects/{}", encoded));
            std::fs::create_dir_all(&proj).expect("mkdir proj");
            // One complete assistant turn — count = 1, idle = true.
            std::fs::write(
                proj.join("sid-w.jsonl"),
                r##"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"complete"}]}}"##,
            )
            .expect("write transcript");

            // Build a run with active_role="worker" and a
            // history entry for worker with
            // assistant_count_at_start=0 (baseline). The active
            // assistant_count is now 1, so the gate should fire
            // on both sides.
            let mut role_sessions = BTreeMap::new();
            role_sessions.insert(
                "worker".to_string(),
                RoleBinding {
                    session_label: "worker".to_string(),
                    current_session_id: Some("sid-w".to_string()),
                    daemon_session_uid: None,
                },
            );
            let baselines = BTreeMap::new();
            let run = WorkflowRun::new(
                "wf-gate-parity".to_string(),
                "feedback".to_string(),
                "/tmp/gate-parity".to_string(),
                role_sessions,
                "worker".to_string(),
                baselines,
                None,
                BTreeMap::new(),
                0,
            );

            // TUI gate behavior: count_assistant_turns >
            // active_assistant_start_count AND is_idle. The TUI's
            // `tick_local` does this inline (controller.rs:1093).
            // We replicate the predicate here for the parity test.
            let tui_baseline = run.active_assistant_start_count().unwrap_or(0);
            let tui_count = workflow::transcript::count_messages(
                &Engine::ClaudeCode,
                &wt,
                "sid-w",
                workflow::transcript::MessageKind::Assistant,
            );
            let tui_idle = workflow::transcript::role_turn_complete(
                &Engine::ClaudeCode,
                &wt,
                "sid-w",
            );
            let tui_would_fire = tui_count > tui_baseline && tui_idle;

            // Daemon gate behavior: same predicate via the
            // bundled helper.
            let dae_would_fire =
                workflow::transcript::assistant_turn_completed_since(
                    &Engine::ClaudeCode,
                    &wt,
                    "sid-w",
                    run.active_assistant_start_count().unwrap_or(0),
                );

            assert_eq!(
                tui_would_fire, dae_would_fire,
                "gate decisions diverge: TUI={}, daemon={} \
                 (count={}, baseline={}, idle={})",
                tui_would_fire, dae_would_fire, tui_count, tui_baseline, tui_idle,
            );
            // Should be TRUE for this scenario — both fire.
            assert!(
                tui_would_fire,
                "test setup: both should fire (1 turn > 0 baseline, idle)",
            );
        });
    }

    /// Resolver parity (T6). Daemon's `DaemonWorkflowResolver`
    /// and TUI's `WorkflowResolver` must produce byte-identical
    /// rendered output for the same logical inputs (workflow_def
    /// + run + role bindings + worktree + role engines).
    ///
    /// **The contract**: any divergence here means a future
    /// behavior change drifts between daemon-driven and TUI-driven
    /// static-idle fires. If a future read-path needs to diverge
    /// (e.g., daemon reads from a different transcript source),
    /// surface it as an EXPLICIT trait method, don't silently
    /// fork.
    #[test]
    fn daemon_and_tui_resolvers_produce_identical_template_output() {
        with_temp_home(|| {
            let home =
                std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME"));
            let wt = home.join("wt-parity");
            std::fs::create_dir_all(&wt).expect("mkdir wt");
            let wt_str = wt.to_str().unwrap();
            let encoded = wt_str.replace('/', "-").replace('.', "-");
            let proj = home.join(format!(".claude/projects/{}", encoded));
            std::fs::create_dir_all(&proj).expect("mkdir proj");
            std::fs::write(
                proj.join("sid-w.jsonl"),
                r##"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"first prompt"}]}}
{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"answer one"}]}}
{"type":"user","message":{"role":"user","content":[{"type":"text","text":"second prompt"}]}}
{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"answer two"}]}}
"##,
            )
            .expect("write transcript");

            // Build the run with worker bound + a baseline that
            // skips the first user/assistant pair (so prior_* vs
            // current_* slicing diverges meaningfully across the
            // two resolvers if there's any drift).
            let mut role_sessions = BTreeMap::new();
            role_sessions.insert(
                "worker".to_string(),
                RoleBinding {
                    session_label: "worker".to_string(),
                    current_session_id: Some("sid-w".to_string()),
                    daemon_session_uid: None,
                },
            );
            let mut baselines = BTreeMap::new();
            baselines.insert(
                "worker".to_string(),
                MessageBaseline {
                    user_count: 1,
                    assistant_count: 1,
                },
            );
            let mut run = WorkflowRun::new(
                "wf-parity".to_string(),
                "feedback".to_string(),
                "/parity".to_string(),
                role_sessions,
                "worker".to_string(),
                baselines,
                Some("the run goal".to_string()),
                BTreeMap::new(),
                0,
            );
            // `this_turn` slices the role's assistant messages from its latest
            // history entry's `text_messages_at_start`. Set it to 1 so this_turn
            // drops "answer one" and keeps "answer two" — a meaningful slice
            // that diverges if either resolver mis-reads the offset.
            run.history[0].text_messages_at_start = 1;
            // Populate rejected_findings so `{{ rejected_findings }}` is
            // non-empty (the daemon resolver must override the empty default).
            run.rejected_findings.push(workflow::run::RejectedFinding {
                text: "stop flagging the unused import".to_string(),
                recorded_at: 0,
                iteration: 1,
            });
            run.rejected_findings.push(workflow::run::RejectedFinding {
                text: "the TODO comment is intentional".to_string(),
                recorded_at: 0,
                iteration: 1,
            });

            let mut role_engines = BTreeMap::new();
            role_engines.insert("worker".to_string(), Engine::ClaudeCode);

            // TUI side.
            let tui_resolver = WorkflowResolver {
                run: &run,
                worktree_path: Some(wt.as_path()),
                role_engines: role_engines.clone(),
            };
            // Daemon side. Same crate boundary; importable here
            // because the parity test lives in the TUI crate,
            // which depends on cm-daemon.
            let dae_resolver =
                cm_daemon::workflow::poller::DaemonWorkflowResolver {
                    run: &run,
                    worktree_path: Some(wt.as_path()),
                    role_engines,
                };

            // Three template shapes covering: post-baseline,
            // pre-baseline, last-message alias, goal.
            for tpl in [
                "{{ roles.worker.user[0] }}",
                "{{ roles.worker.assistant[0] }}",
                "{{ roles.worker.prior_user[-1] }}",
                "{{ roles.worker.prior_assistant[-1] }}",
                "{{ roles.worker.last_message }}",
                "{{ goal }}",
                "review: {{ roles.worker.last_message }} ({{ goal }})",
                // Phase 3 parity additions: this_turn (sliced from the
                // activation's text_messages_at_start) and rejected_findings.
                "{{ roles.worker.this_turn }}",
                "{{ rejected_findings }}",
                "turn: {{ roles.worker.this_turn }} | rejected: {{ rejected_findings }}",
            ] {
                let tui_out = workflow::template::render(tpl, &tui_resolver);
                let dae_out = workflow::template::render(tpl, &dae_resolver);
                assert_eq!(
                    tui_out, dae_out,
                    "resolver outputs diverge for template {:?}: \
                     TUI={:?}, daemon={:?}",
                    tpl, tui_out, dae_out,
                );
            }
        });
    }


    // 11g-2: `malformed_event_line_does_not_wedge_offset` retired.
    // The file-tail malformed-line concern is gone with the file-tail
    // itself. Channel events are pre-deserialized by
    // `workflow_watch::drive_stream`; a malformed wire frame is logged
    // and dropped there, never reaching the controller. There's
    // nothing left for the controller to defend against.








    // 11g-2: `tui_local_tick_advances_events_offset_on_disk` retired.
    // The TuiLocal path is itself slated for deletion per A2 (daemon
    // mandatory since 10f makes the `_append_event` Python branch
    // vestigial). The test's other concern, events_offset advancing,
    // is retired per A4.





    // ============================================================
    // 10d-2c-1 review round-3 tests (F1, F3)
    // ============================================================

    // 11g-2: `daemon_routed_tick_advances_events_offset_on_disk`
    // retired. The test exclusively pinned events_offset advance
    // on disk after a daemon-routed tick, which is A4's retired
    // behavior. The "event re-processed?" companion assertion is
    // now structural (the per-run deque is drained on first read;
    // there's no re-replay surface to defend against). Coverage
    // for daemon-routed history append survives via
    // `daemon_routed_tick_persistent_role_no_reset_appends_history`
    // and the per-event-target-role family of tests above.



}
