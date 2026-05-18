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
}

impl<'a> WorkflowControllerCtx<'a> {
    /// Launch a workflow on a workspace: validate slots, spawn-or-bind a
    /// session per role, snapshot baselines, build and persist the
    /// `WorkflowRun`, and emit App-level actions (status bar, manifest
    /// save). Counterpart to `tick` / `fire_transition` already living
    /// here. Companion to F7's controller extraction (8c3c152) which
    /// deferred this method.
    ///
    /// All mutation happens against `self.workspaces` / `self.workflow_runs`
    /// in place — newly-spawned `TerminalSession`s are pushed straight
    /// onto the target workspace, and the new `WorkflowRun` lands at the
    /// end of `workflow_runs`. The dispatcher in `App` only needs to
    /// apply the returned `WorkflowAction`s (status messages and the
    /// final manifest save).
    pub fn launch_workflow(
        &mut self,
        ws_index: usize,
        workflow_name: &str,
        slots: Vec<WorkflowSlotChoice>,
        goal: Option<String>,
    ) -> Vec<WorkflowAction> {
        let mut actions: Vec<WorkflowAction> = Vec::new();
        let Some(wf) = self.workflows.get(workflow_name).cloned() else {
            actions.push(WorkflowAction::SetStatusMsg("Workflow not found".to_string()));
            return actions;
        };
        if ws_index >= self.workspaces.len() {
            return actions;
        }

        // Validate: `fresh` slots cannot use existing sessions. Also reject
        // duplicate existing-session assignments across slots.
        let mut existing_seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
        for slot in &slots {
            let role = match wf.roles.get(&slot.role) {
                Some(r) => r,
                None => {
                    actions.push(WorkflowAction::SetStatusMsg(format!(
                        "Unknown role: {}",
                        slot.role
                    )));
                    return actions;
                }
            };
            if let WorkflowSlotSource::Existing(si) = slot.source() {
                if matches!(role.context, workflow::toml_schema::Context::Fresh) {
                    actions.push(WorkflowAction::SetStatusMsg(format!(
                        "Role '{}' has fresh context; must use a new session",
                        slot.role
                    )));
                    return actions;
                }
                if !existing_seen.insert(*si) {
                    actions.push(WorkflowAction::SetStatusMsg(
                        "Two roles can't share the same existing session".to_string(),
                    ));
                    return actions;
                }
                // Engine match: an Existing slot's session type must agree
                // with the role's TOML-declared engine. The bound session
                // is the source of truth for the engine actually used at
                // runtime (templating, respawn paths), so a mismatch
                // silently overrides the TOML — almost certainly a bug.
                // Fail loudly instead.
                if let Some(ts) = self.workspaces[ws_index].sessions.get(*si) {
                    let session_engine = engine_for_session_type(&ts.session_type);
                    if session_engine != role.engine {
                        actions.push(WorkflowAction::SetStatusMsg(format!(
                            "Role '{}' declares engine '{}' but session '{}' is '{}'",
                            slot.role,
                            role.engine.as_session_type(),
                            ts.label,
                            ts.session_type,
                        )));
                        return actions;
                    }
                }
            }
        }

        let task_key = self.workspaces[ws_index].id.clone();
        let run_id = workflow::run::new_run_id();
        let worktree_path = self.workspaces[ws_index].worktree_path.clone();

        // Inherit task_id from the first existing session in a slot so new
        // workflow participants sit under the same task subheader in the
        // sidebar. If all slots are fresh, participants are workspace-level.
        let inherit_task_id: Option<String> = slots.iter().find_map(|slot| {
            if let WorkflowSlotSource::Existing(si) = slot.source() {
                self.workspaces[ws_index]
                    .sessions
                    .get(*si)
                    .and_then(|ts| ts.task_id.clone())
            } else {
                None
            }
        });

        // Spawn / bind sessions for each slot and build role_sessions.
        // For existing sessions we also snapshot the current user/assistant counts
        // so that templates like `{{ roles.worker.initial_prompt }}` point at the
        // first message *after* this launch, not the first message ever.
        let mut role_sessions: BTreeMap<String, RoleBinding> = BTreeMap::new();
        let mut role_baselines: BTreeMap<String, MessageBaseline> = BTreeMap::new();
        // Captured at launch from each role's pre-launch transcript tail. Lets
        // `{{ roles.<role>.plan }}` keep returning the plan the user accepted
        // even after the worker has produced more turns and the live
        // transcript's last assistant line is no longer the ExitPlanMode
        // tool_use.
        let mut role_plans: BTreeMap<String, String> = BTreeMap::new();
        // Sids already claimed by some live session in the TUI. Detection
        // below excludes these so an Existing-bound role with an empty pending
        // snapshot (e.g. a freshly-created pane that hasn't written its
        // transcript yet) can't accidentally claim a sid that already belongs
        // to a sibling session in the same worktree. Updated as the loop
        // binds new sids so later slots see them too.
        let mut bound_sids: std::collections::HashSet<String> = self
            .workspaces
            .iter()
            .flat_map(|w| w.sessions.iter())
            .filter_map(|s| s.transcript_id.clone())
            .collect();
        for slot in &slots {
            let role = &wf.roles[&slot.role];
            let (session_label, session_id, effective_engine) = match slot.source() {
                WorkflowSlotSource::Existing(si) => {
                    // Tag with workflow metadata, and if sid isn't known yet,
                    // try to detect it NOW (newest JSONL heuristic) so the
                    // baseline below is computed from the actual transcript.
                    let worktree_for_detect = self.workspaces[ws_index].worktree_path.clone();
                    let (cols, rows) = self.last_term_size;
                    let ts = match self.workspaces[ws_index].sessions.get_mut(*si) {
                        Some(s) => s,
                        None => continue,
                    };
                    ts.workflow_run_id = Some(run_id.clone());
                    ts.workflow_role = Some(slot.role.clone());
                    if let Some(wt) = worktree_for_detect.as_deref() {
                        if ts.transcript_id.is_none() {
                            // Augment the per-session pre-launch snapshot with
                            // sids already bound to other sessions so we don't
                            // accidentally claim a sibling's sid (e.g. when
                            // this pane was created before any transcript file
                            // existed and a sibling pane wrote its transcript
                            // between then and now).
                            let mut existing: Vec<String> =
                                ts.pending_jsonl_files.clone().unwrap_or_default();
                            existing.extend(bound_sids.iter().cloned());
                            let detected = match ts.session_type.as_str() {
                                "claude" => App::detect_session_id(wt, &existing),
                                "codex" => App::detect_codex_session_id(wt, &existing),
                                _ => None,
                            };
                            if let Some(sid) = detected {
                                bound_sids.insert(sid.clone());
                                ts.transcript_id = Some(sid);
                                ts.pending_jsonl_files = None;
                            }
                        } else if ts.session_type == "codex" {
                            let current_sid = ts.transcript_id.clone();
                            let existing: Vec<String> = bound_sids
                                .iter()
                                .filter(|sid| Some(sid.as_str()) != current_sid.as_deref())
                                .cloned()
                                .collect();
                            if let Some(sid) = App::detect_codex_session_id(wt, &existing) {
                                if current_sid.as_deref() != Some(sid.as_str()) {
                                    if let Some(old) = current_sid.as_ref() {
                                        bound_sids.remove(old);
                                    }
                                    bound_sids.insert(sid.clone());
                                    ts.rebind_transcript(Some(sid.clone()));
                                    ts.pending_jsonl_files = None;
                                    log_tick(
                                        &run_id,
                                        &format!(
                                            "launch-codex-rebind: role={} {:?} -> {}",
                                            slot.role, current_sid, sid
                                        ),
                                    );
                                }
                            }
                        }
                    }
                    let eng = engine_for_session_type(&ts.session_type);
                    let sid = ts.transcript_id.clone();
                    // Respawn-with-`--resume` only happens when the role
                    // genuinely needs the workflow MCP server wired in.
                    // Skipping it for `needs_mcp = false` roles (e.g. the
                    // feedback worker) preserves ephemeral process state
                    // — most importantly plan-mode UI — across launch.
                    let respawn_warning = if role.needs_mcp {
                        respawn_existing_with_workflow_mcp(
                            ts,
                            &eng,
                            &run_id,
                            &slot.role,
                            sid.as_deref(),
                            worktree_for_detect.as_deref(),
                            cols,
                            rows,
                            self.config,
                            self.cap_status,
                            self.kill_tx,
                        )
                    } else {
                        None
                    };
                    let session_label_clone = ts.label.clone();
                    let session_id_clone = ts.transcript_id.clone();
                    if let Some(msg) = respawn_warning {
                        actions.push(WorkflowAction::SetStatusMsg(msg));
                    }
                    (session_label_clone, session_id_clone, eng)
                }
                WorkflowSlotSource::New(engine) => {
                    match self.spawn_workflow_session(
                        ws_index,
                        &slot.role,
                        engine,
                        &run_id,
                        inherit_task_id.clone(),
                    ) {
                        Some((label, sid)) => (label, sid, engine.clone()),
                        None => {
                            actions.push(WorkflowAction::SetStatusMsg(format!(
                                "Failed to spawn {}",
                                slot.role
                            )));
                            return actions;
                        }
                    }
                }
            };
            // Compute baseline now, before the session does any new work.
            // Use count_messages (counts any turn) for assistant_count so the
            // idle gate sees a consistent picture later — it compares current
            // count against baseline.assistant_count at start. user_count
            // still uses list_messages (template slice uses text messages).
            let baseline = match (worktree_path.as_deref(), session_id.as_deref()) {
                (Some(wt), Some(sid)) => MessageBaseline {
                    user_count: workflow::transcript::list_messages(
                        &effective_engine,
                        wt,
                        sid,
                        workflow::transcript::MessageKind::User,
                    )
                    .len(),
                    assistant_count: workflow::transcript::count_messages(
                        &effective_engine,
                        wt,
                        sid,
                        workflow::transcript::MessageKind::Assistant,
                    ),
                },
                _ => MessageBaseline::default(),
            };
            let _ = role;
            // Snapshot the role's most-recent pre-launch ExitPlanMode plan, if
            // any. This must run BEFORE the role produces any new turns —
            // i.e. right here at launch, before activation prompts fire.
            if let (Some(wt), Some(sid)) = (worktree_path.as_deref(), session_id.as_deref()) {
                if let Some(plan) =
                    workflow::transcript::latest_plan(&effective_engine, wt, sid)
                {
                    role_plans.insert(slot.role.clone(), plan);
                }
            }
            role_baselines.insert(slot.role.clone(), baseline);
            role_sessions.insert(
                slot.role.clone(),
                RoleBinding {
                    session_label,
                    current_session_id: session_id,
                },
            );
        }

        // Initial active role = first in role_order.
        let initial_role = wf
            .role_order
            .first()
            .cloned()
            .unwrap_or_else(|| "worker".into());
        let run = WorkflowRun::new(
            run_id.clone(),
            workflow_name.to_string(),
            task_key,
            role_sessions,
            initial_role.clone(),
            role_baselines,
            goal,
            role_plans,
        );
        let _ = workflow::run::save(&run);
        self.workflow_runs.push(run);
        actions.push(WorkflowAction::SaveSessionManifest);
        actions.push(WorkflowAction::SetStatusMsg(format!(
            "Launched {} ({} roles, initial: {})",
            workflow_name,
            wf.role_order.len(),
            initial_role
        )));
        actions
    }

    /// Spawn a fresh `TerminalSession` for a workflow role, push it onto
    /// the target workspace, and return `(label, session_id)`. Mirror of
    /// the old `App::spawn_workflow_session`; lives here now because
    /// `launch_workflow` is its only caller.
    fn spawn_workflow_session(
        &mut self,
        ws_index: usize,
        role_name: &str,
        engine: &Engine,
        run_id: &str,
        task_id: Option<String>,
    ) -> Option<(String, Option<String>)> {
        let worktree_path = self.workspaces[ws_index].worktree_path.clone()?;
        let (cols, rows) = self.last_term_size;
        // Generate the uid first so the MCP config bakes the same value
        // the TerminalSession ends up holding.
        let session_uid = new_session_uid();
        let workflow_meta = crate::mcp_config::WorkflowMeta {
            run_id,
            role: role_name,
        };
        let (program, args) = match crate::mcp_config::build_args(
            engine,
            &session_uid,
            Some(workflow_meta),
            None,
        ) {
            Ok(v) => v,
            Err(e) => {
                // The caller will surface a generic "Failed to spawn"
                // status; the more specific args-build error is logged
                // here so it's visible in the workflow log.
                log_tick(run_id, &format!("spawn args: {}", e));
                return None;
            }
        };
        let pending = Some(match engine {
            Engine::ClaudeCode => App::list_jsonl_files(&worktree_path),
            Engine::Codex => App::list_codex_sessions(&worktree_path),
        });
        let session_type = engine.as_session_type().to_string();
        let sess = crate::session::spawn_agent_session(
            &session_type,
            &session_uid,
            &program,
            &args,
            cols,
            rows,
            Some(worktree_path.clone()),
            Default::default(),
            self.config,
            self.cap_status,
            self.kill_tx,
        )
        .ok()?;
        let label = role_name.to_string();
        let ts = TerminalSession {
            uid: session_uid,
            label: label.clone(),
            session_type,
            session: sess,
            // Start Idle — PTY startup noise isn't "work". Wakeup-burst
            // detection will flip to Running when the agent actually responds.
            status: SessionStatus::Idle,
            last_write_at: None,
            transcript_id: None,
            generation: 0,
            pending_jsonl_files: pending,
            // Participants default hidden — the workflow header carries the
            // aggregate indicator. Toggle per session with A-h.
            hidden: true,
            idle_timeout_secs: 0,
            burst_threshold: 0,
            pending_prompt: None,
            pending_clear: None,
            workflow_run_id: Some(run_id.to_string()),
            workflow_role: Some(role_name.to_string()),
            last_delivery: None,
            task_id,
            notify_on_idle: false,
            pending_enter: None,
            created_at: Instant::now(),
            managed_by_uid: None,
        };
        self.workspaces[ws_index].sessions.push(ts);
        Some((label, None))
    }

    /// Drive every active workflow run forward by one tick. Steps:
    ///   1. Sync each role's `current_session_id` with whatever the
    ///      live `TerminalSession.transcript_id` is (sids land async).
    ///   2. Read new events from `events.jsonl`; queue dynamic
    ///      transitions / done.
    ///   3. For runs with no events this tick, check whether the
    ///      active role's session has gone idle past its `start_count`
    ///      — if so, queue a static `on_idle` transition.
    ///   4. Apply the queued decisions in order.
    ///
    /// Returns the App-level actions the dispatcher should apply.
    pub fn tick(&mut self) -> Vec<WorkflowAction> {
        let mut actions = Vec::new();
        if self.workflow_runs.is_empty() {
            return actions;
        }

        // Keep role_sessions.current_session_id in sync with whatever the
        // live TerminalSession.session_id is. Needed because templating
        // (WorkflowResolver) reads from role_sessions, and sessions may get
        // their sid detected asynchronously (5-second poll) after launch.
        // This is a pure sync — no baseline / start_count mutation, which
        // would shift gates unpredictably.
        self.sync_role_session_ids();

        // Collect decisions first, then apply. (Avoids borrow issues with mutable
        // access to both self.workflow_runs and self.tasks.)
        #[derive(Debug)]
        enum Decision {
            ActivateStatic { run_id: String, to: String, from: String },
            ActivateDynamic {
                run_id: String,
                to: String,
                from: String,
                prompt: String,
                event_id: String,
            },
            Done { run_id: String, reason: String },
        }
        let mut decisions: Vec<Decision> = Vec::new();

        // Snapshot run states.
        let run_snapshots: Vec<(usize, String, u64, Option<String>, bool)> = self
            .workflow_runs
            .iter()
            .enumerate()
            .filter(|(_, r)| r.is_active())
            .map(|(i, r)| {
                (
                    i,
                    r.run_id.clone(),
                    r.events_offset,
                    r.active_role.clone(),
                    r.paused,
                )
            })
            .collect();

        for (idx, run_id, offset, active_role, paused) in run_snapshots {
            // Log per-session status so we can tell at a glance whether each
            // role ever reaches Running. Rate-limited by log_tick so this
            // doesn't flood. Now locates sessions by their workflow tags
            // (run_id+role), which is the source of truth — the workflow's
            // stored task_key can drift.
            {
                let role_names: Vec<String> = self.workflow_runs[idx]
                    .role_sessions
                    .keys()
                    .cloned()
                    .collect();
                let mut parts = Vec::new();
                for role in &role_names {
                    let status = match locate_workflow_session(self.workspaces, &run_id, role) {
                        Some((ti, si)) => {
                            let ts = &self.workspaces[ti].sessions[si];
                            format!(
                                "{:?}{}",
                                ts.status,
                                if ts.session.exited { "(exited)" } else { "" }
                            )
                        }
                        None => "<no session>".to_string(),
                    };
                    parts.push(format!("{}={}", role, status));
                }
                log_tick(
                    &run_id,
                    &format!(
                        "statuses: active={} [{}]",
                        active_role.as_deref().unwrap_or("?"),
                        parts.join(", ")
                    ),
                );
            }

            // Read new events regardless of paused state so the log stays in sync;
            // events are still recorded in history but not fired while paused.
            let (events, new_offset) = workflow::events::read_new(&run_id, offset);
            self.workflow_runs[idx].events_offset = new_offset;

            if paused {
                continue;
            }

            for ev in &events {
                match ev.kind() {
                    workflow::events::EventKind::Transition { to, prompt } => {
                        if let Some(from) = active_role.clone() {
                            decisions.push(Decision::ActivateDynamic {
                                run_id: run_id.clone(),
                                to,
                                from,
                                prompt,
                                event_id: ev.id.clone(),
                            });
                        }
                    }
                    workflow::events::EventKind::Done { reason } => {
                        decisions.push(Decision::Done {
                            run_id: run_id.clone(),
                            reason,
                        });
                    }
                    workflow::events::EventKind::Unknown => {}
                }
            }

            // If no dynamic event fired, check for static idle transition.
            if events.is_empty() {
                let Some(active) = active_role.as_deref() else { continue };
                let wf = self
                    .workflows
                    .get(&self.workflow_runs[idx].workflow_name)
                    .cloned();
                let Some(wf) = wf else { continue };
                // Locate by workflow tags — not by task_key + session_label,
                // which can drift.
                let Some((ti, si)) = locate_workflow_session(self.workspaces, &run_id, active) else {
                    continue;
                };
                let session_idle = matches!(
                    self.workspaces[ti].sessions[si].status,
                    SessionStatus::Idle
                );
                if session_idle {
                    // Combined turn-complete + new-turn-since-baseline check
                    // routes through `Agent::assistant_turn_completed_since`.
                    // Activation baseline (start_count) is still snapshotted
                    // by app.rs at activation time; the trait helper just
                    // wraps the count > baseline && is_idle predicate.
                    let start_count = self.workflow_runs[idx]
                        .active_assistant_start_count()
                        .unwrap_or(0);
                    let current_sid = self.workspaces[ti].sessions[si].transcript_id.clone();
                    let session_type = self.workspaces[ti].sessions[si].session_type.clone();
                    let agent = crate::agent::agent_for(&session_type);
                    let (will_fire, current_count, turn_complete) = match self.workspaces[ti]
                        .worktree_path
                        .as_deref()
                    {
                        Some(wt) => {
                            let ctx = crate::agent::AgentCtx {
                                ts: &self.workspaces[ti].sessions[si],
                                worktree_path: wt,
                            };
                            let count = agent.count_assistant_turns(ctx);
                            let complete = agent.is_idle(ctx);
                            let fire = agent.assistant_turn_completed_since(ctx, start_count);
                            (fire, count, complete)
                        }
                        None => (false, 0, false),
                    };
                    log_tick(
                        &run_id,
                        &format!(
                            "idle check: role={} sid={:?} start={} current={} turn_complete={} will_fire={}",
                            active,
                            current_sid.as_deref().unwrap_or("<none>"),
                            start_count,
                            current_count,
                            turn_complete,
                            will_fire,
                        ),
                    );
                    if will_fire {
                        if let Some(t) = wf.static_transition_on_idle(active) {
                            decisions.push(Decision::ActivateStatic {
                                run_id: run_id.clone(),
                                to: t.to.clone(),
                                from: active.to_string(),
                            });
                        }
                    }
                }
            }
        }

        for d in decisions {
            match d {
                Decision::ActivateStatic { run_id, to, from } => {
                    self.fire_transition(
                        &run_id,
                        &to,
                        TriggerKind::StaticIdle { from_role: from },
                        None,
                        &mut actions,
                    );
                }
                Decision::ActivateDynamic { run_id, to, from, prompt, event_id } => {
                    self.fire_transition(
                        &run_id,
                        &to,
                        TriggerKind::McpTransition { from_role: from, prompt: prompt.clone(), event_id },
                        Some(prompt),
                        &mut actions,
                    );
                }
                Decision::Done { run_id, reason } => {
                    self.finish_run(&run_id, reason, &mut actions);
                }
            }
        }

        actions
    }

    /// Deliver the very first activation prompt to the initial role's
    /// session in a freshly-launched workflow. Called only by the MCP
    /// launch path (`start_workflow_run`); UI launches don't need this
    /// because the user types directly into the session.
    ///
    /// Uses the same `Agent::submit_prompt` queue path as the on_idle
    /// transition — body + Enter separation, quiet-window timing, all
    /// inherited automatically.
    pub fn deliver_initial_workflow_prompt(
        &mut self,
        run_id: &str,
        role_name: &str,
        ws_index: usize,
    ) -> Vec<WorkflowAction> {
        let actions = Vec::new();
        let run_idx = match self.workflow_runs.iter().position(|r| r.run_id == run_id) {
            Some(i) => i,
            None => return actions,
        };
        let wf_name = self.workflow_runs[run_idx].workflow_name.clone();
        let wf = match self.workflows.get(&wf_name).cloned() {
            Some(w) => w,
            None => return actions,
        };
        let role = match wf.roles.get(role_name) {
            Some(r) => r.clone(),
            None => return actions,
        };
        let goal = self.workflow_runs[run_idx].goal.clone();
        let rendered = prepare_initial_prompt(
            role.activation_prompt.as_deref(),
            goal.as_deref(),
            // Lazy renderer — only built if `activation_prompt` is
            // set. Goal-only callers don't pay for the resolver.
            |template| {
                let worktree_ref = self.workspaces[ws_index].worktree_path.as_deref();
                let mut role_engines: BTreeMap<String, Engine> = BTreeMap::new();
                for r in wf.roles.keys() {
                    let engine = match locate_workflow_session(self.workspaces, run_id, r) {
                        Some((wi, sj)) => engine_for_session_type(
                            &self.workspaces[wi].sessions[sj].session_type,
                        ),
                        None => wf.roles[r].engine.clone(),
                    };
                    role_engines.insert(r.clone(), engine);
                }
                let resolver = WorkflowResolver {
                    run: &self.workflow_runs[run_idx],
                    worktree_path: worktree_ref,
                    role_engines,
                };
                workflow::template::render(template, &resolver)
            },
        );
        let rendered = match rendered {
            Some(s) => s,
            None => {
                log_tick(
                    run_id,
                    &format!(
                        "start_workflow: no activation prompt or goal for initial role '{}' — workflow will idle",
                        role_name
                    ),
                );
                return actions;
            }
        };

        // Locate the initial role's session and queue the prompt via
        // the same Agent::submit_prompt path the on_idle gate uses.
        let Some((ti, si)) = locate_workflow_session(self.workspaces, run_id, role_name) else {
            return actions;
        };
        let session_type = self.workspaces[ti].sessions[si].session_type.clone();
        let label = self.workspaces[ti].sessions[si].label.clone();
        log_tick(
            run_id,
            &format!(
                "start_workflow: queued initial prompt for role='{}' session='{}' ({} bytes)",
                role_name,
                label,
                rendered.trim_end().len(),
            ),
        );
        let ws = &mut self.workspaces[ti];
        let wt = ws.worktree_path.clone().unwrap_or_default();
        let ts = &mut ws.sessions[si];
        let ctx = crate::agent::AgentCtxMut {
            ts,
            worktree_path: &wt,
        };
        let agent = crate::agent::agent_for(&session_type);
        if let Err(e) = agent.submit_prompt(ctx, &rendered) {
            log_tick(
                run_id,
                &format!(
                    "start_workflow: submit_prompt error on initial role '{}': {}",
                    role_name, e
                ),
            );
        }
        actions
    }

    /// Keep `role_sessions.current_session_id` aligned with the live
    /// `TerminalSession.transcript_id`. Nothing else.
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
            let mut changed = false;
            for role in role_names {
                let Some((ti, si)) = locate_workflow_session(self.workspaces, &run_id, &role)
                else {
                    continue;
                };

                let live = self.workspaces[ti].sessions[si].transcript_id.clone();
                let binding_sid = self.workflow_runs[idx]
                    .role_sessions
                    .get(&role)
                    .and_then(|b| b.current_session_id.clone());
                if live != binding_sid {
                    if let Some(b) = self.workflow_runs[idx].role_sessions.get_mut(&role) {
                        b.current_session_id = live;
                    }
                    changed = true;
                }
            }
            if changed {
                let _ = workflow::run::save(&self.workflow_runs[idx]);
            }
        }
    }

    /// Execute a role transition: capture outgoing role's last message,
    /// render the target role's prompt, deliver it into the PTY
    /// (queueing `/clear` first if the target role has fresh context).
    fn fire_transition(
        &mut self,
        run_id: &str,
        to_role: &str,
        trigger: TriggerKind,
        supplied_prompt: Option<String>,
        actions: &mut Vec<WorkflowAction>,
    ) {
        let run_idx = match self.workflow_runs.iter().position(|r| r.run_id == run_id) {
            Some(i) => i,
            None => return,
        };
        let wf_name = self.workflow_runs[run_idx].workflow_name.clone();
        let wf = match self.workflows.get(&wf_name).cloned() {
            Some(w) => w,
            None => return,
        };

        // Locate target role's session by workflow tags (source of truth).
        let Some((ti, si)) = locate_workflow_session(self.workspaces, run_id, to_role) else {
            return;
        };

        // Capture outgoing role's last assistant message for history.
        // Engine comes from the bound session's actual `session_type`,
        // not the role's TOML-declared engine — a Claude-declared role
        // can be bound to a Codex session at launch, and parsing its
        // transcript with the wrong parser yields garbage / None.
        let from_role = self.workflow_runs[run_idx].active_role.clone();
        let captured = if let Some(from) = &from_role {
            if let Some((fti, fsi)) = locate_workflow_session(self.workspaces, run_id, from) {
                let from_session = &self.workspaces[fti].sessions[fsi];
                let engine = engine_for_session_type(&from_session.session_type);
                let fsid = from_session.transcript_id.clone();
                let fwt = self.workspaces[fti].worktree_path.clone();
                if let (Some(sid), Some(wt)) = (fsid, fwt) {
                    workflow::transcript::last_message(&engine, &wt, &sid)
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        self.workflow_runs[run_idx].close_active_role(captured);

        // Render prompt for target role. After the first activation of this
        // role in the run, prefer `subsequent_activation_prompt` if set —
        // persistent roles already have the first-activation context in
        // their conversation history and don't need it re-rendered.
        let target_role_spec = match wf.roles.get(to_role).cloned() {
            Some(r) => r,
            None => return,
        };
        let prior_activations = self.workflow_runs[run_idx]
            .history
            .iter()
            .filter(|h| h.role == to_role)
            .count();
        let default_template = if prior_activations > 0 {
            target_role_spec
                .subsequent_activation_prompt
                .clone()
                .or_else(|| target_role_spec.activation_prompt.clone())
        } else {
            target_role_spec.activation_prompt.clone()
        };
        let template_source = supplied_prompt.or(default_template).unwrap_or_default();
        let worktree_ref = self.workspaces[ti].worktree_path.as_deref();
        // Build role → actual-engine map by walking each role's bound session.
        // Falls back to the workflow TOML's declared engine if no session is
        // currently bound for that role (e.g. fresh-context roles before first
        // activation, or pre-launch).
        let mut role_engines: BTreeMap<String, Engine> = BTreeMap::new();
        for role_name in wf.roles.keys() {
            let engine = match locate_workflow_session(self.workspaces, run_id, role_name) {
                Some((wi, sj)) => engine_for_session_type(
                    &self.workspaces[wi].sessions[sj].session_type,
                ),
                None => wf.roles[role_name].engine.clone(),
            };
            role_engines.insert(role_name.clone(), engine);
        }
        let resolver = WorkflowResolver {
            run: &self.workflow_runs[run_idx],
            worktree_path: worktree_ref,
            role_engines,
        };
        let rendered = workflow::template::render(&template_source, &resolver);

        if matches!(target_role_spec.context, workflow::toml_schema::Context::Fresh) {
            self.reset_fresh_session(run_id, to_role, ti, si, actions);
        }

        // Update role_sessions with (possibly new) session_id from the session.
        let current_sid = self.workspaces[ti].sessions[si].transcript_id.clone();
        if let Some(b) = self.workflow_runs[run_idx].role_sessions.get_mut(to_role) {
            b.current_session_id = current_sid;
        }

        // Snapshot the target role's current assistant TURN count at activation.
        // Uses `count_messages` (any assistant JSONL entry counts) so that
        // downstream the idle gate compares turn-to-turn regardless of whether
        // the agent's reply contains text, thinking, or tool_use content.
        let start_count = {
            let current_sid = self.workspaces[ti].sessions[si].transcript_id.clone();
            let session_engine =
                engine_for_session_type(&self.workspaces[ti].sessions[si].session_type);
            match (
                self.workspaces[ti].worktree_path.as_deref(),
                current_sid.as_deref(),
            ) {
                (Some(wt), Some(sid)) => workflow::transcript::count_messages(
                    &session_engine,
                    wt,
                    sid,
                    workflow::transcript::MessageKind::Assistant,
                ),
                _ => 0,
            }
        };

        self.workflow_runs[run_idx].activate_role(to_role.to_string(), trigger, start_count);
        let _ = workflow::run::save(&self.workflow_runs[run_idx]);
        let from_label = from_role.as_deref().unwrap_or("?");
        actions.push(WorkflowAction::SetStatusMsg(format!(
            "Workflow: {} → {}",
            from_label, to_role
        )));

        // Deliver prompt. Trim trailing whitespace first so our explicit "\r"
        // submit lands on non-newline text — otherwise a trailing "\n" in the
        // TOML multiline string gets typed into the input box and the "\r"
        // then only adds another newline instead of submitting. Longer delay
        // for fresh-context roles because they just received a `/clear` and
        // need a beat to reset internal state.
        if !rendered.trim().is_empty() {
            // Route through Agent::submit_prompt — same PendingWrite shape
            // as before, but engine-specific knobs (Codex's longer settle
            // delay, kitty Enter encoding, etc.) live in the Agent impl
            // rather than scattered through this function.
            let session_type = self.workspaces[ti].sessions[si].session_type.clone();
            let label = self.workspaces[ti].sessions[si].label.clone();
            let body_len = rendered.trim_end().len();
            log_tick(
                run_id,
                &format!(
                    "fire_transition: activated '{}' queued prompt ({} bytes, fires on quiet PTY) on session '{}'",
                    to_role, body_len, label,
                ),
            );
            let ws = &mut self.workspaces[ti];
            let wt = ws.worktree_path.clone().unwrap_or_default();
            let ts = &mut ws.sessions[si];
            let ctx = crate::agent::AgentCtxMut {
                ts,
                worktree_path: &wt,
            };
            let agent = crate::agent::agent_for(&session_type);
            if let Err(e) = agent.submit_prompt(ctx, &rendered) {
                log_tick(
                    run_id,
                    &format!(
                        "fire_transition: submit_prompt error on '{}': {}",
                        label, e
                    ),
                );
            }
        } else {
            log_tick(
                run_id,
                &format!(
                    "fire_transition: activated '{}' but rendered prompt was EMPTY — nothing to deliver",
                    to_role
                ),
            );
        }
        actions.push(WorkflowAction::SaveSessionManifest);
    }

    /// Queue `/clear` to reset a fresh-context role's agent. Delivery is
    /// gated on PTY quiet (see `PendingWrite`) so we don't try to type the
    /// command while the agent is still painting its startup UI — that's
    /// when `\r` gets buffered into the input box instead of interpreted
    /// as submit.
    ///
    /// Also invalidates the session's bound sid and role baseline because
    /// claude rotates its transcript file on `/clear`; the new file's sid
    /// is picked up later by the history.jsonl correlator.
    fn reset_fresh_session(
        &mut self,
        run_id: &str,
        role: &str,
        ti: usize,
        si: usize,
        actions: &mut Vec<WorkflowAction>,
    ) -> bool {
        let wt = self.workspaces[ti]
            .worktree_path
            .as_deref()
            .map(|p| p.to_path_buf());
        let label = self.workspaces[ti].sessions[si].label.clone();
        let ts = &mut self.workspaces[ti].sessions[si];
        if ts.session.exited {
            log_tick(
                run_id,
                &format!("reset_fresh: session '{}' already exited", label),
            );
            return false;
        }
        // Queue /clear to fire when the PTY first goes quiet. Floor of 1s so
        // we don't fire during the PTY startup noise. Hard deadline 120s in
        // case the agent never goes quiet.
        ts.pending_clear = Some(PendingWrite::wait_for_quiet(
            "/clear".to_string(),
            true,
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(120),
        ));
        ts.status = SessionStatus::Idle;
        // Refresh the pending-jsonl baseline to the current files so the new
        // file created by /clear shows up as new, and clear session_id so the
        // detection poll rebinds to it. Without this the detector treats the
        // pre-/clear file as still bound.
        ts.pending_jsonl_files = match (ts.session_type.as_str(), wt.as_deref()) {
            ("claude", Some(wt)) => Some(App::list_jsonl_files(wt)),
            ("codex", Some(wt)) => Some(App::list_codex_sessions(wt)),
            _ => None,
        };
        // Rebind to None — bumps generation so any in-flight transcript
        // reads see the rebind and reset their cursor offsets to the new
        // file once the detector picks it up.
        ts.rebind_transcript(None);
        ts.pending_prompt = None;
        // Old file's turn counts no longer apply to the new file — reset the
        // role's message baseline so templates slice from 0 post-/clear.
        // Key off the role name (stable, workflow-managed), NOT the session
        // label — labels are user-editable in the per-session settings, and
        // a renamed fresh role would otherwise leave a stale baseline keyed
        // on the prior label and the role-keyed binding never updates.
        if let Some(run) = self.workflow_runs.iter_mut().find(|r| r.run_id == run_id) {
            run.role_baselines
                .insert(role.to_string(), MessageBaseline::default());
            if let Some(b) = run.role_sessions.get_mut(role) {
                b.current_session_id = None;
            }
        }
        actions.push(WorkflowAction::SaveSessionManifest);
        log_tick(
            run_id,
            &format!(
                "reset_fresh: queued /clear for '{}' (fires on first quiet PTY)",
                label
            ),
        );
        true
    }

    /// Mark a workflow run as done, persist the change, and surface a
    /// status-bar note. Distinct from `App::stop_workflow_run` (the
    /// user-driven stop) — this fires when an MCP `workflow_done`
    /// event is processed.
    fn finish_run(&mut self, run_id: &str, reason: String, actions: &mut Vec<WorkflowAction>) {
        if let Some(run) = self.workflow_runs.iter_mut().find(|r| r.run_id == run_id) {
            run.mark_done(reason.clone());
            let _ = workflow::run::save(run);
        }
        actions.push(WorkflowAction::SetStatusMsg(format!(
            "Workflow done: {}",
            reason
        )));
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
        }
    }

    fn make_run(run_id: &str, wf_name: &str, initial_role: &str) -> WorkflowRun {
        let mut role_sessions = BTreeMap::new();
        role_sessions.insert(
            "worker".to_string(),
            RoleBinding {
                session_label: "worker".to_string(),
                current_session_id: None,
            },
        );
        role_sessions.insert(
            "reviewer".to_string(),
            RoleBinding {
                session_label: "reviewer".to_string(),
                current_session_id: None,
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

    /// `finish_run` flips status to Done, clears the active role,
    /// persists state.json, and emits a status-bar action.
    #[test]
    fn finish_run_marks_done_and_persists() {
        with_temp_home(|| {
            let run_id = "wf_test_finish";
            let run = make_run(run_id, "feedback", "worker");
            // Pre-condition: active.
            assert_eq!(run.active_role.as_deref(), Some("worker"));
            assert_eq!(run.status, RunStatus::Running);
            let mut runs = vec![run];
            let mut workspaces: Vec<Workspace> = Vec::new();
            let workflows: HashMap<String, Workflow> = HashMap::new();

            let dummy = dummy_cap_state();
            let mut ctx = WorkflowControllerCtx {
                workflow_runs: &mut runs,
                workspaces: &mut workspaces,
                workflows: &workflows,
                last_term_size: (80, 24),
                config: &dummy.config,
                cap_status: &dummy.cap_status,
                kill_tx: &dummy.kill_tx,
            };
            let mut actions = Vec::new();
            ctx.finish_run(run_id, "completed by test".into(), &mut actions);

            assert_eq!(runs[0].status, RunStatus::Done);
            assert!(runs[0].active_role.is_none());
            assert_eq!(runs[0].done_reason.as_deref(), Some("completed by test"));
            assert!(matches!(actions[0], WorkflowAction::SetStatusMsg(_)));

            // state.json must exist on disk after finish_run runs.
            let state_path = workflow::run::run_dir(run_id).join("state.json");
            assert!(
                state_path.exists(),
                "state.json should have been persisted at {:?}",
                state_path
            );
        });
    }

    /// `reset_fresh_session` queues `/clear`, rebinds the transcript to
    /// `None` (bumping `generation`), drops the role's message baseline
    /// from `role_baselines`, and emits a `SaveSessionManifest` action.
    /// The session is also given a renamed `label` so we pin that the
    /// reset still keys off the role name (stable) and not the label
    /// (user-editable) — otherwise a renamed fresh role would keep its
    /// stale baseline forever.
    #[test]
    fn reset_fresh_rebinds_session() {
        with_temp_home(|| {
            let run_id = "wf_test_fresh";
            let mut run = make_run(run_id, "feedback", "worker");
            // Pre-seed a baseline + bound sid for the reviewer to prove they
            // get reset.
            run.role_baselines.insert(
                "reviewer".to_string(),
                MessageBaseline {
                    user_count: 7,
                    assistant_count: 3,
                },
            );
            if let Some(b) = run.role_sessions.get_mut("reviewer") {
                b.current_session_id = Some("old-sid".into());
            }
            let mut runs = vec![run];

            let worker = stub_session("worker", "claude", run_id, "worker", Some("worker-sid"));
            // Renamed label: simulates the user editing the sidebar
            // label in session settings. The role tag is what matters.
            let mut reviewer = stub_session(
                "renamed-by-user",
                "claude",
                run_id,
                "reviewer",
                Some("reviewer-sid-old"),
            );
            // Pre-bump generation to 1 so we can show the rebind bumps it again.
            reviewer.generation = 1;
            let workspace = workspace_with(
                vec![worker, reviewer],
                Some(PathBuf::from("/tmp/cm-test-fresh")),
            );
            let mut workspaces = vec![workspace];

            let workflows: HashMap<String, Workflow> = HashMap::new();
            let dummy = dummy_cap_state();
            let mut ctx = WorkflowControllerCtx {
                workflow_runs: &mut runs,
                workspaces: &mut workspaces,
                workflows: &workflows,
                last_term_size: (80, 24),
                config: &dummy.config,
                cap_status: &dummy.cap_status,
                kill_tx: &dummy.kill_tx,
            };
            let mut actions = Vec::new();
            let did_reset = ctx.reset_fresh_session(run_id, "reviewer", 0, 1, &mut actions);
            assert!(did_reset);

            let reviewer = &workspaces[0].sessions[1];
            assert!(reviewer.pending_clear.is_some(), "/clear should be queued");
            assert!(
                reviewer.transcript_id.is_none(),
                "transcript should rebind to None"
            );
            assert_eq!(reviewer.generation, 2, "generation must bump on rebind");
            assert!(matches!(actions[0], WorkflowAction::SaveSessionManifest));

            let baseline = runs[0]
                .role_baselines
                .get("reviewer")
                .expect("baseline reset under role key");
            assert_eq!(baseline.user_count, 0);
            assert_eq!(baseline.assistant_count, 0);
            // The renamed label must NOT have a baseline written under it
            // — that's the bug this test pins against.
            assert!(
                runs[0].role_baselines.get("renamed-by-user").is_none(),
                "baseline must not be keyed by mutable session label"
            );
            let binding = runs[0].role_sessions.get("reviewer").unwrap();
            assert!(
                binding.current_session_id.is_none(),
                "binding sid must clear on /clear"
            );
        });
    }

    /// Static-transition smoke test: an active run whose worker is
    /// `Idle` but bound to no transcript leaves the gate shut — the
    /// idle predicate returns false because no real assistant turn has
    /// landed. Pins that the decision loop traverses run state without
    /// firing premature transitions, the original failure mode for
    /// every prior version of this code.
    #[test]
    fn tick_with_idle_role_and_no_transcript_does_not_fire() {
        with_temp_home(|| {
            let run_id = "wf_test_tick";
            let run = make_run(run_id, "feedback", "worker");
            let mut runs = vec![run];

            let worker = stub_session("worker", "claude", run_id, "worker", None);
            let workspace = workspace_with(vec![worker], None);
            let mut workspaces = vec![workspace];

            let mut roles = BTreeMap::new();
            roles.insert(
                "worker".to_string(),
                role_with(Engine::ClaudeCode, Context::Persistent),
            );
            roles.insert(
                "reviewer".to_string(),
                role_with(Engine::ClaudeCode, Context::Fresh),
            );
            let transitions = vec![Transition {
                from: "worker".to_string(),
                on: TriggerOn::Idle,
                to: "reviewer".to_string(),
            }];
            let workflow_def = make_workflow(
                "feedback",
                roles,
                vec!["worker".to_string(), "reviewer".to_string()],
                transitions,
            );
            let mut workflows = HashMap::new();
            workflows.insert("feedback".to_string(), workflow_def);

            let dummy = dummy_cap_state();
            let mut ctx = WorkflowControllerCtx {
                workflow_runs: &mut runs,
                workspaces: &mut workspaces,
                workflows: &workflows,
                last_term_size: (80, 24),
                config: &dummy.config,
                cap_status: &dummy.cap_status,
                kill_tx: &dummy.kill_tx,
            };
            let actions = ctx.tick();

            // No worktree path → idle gate's `assistant_turn_completed_since`
            // returns false → no static transition fires → no actions.
            assert!(actions.is_empty(), "tick should be a no-op: {:?}", actions);
            assert_eq!(runs[0].active_role.as_deref(), Some("worker"));
            assert_eq!(runs[0].history.len(), 1);
        });
    }

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

    /// (a) `prepare_initial_prompt` falls back to `goal` when the role
    /// has no `activation_prompt`. End-to-end test: call
    /// `launch_workflow`, then `deliver_initial_workflow_prompt`, and
    /// pin that the worker's `pending_prompt.text` is the goal verbatim
    /// (the bypass-rendering branch).
    #[test]
    fn launch_then_deliver_uses_goal_when_activation_prompt_empty() {
        with_temp_home(|| {
            let mut runs: Vec<WorkflowRun> = Vec::new();
            let mut workspaces = launch_test_workspace(None);
            let mut workflows = HashMap::new();
            workflows.insert("feedback".to_string(), launch_test_workflow());

            let goal = "build feature X — verbatim {{ not_a_template }}";

            {
                let dummy = dummy_cap_state();
                let mut ctx = WorkflowControllerCtx {
                    workflow_runs: &mut runs,
                    workspaces: &mut workspaces,
                    workflows: &workflows,
                    last_term_size: (80, 24),
                    config: &dummy.config,
                    cap_status: &dummy.cap_status,
                    kill_tx: &dummy.kill_tx,
                };
                let _ = ctx.launch_workflow(
                    0,
                    "feedback",
                    launch_test_slots(),
                    Some(goal.to_string()),
                );
            }
            assert_eq!(runs.len(), 1, "launch should push exactly one run");
            let run_id = runs[0].run_id.clone();

            // Now deliver the initial prompt — same code path the MCP
            // launch (`start_workflow_run`) uses.
            {
                let dummy = dummy_cap_state();
                let mut ctx = WorkflowControllerCtx {
                    workflow_runs: &mut runs,
                    workspaces: &mut workspaces,
                    workflows: &workflows,
                    last_term_size: (80, 24),
                    config: &dummy.config,
                    cap_status: &dummy.cap_status,
                    kill_tx: &dummy.kill_tx,
                };
                let _ = ctx.deliver_initial_workflow_prompt(&run_id, "worker", 0);
            }

            let worker = &workspaces[0].sessions[0];
            let pp = worker
                .pending_prompt
                .as_ref()
                .expect("worker should have a queued prompt after deliver");
            // The goal text bypasses the template renderer — `{{` is
            // preserved verbatim. submit=true so a trailing Enter
            // lands.
            assert_eq!(pp.text, goal);
            assert!(pp.submit, "initial prompt should be auto-submitted");
        });
    }

    /// (b) Every role in the workflow is bound to a participant
    /// session: launch tags each Existing-slot session with the run id
    /// + role name, and `WorkflowRun.role_sessions` ends up with one
    /// `RoleBinding` per role pointing at the right session label /
    /// engine-derived from the bound session's `session_type`.
    #[test]
    fn launch_binds_every_role_to_a_participant_session() {
        with_temp_home(|| {
            let mut runs: Vec<WorkflowRun> = Vec::new();
            let mut workspaces = launch_test_workspace(None);
            let mut workflows = HashMap::new();
            workflows.insert("feedback".to_string(), launch_test_workflow());

            let actions = {
                let dummy = dummy_cap_state();
                let mut ctx = WorkflowControllerCtx {
                    workflow_runs: &mut runs,
                    workspaces: &mut workspaces,
                    workflows: &workflows,
                    last_term_size: (80, 24),
                    config: &dummy.config,
                    cap_status: &dummy.cap_status,
                    kill_tx: &dummy.kill_tx,
                };
                ctx.launch_workflow(0, "feedback", launch_test_slots(), None)
            };

            assert_eq!(runs.len(), 1, "exactly one run pushed");
            let run = &runs[0];
            let run_id = run.run_id.as_str();
            assert_eq!(run.workflow_name, "feedback");
            assert_eq!(run.active_role.as_deref(), Some("worker"));
            // Bindings present for every declared role.
            let worker_binding = run.role_sessions.get("worker").expect("worker bound");
            let reviewer_binding = run
                .role_sessions
                .get("reviewer")
                .expect("reviewer bound");
            assert_eq!(worker_binding.session_label, "worker");
            assert_eq!(reviewer_binding.session_label, "reviewer");

            // Each underlying TerminalSession got its workflow tags set.
            let worker = &workspaces[0].sessions[0];
            let reviewer = &workspaces[0].sessions[1];
            assert_eq!(worker.workflow_run_id.as_deref(), Some(run_id));
            assert_eq!(worker.workflow_role.as_deref(), Some("worker"));
            assert_eq!(reviewer.workflow_run_id.as_deref(), Some(run_id));
            assert_eq!(reviewer.workflow_role.as_deref(), Some("reviewer"));
            // session_type → engine derivation matches both stubs
            // (which use "claude").
            assert_eq!(worker.session_type, "claude");
            assert_eq!(reviewer.session_type, "claude");

            // Action sequence ends with a SaveSessionManifest + a
            // "Launched ..." status message. The dispatcher in App
            // applies these in order.
            assert!(
                actions
                    .iter()
                    .any(|a| matches!(a, WorkflowAction::SaveSessionManifest)),
                "launch must emit SaveSessionManifest: {:?}",
                actions
            );
            let last_status = actions.iter().rev().find_map(|a| match a {
                WorkflowAction::SetStatusMsg(s) => Some(s.as_str()),
                _ => None,
            });
            assert_eq!(
                last_status,
                Some("Launched feedback (2 roles, initial: worker)")
            );
        });
    }

    #[test]
    fn launch_existing_codex_rebinds_stale_sid_to_newest_rollout() {
        with_temp_home(|| {
            let worktree = PathBuf::from("/tmp/cm-codex-launch-rebind");
            write_codex_meta("old-sid", &worktree);
            std::thread::sleep(std::time::Duration::from_millis(20));
            write_codex_meta("new-sid", &worktree);

            let mut worker = bare_session("worker", "codex", Some("task-1"));
            worker.transcript_id = Some("old-sid".into());
            worker.generation = 3;
            let reviewer = bare_session("reviewer", "claude", None);
            let mut workspaces = vec![workspace_with(vec![worker, reviewer], Some(worktree))];

            let mut runs: Vec<WorkflowRun> = Vec::new();
            let mut workflows = HashMap::new();
            // Worker role must declare Codex engine to match the
            // codex worker session bound below — launch validation
            // rejects engine mismatches on Existing slots.
            let mut codex_worker = role_with(Engine::Codex, Context::Persistent);
            codex_worker.needs_mcp = false;
            let mut reviewer_role = role_with(Engine::ClaudeCode, Context::Persistent);
            reviewer_role.needs_mcp = false;
            let mut roles = BTreeMap::new();
            roles.insert("worker".to_string(), codex_worker);
            roles.insert("reviewer".to_string(), reviewer_role);
            workflows.insert(
                "feedback".to_string(),
                make_workflow(
                    "feedback",
                    roles,
                    vec!["worker".to_string(), "reviewer".to_string()],
                    vec![Transition {
                        from: "worker".to_string(),
                        on: TriggerOn::Idle,
                        to: "reviewer".to_string(),
                    }],
                ),
            );

            {
                let dummy = dummy_cap_state();
                let mut ctx = WorkflowControllerCtx {
                    workflow_runs: &mut runs,
                    workspaces: &mut workspaces,
                    workflows: &workflows,
                    last_term_size: (80, 24),
                    config: &dummy.config,
                    cap_status: &dummy.cap_status,
                    kill_tx: &dummy.kill_tx,
                };
                let _ = ctx.launch_workflow(0, "feedback", launch_test_slots(), None);
            }

            let worker = &workspaces[0].sessions[0];
            assert_eq!(worker.transcript_id.as_deref(), Some("new-sid"));
            assert_eq!(worker.generation, 4, "stale sid rebind must bump generation");
            let run = &runs[0];
            assert_eq!(
                run.role_sessions["worker"].current_session_id.as_deref(),
                Some("new-sid")
            );
            assert_eq!(run.history[0].session_id.as_deref(), Some("new-sid"));
        });
    }

    /// (c) Every spawned/bound participant session ends up with a
    /// matching `task_id`. For Existing slots, the launch leaves the
    /// pre-existing `task_id` intact; the `inherit_task_id` it picks
    /// up from the first Existing slot is what fresh-spawned New
    /// slots would inherit (the New-slot path itself isn't exercised
    /// here because it would shell out to `claude`). Together with
    /// `MCP-path post-launch stamping in `start_workflow_run`, this
    /// is the descendant-auth contract: every workflow participant
    /// is reachable via the launching task's id.
    #[test]
    fn launch_preserves_task_id_on_every_participant_session() {
        with_temp_home(|| {
            let mut runs: Vec<WorkflowRun> = Vec::new();
            // Pre-seed both slot sessions with the same task id so the
            // launch ends up with EVERY participant tagged. (Mimics the
            // common A-f flow where the user launches from a session
            // already attached to a task; subsequent slots are picked
            // from that same task's sessions.)
            let worker = bare_session("worker", "claude", Some("task-X"));
            let reviewer = bare_session("reviewer", "claude", Some("task-X"));
            let mut workspaces = vec![workspace_with(vec![worker, reviewer], None)];
            let mut workflows = HashMap::new();
            workflows.insert("feedback".to_string(), launch_test_workflow());

            let _ = {
                let dummy = dummy_cap_state();
                let mut ctx = WorkflowControllerCtx {
                    workflow_runs: &mut runs,
                    workspaces: &mut workspaces,
                    workflows: &workflows,
                    last_term_size: (80, 24),
                    config: &dummy.config,
                    cap_status: &dummy.cap_status,
                    kill_tx: &dummy.kill_tx,
                };
                ctx.launch_workflow(0, "feedback", launch_test_slots(), None)
            };

            // Every slot session got tagged into the new run AND
            // retained its task_id. start_workflow_run's post-launch
            // stamp pass relies on this — it walks every session whose
            // workflow_run_id matches the new run and overwrites
            // task_id, which only works if launch left them tagged
            // here.
            assert_eq!(runs.len(), 1);
            let run_id = runs[0].run_id.as_str();
            for ts in &workspaces[0].sessions {
                assert_eq!(
                    ts.workflow_run_id.as_deref(),
                    Some(run_id),
                    "session '{}' missing run tag",
                    ts.label
                );
                assert_eq!(
                    ts.task_id.as_deref(),
                    Some("task-X"),
                    "session '{}' lost its task id",
                    ts.label
                );
            }
        });
    }
}
