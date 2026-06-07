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

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::{Instant, Duration};

use crate::app::{engine_for_session_type, log_tick, new_session_uid, prepare_initial_prompt,
    respawn_existing_with_workflow_mcp, App, PendingWrite, SessionStatus, TerminalSession,
    Workspace, WorkflowSlotChoice, WorkflowSlotSource};
use crate::session::Session;
use crate::workflow::{self, run::MessageBaseline, toml_schema::Engine, RoleBinding, TriggerKind,
    Workflow, WorkflowRun};

/// Mutable + immutable references the controller needs from `App`.
/// Built fresh per call so the controller can't reach into unrelated
/// App state.
pub struct WorkflowControllerCtx<'a> {
    pub workflow_runs: &'a mut Vec<WorkflowRun>,
    pub workspaces: &'a mut Vec<Workspace>,
    pub workflows: &'a HashMap<String, Workflow>,
    /// Latest terminal cell dims; used when spawning new participant
    /// sessions so the PTY child starts sized to the visible area.
    pub last_term_size: (u16, u16),
    /// Read-only handle to the App's `Config`. Workflow participant
    /// spawns route through `session::spawn_agent_session`, which
    /// reads `Config::memory_cap_for(session_type)` to decide whether
    /// to wrap the spawn — without this, a runaway workflow agent
    /// would defeat the per-session memory cap.
    pub config: &'a crate::config::Config,
    /// Cached preflight result. Same gate as every other agent spawn:
    /// a `MemoryCap` is built only when *both* the user configured
    /// limits and preflight succeeded.
    pub cap_status: &'a crate::memory_cap::MemoryCapAvailability,
    /// Sender clone for memory-kill events. Cloned again into each
    /// per-session watcher thread spawned by `spawn_agent_session`.
    pub kill_tx: &'a std::sync::mpsc::Sender<crate::session_watch::MemoryKillEvent>,
    /// 12e: host_pool reference so workflow-respawn paths can
    /// route their `kill_daemon_session_if_attached` /
    /// `push_*_to_daemon_if_attached` calls through the
    /// session's pinned host.
    pub host_pool: &'a crate::host_pool::HostPool,
    /// 12e-r3 F1: snapshot of `App.active_host` taken at the
    /// top of the user-action handler (e.g. `A-f` launch).
    /// `spawn_workflow_session` reads this for the fresh-slot
    /// participant's host_id tag AND consults
    /// `host_pool.for_host(&active_host)` to dial the right
    /// daemon. Pre-r3 the field didn't exist; fresh slots
    /// were hardcoded to `HostId::local()`, so A-f on a
    /// non-default active_host produced participants that
    /// silently ran locally and were tagged local.
    pub active_host: cm_daemon::host_id::HostId,
    /// 11g-2: per-run buffer of channel-delivered events. Drained
    /// by `tick()` per run via [`take_pending_events`]; failed
    /// decisions re-push the source event at the front via
    /// [`requeue_pending_event_front`] so the next tick retries.
    /// Replaces the pre-11g-2 file-tail (`read_new_with_offsets`)
    /// path — `events.subscribe` broadcasts AFTER fsync, so the
    /// channel ordering equals `events.jsonl` append order and
    /// no file-read is needed.
    pub pending_workflow_events: &'a mut HashMap<
        String,
        std::collections::VecDeque<workflow::events::Event>,
    >,
}

impl<'a> WorkflowControllerCtx<'a> {
    /// 11g-2: drain the per-run pending-events buffer in FIFO
    /// order. Mirror of `App::take_pending_workflow_events` for
    /// the controller's borrow scope. Returns an empty Vec when
    /// no events are pending.
    pub(crate) fn take_pending_events(
        &mut self,
        run_id: &str,
    ) -> Vec<workflow::events::Event> {
        match self.pending_workflow_events.get_mut(run_id) {
            Some(deque) => deque.drain(..).collect(),
            None => Vec::new(),
        }
    }

    /// 11g-2: front-push retry. A failed decision (apply errored,
    /// session missing, etc.) re-pushes the source event so the
    /// next tick re-processes it. Same retry semantics the
    /// pre-11g-2 "leave events_offset un-advanced" pattern
    /// provided via file-tail; the deque IS the bookmark now.
    pub(crate) fn requeue_pending_event_front(
        &mut self,
        run_id: &str,
        event: workflow::events::Event,
    ) {
        self.pending_workflow_events
            .entry(run_id.to_string())
            .or_default()
            .push_front(event);
    }
}

/// App-level side effect requested by the controller. The dispatcher
/// in `App` matches on these and invokes the corresponding `App`
/// method; the controller never touches `&mut App`.
#[derive(Debug, Clone)]
pub enum WorkflowAction {
    /// Persist `~/.cm/tui-sessions.json` — sessions inside a workflow
    /// participant just had `pending_clear` / `pending_prompt` /
    /// `transcript_id` mutated and the manifest needs to reflect that.
    SaveSessionManifest,
    /// Show `msg` in the bottom status bar.
    SetStatusMsg(String),
}




/// Locate the `(workspace_index, session_index)` of the session tagged
/// as `role` for workflow run `run_id`. Searches across all workspaces
/// because workflow tags on the session itself are the source of truth
/// — the run's stored `task_key` can drift away from reality.
pub fn locate_workflow_session(
    workspaces: &[Workspace],
    run_id: &str,
    role: &str,
) -> Option<(usize, usize)> {
    for (wi, ws) in workspaces.iter().enumerate() {
        for (si, ts) in ws.sessions.iter().enumerate() {
            if ts.workflow_run_id.as_deref() == Some(run_id)
                && ts.workflow_role.as_deref() == Some(role)
            {
                return Some((wi, si));
            }
        }
    }
    None
}

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

impl<'a> WorkflowControllerCtx<'a> {




    /// Keep `role_sessions.current_session_id` aligned with the live
    /// `TerminalSession.transcript_id`. Nothing else.
    ///
    /// 10d-2c-1 review round-6 (F1): collect the (role, live_sid)
    /// updates that need applying for this run, then push them
    /// through `workflow::run::modify` so the on-disk run
    /// reflects daemon-written advances (active_role, iteration,
    /// status, events_offset, daemon-routed history entries)
    /// alongside our TUI-only `role_sessions[*].current_session_id`
    /// updates. Pre-fix this did `run::save(&in_mem_copy)` which
    /// wholesale-clobbered any daemon write made between TUI's
    /// load and TUI's save — the named acceptance criterion bug.
    fn sync_role_session_ids(&mut self) {
        let run_count = self.workflow_runs.len();
        for idx in 0..run_count {
            if !self.workflow_runs[idx].is_active() {
                continue;
            }
            let run_id = self.workflow_runs[idx].run_id.clone();
            let role_names: Vec<String> = self.workflow_runs[idx]
                .role_sessions
                .keys()
                .cloned()
                .collect();
            // Gather (role, new_sid, new_daemon_uid) tuples. Round-5
            // F1: also re-sync `daemon_session_uid` from the live
            // `TerminalSession.session.daemon_session_uid`. Pre-r5
            // this only synced transcript_id; the daemon-poller's
            // ownership gate would miss a session that lost or
            // gained its daemon attachment between writes.
            let mut updates: Vec<(String, Option<String>, Option<String>)> =
                Vec::new();
            for role in role_names {
                let Some((ti, si)) = locate_workflow_session(self.workspaces, &run_id, &role)
                else {
                    continue;
                };

                let live = self.workspaces[ti].sessions[si].transcript_id.clone();
                let live_daemon_uid =
                    self.workspaces[ti].sessions[si].session.daemon_session_uid.clone();
                let binding = self.workflow_runs[idx]
                    .role_sessions
                    .get(&role);
                let binding_sid = binding
                    .and_then(|b| b.current_session_id.clone());
                let binding_daemon_uid = binding
                    .and_then(|b| b.daemon_session_uid.clone());
                if live != binding_sid || live_daemon_uid != binding_daemon_uid {
                    updates.push((role, live, live_daemon_uid));
                }
            }
            if updates.is_empty() {
                continue;
            }
            // Apply via modify so daemon writes survive. Closure
            // touches role_sessions only (TUI-owned).
            let updates_for_closure = updates.clone();
            let updated = workflow::run::modify(&run_id, move |r| {
                for (role, new_sid, new_daemon_uid) in updates_for_closure {
                    if let Some(b) = r.role_sessions.get_mut(&role) {
                        b.current_session_id = new_sid;
                        b.daemon_session_uid = new_daemon_uid;
                    }
                }
            });
            if let Ok(updated) = updated {
                self.workflow_runs[idx] = updated;
            } else {
                // Modify failed (e.g., run was deleted off disk).
                // Mirror the updates in memory anyway so subsequent
                // in-process logic doesn't see stale bindings.
                for (role, new_sid, new_daemon_uid) in updates {
                    if let Some(b) =
                        self.workflow_runs[idx].role_sessions.get_mut(&role)
                    {
                        b.current_session_id = new_sid;
                        b.daemon_session_uid = new_daemon_uid;
                    }
                }
            }
        }
    }





}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{SessionStatus, TerminalSession, Workspace};
    use crate::session::Session;
    use crate::workflow::run::{MessageBaseline, RoleBinding, RunStatus};
    use crate::workflow::toml_schema::{Context, Engine, Role, Transition, TriggerOn, Workflow};
    use std::collections::{BTreeMap, HashMap};
    use std::path::PathBuf;
    use std::time::Instant;

    /// Build a TerminalSession backed by a `/bin/true` PTY child — the
    /// process exits immediately, but the resulting `Session` value is
    /// well-formed enough for the controller paths under test, which
    /// only inspect fields and never read from the PTY.
    fn stub_session(
        label: &str,
        session_type: &str,
        run_id: &str,
        role: &str,
        transcript_id: Option<&str>,
    ) -> TerminalSession {
        let session = Session::new("/bin/true", &[], 80, 24, None, HashMap::new(), None)
            .expect("test PTY session");
        TerminalSession {
            uid: format!("uid-{}-{}", run_id, role),
            label: label.to_string(),
            session_type: session_type.to_string(),
            session,
            status: SessionStatus::Idle,
            last_write_at: None,
            transcript_id: transcript_id.map(str::to_string),
            generation: 0,
            pending_jsonl_files: None,
            hidden: true,
            idle_timeout_secs: 0,
            burst_threshold: 0,
            pending_prompt: None,
            pending_clear: None,
            workflow_run_id: Some(run_id.to_string()),
            workflow_role: Some(role.to_string()),
            last_delivery: None,
            task_id: None,
            notify_on_idle: false,
            pending_enter: None,
            created_at: Instant::now(),
            managed_by_uid: None,
            seeded_from_snapshot: None,
            preserved_last_exit: None,
            host_id: crate::hosts::HostId::local(),
        }
    }

    fn workspace_with(sessions: Vec<TerminalSession>, worktree: Option<PathBuf>) -> Workspace {
        Workspace {
            id: "ws-1".to_string(),
            name: "ws-1".to_string(),
            is_closed: false,
            is_cloud: false,
            repo_url: None,
            worktree_path: worktree,
            main_repo_path: None,
            worker_vm: None,
            worker_zone: None,
            sessions,
            tombstones: Vec::new(),
            is_pushing: false,
        }
    }

    fn role_with(engine: Engine, context: Context) -> Role {
        Role {
            engine,
            context,
            needs_mcp: true,
            activation_prompt: None,
            subsequent_activation_prompt: None,
        }
    }

    fn make_workflow(
        name: &str,
        roles: BTreeMap<String, Role>,
        role_order: Vec<String>,
        transitions: Vec<Transition>,
    ) -> Workflow {
        Workflow {
            name: name.to_string(),
            description: String::new(),
            roles,
            role_order,
            transitions,
        }
    }

    /// Inert cap-state values for tests. Tests don't spawn real agents
    /// (they use `/bin/true` via `Session::new` directly), so the
    /// values only need to exist for `WorkflowControllerCtx` field
    /// init — they're never consulted in test paths. Keep one shared
    /// channel + an "Unavailable" preflight result so even if a future
    /// test does land in `spawn_agent_session`, no real cgroup wrap
    /// happens.
    struct DummyCapState {
        config: crate::config::Config,
        cap_status: crate::memory_cap::MemoryCapAvailability,
        kill_tx: std::sync::mpsc::Sender<crate::session_watch::MemoryKillEvent>,
        // Held to keep the channel alive for the duration of the test.
        _kill_rx: std::sync::mpsc::Receiver<crate::session_watch::MemoryKillEvent>,
        /// 11g-2: the channel-driven pending-events buffer the
        /// controller drains from. Tests that don't push channel
        /// events leave this empty; tests that DO push synthetic
        /// events populate it before the tick.
        pending_events: HashMap<
            String,
            std::collections::VecDeque<workflow::events::Event>,
        >,
        /// 12e: host_pool field on `WorkflowControllerCtx`. Tests
        /// don't actually route any RPCs through this — workflow
        /// controller paths that DO call into the pool are
        /// covered by `host_pool::tests` directly. This dummy
        /// just satisfies the ctx-struct's lifetime/typing.
        host_pool: crate::host_pool::HostPool,
    }

    /// 11g-2 helper: push channel-delivered events into the
    /// per-run pending deque. Tests that pre-11g-2 wrote events
    /// to disk via `WorkflowEventsWriter::append_event` now push
    /// directly into the controller's input buffer instead.
    /// Events.jsonl writes can still happen alongside (the
    /// daemon's broadcaster does both atomically) but no test
    /// here exercises the daemon's broadcast path; we feed the
    /// controller directly.
    fn push_pending_events(
        dummy: &mut DummyCapState,
        run_id: &str,
        events: impl IntoIterator<Item = workflow::events::Event>,
    ) {
        let deque = dummy
            .pending_events
            .entry(run_id.to_string())
            .or_default();
        for ev in events {
            deque.push_back(ev);
        }
    }

    fn dummy_cap_state() -> DummyCapState {
        let (kill_tx, kill_rx) = std::sync::mpsc::channel();
        DummyCapState {
            config: crate::config::Config {
                api_url: String::new(),
                api_token: String::new(),
                gcp_project: String::new(),
                gcp_zone: String::new(),
                repos: HashMap::new(),
            },
            cap_status: crate::memory_cap::MemoryCapAvailability::Unavailable {
                reason: "test".into(),
            },
            kill_tx,
            _kill_rx: kill_rx,
            pending_events: HashMap::new(),
            host_pool: crate::host_pool::HostPool::from_config(
                &crate::hosts::HostsConfig::synthesized_local_default(),
            )
            .expect("synthesized-local pool"),
        }
    }

    fn make_run(run_id: &str, wf_name: &str, initial_role: &str) -> WorkflowRun {
        let mut role_sessions = BTreeMap::new();
        role_sessions.insert(
            "worker".to_string(),
            RoleBinding {
                session_label: "worker".to_string(),
                current_session_id: None,
                daemon_session_uid: None,
            },
        );
        role_sessions.insert(
            "reviewer".to_string(),
            RoleBinding {
                session_label: "reviewer".to_string(),
                current_session_id: None,
                daemon_session_uid: None,
            },
        );
        WorkflowRun::new(
            run_id.to_string(),
            wf_name.to_string(),
            "ws-1".to_string(),
            role_sessions,
            initial_role.to_string(),
            BTreeMap::new(),
            None,
            BTreeMap::new(),
            0,
        )
    }

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

    fn write_codex_meta(sid: &str, worktree: &std::path::Path) {
        let home = std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME set"));
        let dir = home.join(".codex/sessions/2026/05/11");
        std::fs::create_dir_all(&dir).expect("codex session dir");
        let line = serde_json::json!({
            "type": "session_meta",
            "payload": {
                "id": sid,
                "cwd": worktree.to_string_lossy(),
            }
        });
        std::fs::write(
            dir.join(format!("rollout-2026-05-11T00-00-00-{}.jsonl", sid)),
            format!("{}\n", line),
        )
        .expect("codex transcript");
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

    /// 10d-2c-1 review round-6 (F1) — named acceptance test.
    /// Simulate the bug shape: daemon advances state.json
    /// (active_role: worker → reviewer) between TUI's load and
    /// TUI's save. Pre-fix `sync_role_session_ids` would
    /// wholesale-save its in-memory copy and clobber the
    /// daemon's `active_role`. Post-fix the modify-under-flock
    /// reads daemon's value and applies only TUI-owned
    /// `role_sessions[*].current_session_id` updates on top.
    #[test]
    fn sync_role_session_ids_preserves_daemon_advance() {
        with_temp_home(|| {
            let run_id = "wf_r6_sync_daemon";
            let mut run = make_run(run_id, "feedback", "worker");
            // Seed state.json on disk with TUI's stale view:
            // active_role = worker, reviewer binding empty.
            workflow::run::save(&run).expect("seed save");

            // Simulate daemon's advance happening on disk
            // between TUI's load and TUI's save:
            // active_role → reviewer, iteration bumped.
            workflow::run::modify(run_id, |r| {
                r.close_active_role(None);
                r.iteration += 1;
                r.active_role = Some("reviewer".into());
            })
            .expect("daemon-side modify");

            // TUI's in-memory copy still has the pre-daemon
            // shape (active=worker) and now also notices the
            // reviewer session's transcript_id changed → it
            // intends to update reviewer's role_sessions sid.
            // Mutate the in-mem copy to simulate the TUI's
            // stale view.
            run.active_role = Some("worker".into()); // stale
            run.iteration = 1; // stale
            // Build a workspace containing a reviewer session
            // tagged for this run; its transcript_id is what
            // sync_role_session_ids will propagate.
            let reviewer = stub_session(
                "reviewer",
                "claude",
                run_id,
                "reviewer",
                Some("reviewer-new-sid"),
            );
            let workspace = workspace_with(vec![reviewer], None);
            let mut workspaces = vec![workspace];

            let mut runs = vec![run];
            let workflows: HashMap<String, Workflow> = HashMap::new();
            let mut dummy = dummy_cap_state();
            {
                let mut ctx = WorkflowControllerCtx {
                    workflow_runs: &mut runs,
                    workspaces: &mut workspaces,
                    workflows: &workflows,
                    last_term_size: (80, 24),
                    config: &dummy.config,
                    cap_status: &dummy.cap_status,
                    kill_tx: &dummy.kill_tx,
                    pending_workflow_events: &mut dummy.pending_events,
                    host_pool: &dummy.host_pool,
                    active_host: cm_daemon::host_id::HostId::local(),
                };
                ctx.sync_role_session_ids();
            }

            // Load from disk — assertion: BOTH daemon's advance
            // AND TUI's role_sessions update landed.
            let post = workflow::run::load_one(run_id).expect("post load");
            assert_eq!(
                post.active_role.as_deref(),
                Some("reviewer"),
                "daemon's active_role advance must NOT be clobbered \
                 by TUI sync_role_session_ids (pre-fix bug)",
            );
            assert_eq!(
                post.iteration, 2,
                "daemon's iteration bump must survive",
            );
            assert_eq!(
                post.role_sessions
                    .get("reviewer")
                    .and_then(|b| b.current_session_id.clone())
                    .as_deref(),
                Some("reviewer-new-sid"),
                "TUI's role_sessions update must land on top of \
                 daemon's advance",
            );
        });
    }




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



    // ── Launch path tests ───────────────────────────────────────────
    //
    // Cover the controller's `launch_workflow` companion to F7's tick /
    // fire_transition extraction. All three tests use Existing slots
    // (with `needs_mcp = false` so the `--resume` respawn path is
    // skipped) — the New-slot path calls `Session::new` against a real
    // `claude`/`codex` binary, which would make these tests environment-
    // dependent. The role/binding/baseline plumbing is identical between
    // the two paths.

    /// Like `stub_session`, but with no workflow tags (so the launch
    /// path can attach them as it would for a freshly-Existing slot).
    /// Lets the caller pre-set `task_id` to verify `inherit_task_id`
    /// behavior.
    fn bare_session(
        label: &str,
        session_type: &str,
        task_id: Option<&str>,
    ) -> TerminalSession {
        let session = Session::new("/bin/true", &[], 80, 24, None, HashMap::new(), None)
            .expect("test PTY session");
        TerminalSession {
            uid: format!("uid-{}", label),
            label: label.to_string(),
            session_type: session_type.to_string(),
            session,
            status: SessionStatus::Idle,
            last_write_at: None,
            transcript_id: None,
            generation: 0,
            pending_jsonl_files: None,
            hidden: false,
            idle_timeout_secs: 0,
            burst_threshold: 0,
            pending_prompt: None,
            pending_clear: None,
            workflow_run_id: None,
            workflow_role: None,
            last_delivery: None,
            task_id: task_id.map(str::to_string),
            notify_on_idle: false,
            pending_enter: None,
            created_at: Instant::now(),
            managed_by_uid: None,
            seeded_from_snapshot: None,
            preserved_last_exit: None,
            host_id: crate::hosts::HostId::local(),
        }
    }

    /// Build a `feedback`-style two-role workflow with persistent
    /// context everywhere and `needs_mcp = false` so the launch path
    /// doesn't try to respawn participants with `--resume`.
    fn launch_test_workflow() -> Workflow {
        let mut roles = BTreeMap::new();
        let mut worker = role_with(Engine::ClaudeCode, Context::Persistent);
        worker.needs_mcp = false;
        let mut reviewer = role_with(Engine::ClaudeCode, Context::Persistent);
        reviewer.needs_mcp = false;
        roles.insert("worker".to_string(), worker);
        roles.insert("reviewer".to_string(), reviewer);
        make_workflow(
            "feedback",
            roles,
            vec!["worker".to_string(), "reviewer".to_string()],
            vec![Transition {
                from: "worker".to_string(),
                on: TriggerOn::Idle,
                to: "reviewer".to_string(),
            }],
        )
    }

    /// Two-role workspace with bare worker + reviewer sessions ready
    /// to be claimed by Existing slots. `worker_task_id` lets a test
    /// pre-set the worker's `task_id` to exercise `inherit_task_id`.
    fn launch_test_workspace(worker_task_id: Option<&str>) -> Vec<Workspace> {
        let worker = bare_session("worker", "claude", worker_task_id);
        let reviewer = bare_session("reviewer", "claude", None);
        vec![workspace_with(vec![worker, reviewer], None)]
    }

    /// Existing slots for a worker (index 0) + reviewer (index 1).
    fn launch_test_slots() -> Vec<crate::app::WorkflowSlotChoice> {
        use crate::app::{WorkflowSlotChoice, WorkflowSlotSource};
        vec![
            WorkflowSlotChoice {
                role: "worker".to_string(),
                options: vec![WorkflowSlotSource::Existing(0)],
                option_index: 0,
            },
            WorkflowSlotChoice {
                role: "reviewer".to_string(),
                options: vec![WorkflowSlotSource::Existing(1)],
                option_index: 0,
            },
        ]
    }






}
