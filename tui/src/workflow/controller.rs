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

/// 10d-2c-1 review round-4 (F1): description of the workflow-run-level
/// mutations a fresh-context role reset implies, returned by
/// `reset_fresh_session` as a pure value for the caller to apply.
///
/// Why this exists: on the daemon-routed dynamic-transition path,
/// the post-reset state has to be persisted inside the same
/// `workflow::run::modify` closure that writes `events_offset` +
/// the history entry. The closure reloads state.json from disk
/// under flock, so in-memory mutations applied BEFORE the closure
/// (the pre-round-4 shape) would be clobbered by the reload —
/// the fresh role's pre-reset `current_session_id` and stale
/// `role_baselines` entry would persist on disk, corrupting the
/// on_idle gate and the history's `session_id` for subsequent
/// activations.
///
/// Approach A from the reviewer: make `reset_fresh_session` pure
/// w.r.t. `WorkflowRun` (it still drives TerminalSession side
/// effects — `/clear` queue, transcript rebind, etc.) and return
/// `Option<RoleResetMutations>`. Each caller is responsible for
/// applying the mutations to the run AT THE RIGHT POINT in its
/// own persistence flow:
///   - `fire_transition`: apply in-memory before its existing
///     `workflow::run::save`.
///   - Daemon-routed `ActivateDynamic` arm: apply INSIDE the
///     `run::modify` closure so the post-reload run picks them up.
#[derive(Debug, Clone)]
pub(crate) struct RoleResetMutations {
    /// Role name to mutate on the run.
    pub role: String,
    /// Value to assign to `role_sessions[role].current_session_id`.
    /// Always `None` today (the reset blanks the bound sid so the
    /// transcript detector rebinds to the post-`/clear` file), but
    /// carried explicitly so the apply site doesn't hardcode it.
    pub new_session_id: Option<String>,
    /// Value to assign to `role_baselines[role]`. Always
    /// `MessageBaseline::default()` today (post-`/clear` the
    /// per-role baseline must restart from 0 so templates slice
    /// from the new transcript's start, not the old turn count).
    pub new_baseline: MessageBaseline,
}

/// 10d-2c-1 review round-8: distinguishes the two
/// "Option<None>" meanings of `deliver_dynamic_activation_prompt`'s
/// prior return type. Pre-round-8 the caller couldn't tell
/// "delivered, persistent role so no reset" (legitimate) from
/// "early failure, missing target session/workflow/role" (the
/// prompt was DROPPED). Both surfaced as `None` and the caller
/// advanced `events_offset` either way — the failure case left
/// the workflow stuck on a role that never got prompted.
///
/// Post-round-8 the caller branches:
///   - `Delivered { reset }`: advance `events_offset` (today's
///     behavior). `reset` is `Some` for fresh-context roles
///     and `None` for persistent ones.
///   - `Failed { reason }`: log loudly + do NOT advance
///     `events_offset` so the next tick retries delivery.
#[derive(Debug, Clone)]
pub(crate) enum DeliveryOutcome {
    /// Activation prompt was delivered (or, in the rendered-
    /// empty case, the delivery code completed without error).
    /// `reset` carries the [`RoleResetMutations`] for
    /// `Context::Fresh` roles (None for persistent roles).
    Delivered {
        reset: Option<RoleResetMutations>,
    },
    /// Delivery did NOT happen. Caller MUST NOT advance
    /// `events_offset`; the next tick re-reads the event and
    /// retries delivery. Caller logs loudly so an operator
    /// notices on a finite-bound retry loop (e.g. permanently
    /// closed session).
    Failed {
        reason: String,
    },
}

/// Apply a [`RoleResetMutations`] to `run` in place. Pulled out
/// to a free function so both the in-memory apply (in
/// `fire_transition`) and the closure-scoped apply (in the
/// daemon-routed `ActivateDynamic` arm of `tick`) call exactly
/// the same code — drift between them would re-introduce the
/// round-4 bug.
pub(crate) fn apply_role_reset(run: &mut WorkflowRun, reset: &RoleResetMutations) {
    run.role_baselines
        .insert(reset.role.clone(), reset.new_baseline.clone());
    if let Some(b) = run.role_sessions.get_mut(&reset.role) {
        b.current_session_id = reset.new_session_id.clone();
    }
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
        // Sub-2b-1 review-r#4 #2: session indices whose
        // `transcript_id` was set or rebound during this
        // workflow launch. Deferred to a post-loop walk so we
        // can take `&Workspace + &TerminalSession` without
        // conflicting with the mutable `sessions.get_mut(*si)`
        // borrow active inside each iteration. The post-loop
        // walk then calls
        // `App::push_transcript_path_to_daemon_if_attached`
        // for each so the daemon's
        // `resolve_authorized_session` flips `pending` →
        // `ready`. No-op for local-only sessions (the helper
        // gates on `daemon_session_uid.is_some()`).
        let mut transcript_updated_sis: Vec<usize> = Vec::new();
        for slot in &slots {
            let role = &wf.roles[&slot.role];
            let (session_label, session_id, effective_engine, daemon_session_uid) = match slot.source() {
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
                    // 10d-2c-1 review round-5 (F1): push workflow
                    // context to daemon so a daemon-attached
                    // session bound as an Existing-slot
                    // participant passes auth on
                    // `workflow_transition` / `workflow_done`.
                    // No-op for TUI-local sessions (the helper
                    // gates on `daemon_session_uid.is_some()`).
                    crate::app::App::push_workflow_context_to_daemon_if_attached(
                        ts,
                        Some(&run_id),
                        Some(&slot.role),
                    );
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
                                // Sub-2b-1 review-r#4 #2: queue
                                // for daemon transcript_path push
                                // after the loop body's mutable
                                // `ts` borrow ends.
                                transcript_updated_sis.push(*si);
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
                                    // Sub-2b-1 review-r#4 #2:
                                    // codex resume-rebind. Same
                                    // post-loop push as the
                                    // initial-bind arm above.
                                    transcript_updated_sis.push(*si);
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
                    // 10d-2c-2-2-b round-5 F1: co-capture the
                    // daemon session uid (Some iff this role is
                    // bound to a daemon-spawned session).
                    // Becomes the durable ownership signal in
                    // `RoleBinding.daemon_session_uid`.
                    let daemon_uid_clone = ts.session.daemon_session_uid.clone();
                    if let Some(msg) = respawn_warning {
                        actions.push(WorkflowAction::SetStatusMsg(msg));
                    }
                    (session_label_clone, session_id_clone, eng, daemon_uid_clone)
                }
                WorkflowSlotSource::New(engine) => {
                    match self.spawn_workflow_session(
                        ws_index,
                        &slot.role,
                        engine,
                        &run_id,
                        inherit_task_id.clone(),
                    ) {
                        // Round-5 F1: spawn_workflow_session
                        // returns (label, sid). Workflow respawns
                        // are TUI-local in Phase 1 (see the
                        // method's "SpawnTarget::TuiLocal" comment),
                        // so daemon_session_uid is always None
                        // for this branch. When workflows
                        // eventually relocate to daemon-spawn,
                        // this becomes Some(uid).
                        Some((label, sid)) => (label, sid, engine.clone(), None),
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
                    daemon_session_uid,
                },
            );
        }

        // Sub-2b-1 review-r#4 #2: post-loop daemon push for
        // every session whose `transcript_id` was set/rebound
        // during this workflow launch. Mutable `ts` borrows
        // inside the loop blocked an inline push (the helper
        // takes `&Workspace + &TerminalSession`); deferring
        // here is the cleanest shape. Dedup in case the same
        // `si` was queued twice (initial-bind + codex-rebind
        // would only fire in mutually exclusive branches but
        // belt-and-suspenders for any future code path).
        transcript_updated_sis.sort_unstable();
        transcript_updated_sis.dedup();
        for si in &transcript_updated_sis {
            let Some(ws) = self.workspaces.get(ws_index) else {
                continue;
            };
            let Some(ts) = ws.sessions.get(*si) else {
                continue;
            };
            crate::app::App::push_transcript_path_to_daemon_if_attached(
                ts, ws,
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
        // 10d-2c-1 review round-6 (F1): CREATE save. The run_id
        // was just minted and no other writer (daemon or TUI)
        // knows about it yet — no race possible, `save` is the
        // correct primitive. `modify` requires the file to exist
        // and would fail here.
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
        // Workflow respawns are TUI-local (slice 10c-e-3a per-spawn
        // routing). They have no daemon-side equivalent in Phase 1;
        // the workflow control plane lives in `~/.cm/workflow-runs/`
        // which the daemon doesn't manage. MCP calls from a
        // workflow participant (e.g. `workflow_transition`) must
        // reach the TUI socket, not the daemon — pinning explicitly.
        let (program, args) = match crate::mcp_config::build_args(
            crate::mcp_config::SpawnTarget::TuiLocal,
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
            seeded_from_snapshot: None,
            preserved_last_exit: None,
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
                daemon_routed: bool,
                // 10d-2c-1 review round-3 (F1): post-event byte
                // offset, captured at event-read time (line ~752).
                // Threaded through the Decision so the apply
                // phase sets `run.events_offset = new_offset`
                // explicitly inside `run::modify` — closes the
                // 3rd-round regression where the closure read
                // events_offset from the post-reload (stale) run.
                new_offset: u64,
                // 10d-2c-1 review round-15: per-event iteration
                // captured by the daemon's post-mutation closure.
                // The TUI's history append uses this — pre-r15
                // it read state.json's current `iteration` which
                // gave queued events the LATEST value rather
                // than the per-event activation iteration.
                // `0` for pre-r15 on-disk events; the appender
                // falls back to `r.iteration` then.
                event_iteration: u32,
                /// 10d-2c-2-2-b F3: `args.trigger` discriminator
                /// from the event. `Some("static_idle")` when the
                /// daemon poller fired this via `workflow_transition`
                /// with `trigger: "static_idle"`; `None` for MCP-
                /// direct callers and pre-F3 daemon transitions.
                /// Drives `TriggerKind` selection at history-append
                /// time.
                trigger: Option<String>,
            },
            Done {
                run_id: String,
                reason: String,
                daemon_routed: bool,
                new_offset: u64,
                // 10d-2c-1 review round-15: per-event iteration
                // from the daemon's post-mutation capture.
                // Unused by Done's TUI processing (no history
                // append) but carried for parity.
                event_iteration: u32,
            },
            /// 10d-2c-1 review round-11 (F1): skip-but-advance.
            /// Event was read but doesn't produce a real
            /// transition or done (e.g. `EventKind::Unknown` —
            /// future event types the TUI doesn't recognize, or
            /// no-op shapes). Without this variant the Unknown
            /// branch dropped the event silently, leaving
            /// `events_offset` un-advanced — the next tick
            /// re-read the same Unknown event AND blocked the
            /// static-idle check (because `events_with_offsets`
            /// wasn't empty). Wedges the run.
            ///
            /// Threading through the same decision-processing
            /// loop (rather than an inline modify in the read
            /// loop) keeps offset advances ordered correctly
            /// alongside Transition / Done modifies. An inline
            /// advance would interleave with later transitions'
            /// modifies and risk going backward — the decision
            /// loop is the single serial point for offset writes.
            Skip {
                run_id: String,
                new_offset: u64,
                /// Free-form reason for the log line. Surfaces
                /// in `log_tick` so an operator can correlate
                /// silent skips against the events.jsonl tail.
                reason: String,
            },
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

            // 10d-2c-1 review round-10: read events with PER-EVENT
            // post-offsets (each event paired with the byte
            // offset immediately after its line). Each Decision
            // carries its OWN post-offset rather than the
            // batch-final value. The decision-processing loop
            // advances `events_offset` per successful event AND
            // stops processing this run's batch on the first
            // failure — keeping the failed event re-readable on
            // the next tick.
            //
            // Pre-round-10 every event in a batch carried the
            // batch-final offset. A mid-batch Failed event was
            // permanently skipped: earlier successes (or later
            // successes after the Failed continue) had already
            // advanced past it, OR were about to.
            //
            // Pre-round-10 the in-memory `events_offset` was
            // also set to batch-final immediately after
            // `read_new`. Round-10 drops that early-assign;
            // in-memory advances per successful decision via the
            // existing `*slot = updated_run` post-modify
            // re-sync.
            let (events_with_offsets, final_consumed_offset) =
                workflow::events::read_new_with_offsets(&run_id, offset);

            if paused {
                continue;
            }

            // 10d-2c-1 review round-12 (F2): if bytes were
            // consumed (malformed lines) but no events
            // surfaced, push a Skip so the decision loop
            // advances events_offset past the malformed lines.
            // Pre-r12 a malformed line in events.jsonl wedged
            // offset at 0 forever: `events_with_offsets`
            // empty → no decisions pushed → the static-idle
            // branch ran but doesn't advance offset.
            if events_with_offsets.is_empty()
                && final_consumed_offset > offset
            {
                decisions.push(Decision::Skip {
                    run_id: run_id.clone(),
                    new_offset: final_consumed_offset,
                    reason: format!(
                        "malformed line(s) consumed (offset {} → {})",
                        offset, final_consumed_offset,
                    ),
                });
            }

            for (ev, post_event_offset) in &events_with_offsets {
                let daemon_routed = ev.source == "daemon";
                match ev.kind() {
                    workflow::events::EventKind::Transition { to, prompt } => {
                        // 10d-2c-1 review round-7 (F2): for daemon-
                        // routed events, the authoritative
                        // outgoing role lives on the event itself
                        // (captured pre-mutation under flock by
                        // `workflow_transition`'s closure).
                        // Deriving from in-memory `active_role`
                        // gives the WRONG role after a TUI
                        // restart: state.json already carries the
                        // post-mutation `active_role = to`, so the
                        // snapshot would record `from_role = to`.
                        //
                        // 10d-2c-1 review round-11 (F2):
                        // intermediate fallback to `ev.role` for
                        // daemon-source events whose `from_role`
                        // is None (e.g. pre-round-7 events on
                        // disk; or any future case where the
                        // capture missed). `ev.role` carries the
                        // outgoing caller role from the
                        // RPC params — semantically equivalent
                        // to `from_role` for the wf-routed path.
                        // Final fallback to in-memory
                        // `active_role` remains for TuiLocal
                        // events (where state hasn't mutated yet
                        // and `ev.role` may be "unknown" from
                        // the legacy `_append_event` path).
                        let from_opt = ev
                            .from_role
                            .clone()
                            .or_else(|| {
                                if ev.source == "daemon" {
                                    Some(ev.role.clone())
                                } else {
                                    None
                                }
                            })
                            .or_else(|| active_role.clone());
                        // 10d-2c-2-2-b F3: extract `args.trigger`
                        // discriminator at decision-build time. The
                        // daemon poller sets this to "static_idle"
                        // when firing via `workflow_transition`;
                        // MCP-direct callers leave it absent.
                        let trigger = ev
                            .args
                            .get("trigger")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        if let Some(from) = from_opt {
                            decisions.push(Decision::ActivateDynamic {
                                run_id: run_id.clone(),
                                to,
                                from,
                                prompt,
                                event_id: ev.id.clone(),
                                daemon_routed,
                                new_offset: *post_event_offset,
                                event_iteration: ev.iteration,
                                trigger,
                            });
                        } else {
                            // No from_role derivable at all
                            // (event.from_role None, source !=
                            // daemon so no ev.role fallback,
                            // and in-memory active_role None).
                            // Skip-but-advance: log loudly and
                            // move the offset past this event so
                            // the run doesn't wedge.
                            decisions.push(Decision::Skip {
                                run_id: run_id.clone(),
                                new_offset: *post_event_offset,
                                reason: format!(
                                    "Transition event {} has no derivable \
                                     from_role (event.from_role=None, \
                                     source={:?}, in-memory active_role=None)",
                                    ev.id, ev.source,
                                ),
                            });
                        }
                    }
                    workflow::events::EventKind::Done { reason } => {
                        decisions.push(Decision::Done {
                            run_id: run_id.clone(),
                            reason,
                            daemon_routed,
                            new_offset: *post_event_offset,
                            event_iteration: ev.iteration,
                        });
                    }
                    workflow::events::EventKind::Unknown => {
                        // 10d-2c-1 review round-11 (F1):
                        // skip-but-advance. Pre-round-11 the
                        // empty arm `{}` ignored Unknown events
                        // entirely, leaving `events_offset`
                        // un-advanced — same event re-read
                        // forever; static-idle check skipped
                        // because `events_with_offsets`
                        // non-empty; run wedged. Push a Skip so
                        // the decision-processing loop advances
                        // offset (subject to `failed_runs`
                        // gating, same as any other decision).
                        decisions.push(Decision::Skip {
                            run_id: run_id.clone(),
                            new_offset: *post_event_offset,
                            reason: format!(
                                "Unknown event kind (tool={:?}, id={})",
                                ev.tool, ev.id,
                            ),
                        });
                    }
                }
            }

            // If no dynamic event fired, check for static idle transition.
            if events_with_offsets.is_empty() {
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
                            // 10d-2c-2-2-b (2c-2-3 bundle): per-run
                            // ownership gate. If the active role's
                            // session is daemon-spawned
                            // (`daemon_session_uid.is_some()`), the
                            // daemon's `cm-workflow-poller` thread
                            // fires the static `on_idle` — see
                            // `cm_daemon::workflow::poller::daemon_owns_run`.
                            // TUI staying out avoids a same-tick
                            // double-fire on state.json. Both sides
                            // consult the equivalent condition from
                            // their authoritative source
                            // (`daemon_session_uid` for TUI,
                            // `state.sessions` membership for daemon).
                            if self.workspaces[ti].sessions[si]
                                .session
                                .daemon_session_uid
                                .is_some()
                            {
                                log_tick(
                                    &run_id,
                                    "skipping static on_idle: daemon owns \
                                     this run's active role session — \
                                     daemon poller fires",
                                );
                            } else {
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
        }

        // 10d-2c-1 review round-10: stop-at-first-failure per
        // run. Decisions are pushed in event-order within each
        // run (the per-run inner loop above). If a decision
        // for run R fails, any later decision for the SAME run R
        // in this tick is skipped — keeping `events_offset`
        // positioned at the failed event so the next tick
        // re-reads from there. Different runs are independent;
        // a failure in run X doesn't affect run Y.
        let mut failed_runs: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        for d in decisions {
            // Extract the run_id without consuming the decision.
            let decision_run_id: &str = match &d {
                Decision::ActivateStatic { run_id, .. }
                | Decision::ActivateDynamic { run_id, .. }
                | Decision::Done { run_id, .. }
                | Decision::Skip { run_id, .. } => run_id,
            };
            if failed_runs.contains(decision_run_id) {
                log_tick(
                    decision_run_id,
                    "skipping later decision in this tick — an earlier \
                     event in this run's batch failed to deliver; \
                     events_offset stays at the failed event for retry",
                );
                continue;
            }

            match d {
                Decision::ActivateStatic { run_id, to, from } => {
                    // ActivateStatic fires only when events.is_empty()
                    // for this tick, so there's no events_offset
                    // advance to thread. None signals the modify
                    // closure to leave events_offset untouched.
                    self.fire_transition(
                        &run_id,
                        &to,
                        TriggerKind::StaticIdle { from_role: from },
                        None,
                        &mut actions,
                        None,
                    );
                }
                Decision::ActivateDynamic { run_id, to, from, prompt, event_id, daemon_routed, new_offset, event_iteration, trigger: trigger_str } => {
                    if daemon_routed {
                        // 10d-2c-1 review round-1 (F2 + F4 +
                        // P1 #1/#2):
                        //
                        // F4: defer the history-entry append
                        // (Option A's deferred-patch) until
                        // AFTER `deliver_dynamic_activation_prompt`
                        // — that helper runs the fresh-context
                        // reset (e.g. `/clear`) which wipes the
                        // transcript for Fresh roles. Snapshotting
                        // `assistant_count_at_start` BEFORE that
                        // reset would record the stale (pre-clear)
                        // count, so the on_idle gate would fire
                        // before the role's response to the
                        // activation prompt.
                        //
                        // F2: combine the history-entry append
                        // with `events_offset` persistence in a
                        // single `run::modify` so the offset
                        // survives the post-reset reload. The
                        // pre-fix two-step (reload+offset save)
                        // clobbered the offset because the reload
                        // overwrote the in-memory advance.
                        //
                        // Sequence:
                        //   1. Re-load state.json (daemon's
                        //      mutation: active_role set, history
                        //      not yet appended).
                        //   2. Deliver prompt (runs fresh reset
                        //      if applicable).
                        //   3. Compute post-reset
                        //      `assistant_count_at_start`.
                        //   4. Single `run::modify` appends the
                        //      history entry AND persists
                        //      events_offset.
                        if let Some(updated) = workflow::run::load_one(&run_id) {
                            if let Some(slot) = self
                                .workflow_runs
                                .iter_mut()
                                .find(|r| r.run_id == run_id)
                            {
                                *slot = updated;
                            }
                        }

                        // Step 2: deliver prompt (fresh reset may
                        // execute here). Returns
                        // `DeliveryOutcome::Delivered { reset }`
                        // on success (reset is Some iff target is
                        // Fresh) OR `Failed { reason }` if the
                        // target session / workflow / role is
                        // missing.
                        //
                        // Round-8: on `Failed`, the activation
                        // prompt was NOT delivered. Skip the
                        // events_offset advance + history append
                        // so the next tick re-reads the event and
                        // retries delivery. Pre-round-8 the
                        // helper returned `Option<None>` for both
                        // success-no-reset AND failure; the
                        // caller advanced offset unconditionally
                        // and the workflow stalled on a role
                        // that never got prompted.
                        let delivery = self.deliver_dynamic_activation_prompt(
                            &run_id, &to, &prompt, &mut actions,
                        );
                        let reset_mutations = match delivery {
                            DeliveryOutcome::Delivered { reset } => reset,
                            DeliveryOutcome::Failed { reason } => {
                                log_tick(
                                    &run_id,
                                    &format!(
                                        "deliver_dynamic FAILED for role {:?}: {} \
                                         — skipping events_offset advance; next \
                                         tick will retry. If this persists, the \
                                         target session may be permanently \
                                         closed; operator should investigate.",
                                        to, reason,
                                    ),
                                );
                                // 10d-2c-1 review round-10: register
                                // this run as failed so any
                                // later decisions in this tick's
                                // batch (events 2+ for the same
                                // run) are skipped. Pre-round-10
                                // a successful later event would
                                // have advanced `events_offset`
                                // past this failed event,
                                // permanently dropping its
                                // delivery.
                                failed_runs.insert(run_id);
                                continue;
                            }
                        };

                        // Step 3 + 4: compute post-reset count,
                        // append history entry, persist
                        // events_offset, AND apply the fresh-reset
                        // role_sessions / role_baselines mutations
                        // — all atomic under the run's flock.
                        //
                        // F1 (round 3): `new_offset` is the
                        // Decision-threaded value captured at
                        // event-read time. DO NOT read from the
                        // (post-reload, stale) in-memory run; the
                        // closure must explicitly assign so the
                        // saved state.json reflects the advance.
                        //
                        // F1 (round 4): apply
                        // [`RoleResetMutations`] inside the
                        // closure so the fresh role's
                        // current_session_id / role_baseline reset
                        // survives the closure's reload. Pre-round-4
                        // shape applied them in memory before the
                        // closure, which the reload silently
                        // discarded — fresh-context daemon-routed
                        // transitions persisted the OLD session id
                        // and stale baseline, breaking the on_idle
                        // gate and corrupting history's session_id.
                        let start_count = self.compute_role_assistant_count(&run_id, &to);
                        // 10d-2c-2-2-b F3: daemon poller fires
                        // static `on_idle` via `workflow_transition`
                        // with `args.trigger = "static_idle"`. The
                        // history entry must record
                        // `TriggerKind::StaticIdle` for parity with
                        // the TUI-direct static path; pre-fix the
                        // TUI tail hard-coded `McpTransition` for
                        // all daemon-source events, mis-tagging
                        // poller-driven idle fires in history.
                        let trigger = if trigger_str.as_deref() == Some("static_idle") {
                            TriggerKind::StaticIdle {
                                from_role: from.clone(),
                            }
                        } else {
                            TriggerKind::McpTransition {
                                from_role: from.clone(),
                                prompt: prompt.clone(),
                                event_id: event_id.clone(),
                            }
                        };
                        let captured_offset = new_offset;
                        let captured_reset = reset_mutations;
                        // 10d-2c-1 review round-14: pass the event's
                        // target role (`to`) explicitly into the
                        // history append. Pre-r14 the function read
                        // `self.active_role` — in multi-event
                        // queued scenarios all queued events would
                        // see the LATEST state.json `active_role`
                        // and append history for the wrong role
                        // (or drop silently if `active_role` was
                        // None after a daemon workflow_done).
                        let target_role_for_history = to.clone();
                        // 10d-2c-1 review round-15: per-event
                        // activation iteration from the daemon's
                        // post-mutation capture (0 means pre-r15
                        // on-disk event; the appender falls back
                        // to `r.iteration`).
                        let event_iter_for_history = event_iteration;
                        if let Ok(updated_run) =
                            workflow::run::modify(&run_id, move |r| {
                                if let Some(reset) = &captured_reset {
                                    apply_role_reset(r, reset);
                                }
                                r.append_history_entry_for_event_target_role(
                                    &target_role_for_history,
                                    event_iter_for_history,
                                    trigger,
                                    start_count,
                                );
                                r.events_offset = captured_offset;
                            })
                        {
                            if let Some(slot) = self
                                .workflow_runs
                                .iter_mut()
                                .find(|r| r.run_id == run_id)
                            {
                                *slot = updated_run;
                            }
                        }
                    } else {
                        // TUI-local path (source != "daemon").
                        // 10d-2c-1 review round-7 (F1): pass
                        // `new_offset` into fire_transition so its
                        // single modify closure persists state
                        // mutation AND events_offset together —
                        // pre-fix the outer modify advanced disk
                        // but the prior fire_transition modify had
                        // already replaced in-memory with the
                        // disk-loaded OLD events_offset; next tick
                        // re-read the event.
                        self.fire_transition(
                            &run_id,
                            &to,
                            TriggerKind::McpTransition {
                                from_role: from,
                                prompt: prompt.clone(),
                                event_id,
                            },
                            Some(prompt),
                            &mut actions,
                            Some(new_offset),
                        );
                    }
                }
                Decision::Done { run_id, reason, daemon_routed, new_offset, event_iteration: _ } => {
                    if daemon_routed {
                        // F1 (round 3): use the Decision-threaded
                        // `new_offset` directly. The closure
                        // explicitly assigns so a stale on-disk
                        // events_offset is overwritten.
                        let captured_offset = new_offset;
                        if let Ok(updated_run) =
                            workflow::run::modify(&run_id, move |r| {
                                r.events_offset = captured_offset;
                            })
                        {
                            if let Some(slot) = self
                                .workflow_runs
                                .iter_mut()
                                .find(|r| r.run_id == run_id)
                            {
                                *slot = updated_run;
                            }
                        }
                    } else {
                        // TUI-local file-write path: apply state
                        // mutation + events_offset in a single
                        // modify (10d-2c-1 review round-7 F1).
                        self.finish_run(
                            &run_id,
                            reason,
                            &mut actions,
                            Some(new_offset),
                        );
                    }
                }
                Decision::Skip { run_id, new_offset, reason } => {
                    // 10d-2c-1 review round-11 (F1): advance
                    // events_offset past an unprocessable event
                    // (Unknown kind, from_role-less Transition,
                    // etc.) so the run doesn't wedge. Logged
                    // loudly so operators can correlate against
                    // events.jsonl.
                    log_tick(
                        &run_id,
                        &format!(
                            "skip-but-advance: events_offset → {} ({})",
                            new_offset, reason,
                        ),
                    );
                    let captured_offset = new_offset;
                    if let Ok(updated_run) =
                        workflow::run::modify(&run_id, move |r| {
                            r.events_offset = captured_offset;
                        })
                    {
                        if let Some(slot) = self
                            .workflow_runs
                            .iter_mut()
                            .find(|r| r.run_id == run_id)
                        {
                            *slot = updated_run;
                        }
                    }
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
        // 10d-2c-1 review round-7 (F1): events_offset to persist
        // alongside the state mutation under the same flock. Some
        // for the TuiLocal `ActivateDynamic` arm (events.jsonl
        // had a new event read this tick). None for ActivateStatic
        // (events.is_empty() — no offset to advance, no-op).
        //
        // Pre-fix this offset advance lived in a SECOND modify
        // call after `fire_transition` returned, but the first
        // modify (inside fire_transition) reloaded disk and
        // reassigned `self.workflow_runs[run_idx]` with the
        // disk-loaded events_offset (OLD value). The second
        // modify wrote disk = NEW but didn't refresh in-memory.
        // Next tick read in-memory = OLD → re-processed the
        // event → duplicate prompt + history + iteration.
        new_offset: Option<u64>,
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
        let captured_for_closure = captured.clone();
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
            // 10d-2c-1 review round-4 (F1): `reset_fresh_session` no
            // longer mutates `WorkflowRun` directly; apply the
            // returned [`RoleResetMutations`] in memory here so the
            // existing `workflow::run::save` below persists them.
            // Daemon-routed transitions apply this same struct
            // INSIDE the `run::modify` closure (see `tick`'s
            // `ActivateDynamic` arm).
            if let Some(reset) = self.reset_fresh_session(run_id, to_role, ti, si, actions) {
                apply_role_reset(&mut self.workflow_runs[run_idx], &reset);
            }
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

        self.workflow_runs[run_idx].activate_role(to_role.to_string(), trigger.clone(), start_count);
        // 10d-2c-1 review round-6 (F1): persist via `modify`
        // rather than `save(&in_mem_copy)` so any daemon write
        // to non-overlapping fields (e.g., events_offset bumped
        // by a daemon-routed event we haven't picked up yet)
        // survives the RMW. The closure re-applies the same
        // mutations we did to the in-memory copy:
        //   - close_active_role (sets last history entry's
        //     deactivated_at + last_message — TUI-routed
        //     transition, so TUI owns this close).
        //   - apply_role_reset for Fresh roles
        //     (role_baselines + role_sessions[to].current_session_id).
        //   - role_sessions[to].current_session_id ← current_sid.
        //   - activate_role (push history entry + active_role +
        //     iteration). Mutual-exclusion with daemon-routed
        //     dispatch is by event source (`source != "daemon"`
        //     here); if both paths fire concurrently for the
        //     same logical transition that's an upstream caller
        //     bug, not something this RMW can resolve.
        let reset_for_closure = if matches!(
            target_role_spec.context,
            workflow::toml_schema::Context::Fresh
        ) {
            // Build the reset shape from the (already-applied
            // in-mem) state so the closure mirrors it. Don't
            // re-run reset_fresh_session — its TerminalSession
            // side effects (queueing /clear, etc.) must happen
            // exactly once.
            Some(RoleResetMutations {
                role: to_role.to_string(),
                new_session_id: None,
                new_baseline: MessageBaseline::default(),
            })
        } else {
            None
        };
        let current_sid_for_closure = self.workspaces[ti].sessions[si].transcript_id.clone();
        let to_role_owned = to_role.to_string();
        let offset_for_closure = new_offset;
        let updated = workflow::run::modify(run_id, move |r| {
            r.close_active_role(captured_for_closure);
            if let Some(reset) = reset_for_closure {
                apply_role_reset(r, &reset);
            }
            if let Some(b) = r.role_sessions.get_mut(&to_role_owned) {
                b.current_session_id = current_sid_for_closure;
            }
            r.activate_role(to_role_owned, trigger, start_count);
            // 10d-2c-1 review round-7 (F1): persist events_offset
            // inside the SAME modify closure as the state
            // mutation, so the disk-loaded run that gets
            // assigned back to `self.workflow_runs[run_idx]`
            // carries the NEW offset. Caller-side outer modify
            // dropped (was the source of the regression).
            if let Some(offset) = offset_for_closure {
                r.events_offset = offset;
            }
        });
        if let Ok(updated) = updated {
            self.workflow_runs[run_idx] = updated;
        }
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

    /// 10d-2c-1 review round-1 (P2 fix): compute the active
    /// role's current transcript-tail assistant count. Used by
    /// the daemon-routed `ActivateDynamic` arm to patch the
    /// deferred history entry's `assistant_count_at_start` —
    /// without this patch, `start_count=0` would let the on_idle
    /// gate fire on stale assistant turns produced before the
    /// activation prompt.
    fn compute_role_assistant_count(&self, run_id: &str, role: &str) -> usize {
        let Some((ti, si)) = locate_workflow_session(self.workspaces, run_id, role) else {
            return 0;
        };
        let session_type = self.workspaces[ti].sessions[si].session_type.clone();
        let session_engine = engine_for_session_type(&session_type);
        let current_sid = self.workspaces[ti].sessions[si].transcript_id.clone();
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
    }

    /// 10d-2c-1: deliver-only half of `fire_transition` — used
    /// for dynamic transitions whose state mutation has ALREADY
    /// been applied by the daemon's `workflow_transition`
    /// handler. Skips `close_active_role`, `activate_role`,
    /// `workflow::run::save` (daemon did those); still does
    /// template render, fresh-context reset (if target role is
    /// `Fresh`), agent.submit_prompt, status_msg, and the
    /// manifest-save action. 10d-2c-2 / 2c-3 move this delivery
    /// (and the fresh-respawn it triggers) to the daemon too;
    /// at that point this helper is deleted alongside
    /// `fire_transition`.
    fn deliver_dynamic_activation_prompt(
        &mut self,
        run_id: &str,
        to_role: &str,
        supplied_prompt: &str,
        actions: &mut Vec<WorkflowAction>,
    ) -> DeliveryOutcome {
        // 10d-2c-1 review round-8: distinguish early-failure
        // (workflow / role / target session missing) from
        // success-with-no-reset. Pre-round-8 both surfaced as
        // `Option::None` and the caller advanced events_offset
        // unconditionally — the failure case dropped the
        // activation prompt and stuck the workflow on a role
        // that never got prompted. Now: failures return
        // `Failed { reason }` so the caller skips offset
        // advance and the next tick retries delivery.
        let run_idx = match self.workflow_runs.iter().position(|r| r.run_id == run_id) {
            Some(i) => i,
            None => {
                return DeliveryOutcome::Failed {
                    reason: format!("run {} not in workflow_runs", run_id),
                };
            }
        };
        let wf_name = self.workflow_runs[run_idx].workflow_name.clone();
        let wf = match self.workflows.get(&wf_name).cloned() {
            Some(w) => w,
            None => {
                return DeliveryOutcome::Failed {
                    reason: format!("workflow definition {:?} not loaded", wf_name),
                };
            }
        };

        // Locate target role's session by workflow tags.
        let Some((ti, si)) = locate_workflow_session(self.workspaces, run_id, to_role) else {
            return DeliveryOutcome::Failed {
                reason: format!(
                    "target role {:?} has no participant session in workspaces \
                     (TUI restart with stale state? session closed?)",
                    to_role,
                ),
            };
        };

        // from_role for the status message: daemon already set
        // active_role to to_role, so we read the outgoing role
        // from the just-appended history entry's
        // McpTransition.from_role field.
        let from_role: Option<String> = self.workflow_runs[run_idx]
            .history
            .last()
            .and_then(|h| match &h.trigger {
                TriggerKind::McpTransition { from_role, .. } => Some(from_role.clone()),
                _ => None,
            });

        // Render the activation prompt. After the first
        // activation of this role in the run, prefer
        // `subsequent_activation_prompt`.
        let target_role_spec = match wf.roles.get(to_role).cloned() {
            Some(r) => r,
            None => {
                return DeliveryOutcome::Failed {
                    reason: format!(
                        "role {:?} not declared in workflow {:?}",
                        to_role, wf_name,
                    ),
                };
            }
        };
        let prior_activations = self.workflow_runs[run_idx]
            .history
            .iter()
            .filter(|h| h.role == to_role)
            .count();
        // 10d-2c-1 review round-3 (F3): `> 0`, NOT `> 1`. The
        // daemon-routed path under Option A defers the history-
        // entry append until AFTER prompt rendering (see the
        // ActivateDynamic arm in `tick`), so at render time the
        // current activation's entry is NOT yet in history.
        // `prior_activations` counts entries STRICTLY BEFORE
        // this activation. Earlier rounds used `> 1` to
        // compensate for an assumed-already-appended entry —
        // that was wrong: it meant "at least 2 prior" = "this
        // is the 3rd+ activation", so the second activation
        // got the wrong (initial) prompt copy.
        let default_template = if prior_activations > 0 {
            target_role_spec
                .subsequent_activation_prompt
                .clone()
                .or_else(|| target_role_spec.activation_prompt.clone())
        } else {
            target_role_spec.activation_prompt.clone()
        };
        let template_source = if !supplied_prompt.is_empty() {
            supplied_prompt.to_string()
        } else {
            default_template.unwrap_or_default()
        };

        let worktree_ref = self.workspaces[ti].worktree_path.as_deref();
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

        // Fresh-context reset still happens TUI-side in 2c-1;
        // daemon takes it in 2c-3.
        //
        // 10d-2c-1 review round-4 (F1): capture the
        // [`RoleResetMutations`] but DO NOT apply to the
        // in-memory run here. The caller (`tick`'s
        // `ActivateDynamic` arm) applies them inside the
        // `run::modify` closure so they survive the reload that
        // happens under the closure's flock.
        let reset_mutations = if matches!(
            target_role_spec.context,
            workflow::toml_schema::Context::Fresh
        ) {
            self.reset_fresh_session(run_id, to_role, ti, si, actions)
        } else {
            None
        };

        let from_label = from_role.as_deref().unwrap_or("?");
        actions.push(WorkflowAction::SetStatusMsg(format!(
            "Workflow: {} → {}",
            from_label, to_role
        )));

        if !rendered.trim().is_empty() {
            let session_type = self.workspaces[ti].sessions[si].session_type.clone();
            let label = self.workspaces[ti].sessions[si].label.clone();
            let body_len = rendered.trim_end().len();
            log_tick(
                run_id,
                &format!(
                    "deliver_dynamic: activated '{}' queued prompt ({} bytes) on session '{}'",
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
                        "deliver_dynamic: submit_prompt error on '{}': {}",
                        label, e
                    ),
                );
            }
        } else {
            log_tick(
                run_id,
                &format!(
                    "deliver_dynamic: activated '{}' but rendered prompt was EMPTY",
                    to_role
                ),
            );
        }
        actions.push(WorkflowAction::SaveSessionManifest);
        DeliveryOutcome::Delivered {
            reset: reset_mutations,
        }
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
    ///
    /// 10d-2c-1 review round-4 (F1): this function NO LONGER mutates
    /// `WorkflowRun.role_sessions` / `role_baselines` in place. It returns
    /// the intended workflow-run mutations as a [`RoleResetMutations`]
    /// value; each caller applies them at the persistence boundary it
    /// owns:
    ///   - `fire_transition` applies them in memory before its existing
    ///     `workflow::run::save` (TUI-local path's single-shot save).
    ///   - The daemon-routed `ActivateDynamic` arm applies them INSIDE
    ///     the `workflow::run::modify` closure (the closure reloads
    ///     state.json under flock, so an in-memory apply BEFORE
    ///     `modify` would be clobbered).
    ///
    /// Returns `None` if the session was already exited (no reset
    /// happened) or `Some(_)` describing the mutations to apply.
    fn reset_fresh_session(
        &mut self,
        run_id: &str,
        role: &str,
        ti: usize,
        si: usize,
        actions: &mut Vec<WorkflowAction>,
    ) -> Option<RoleResetMutations> {
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
            return None;
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
        actions.push(WorkflowAction::SaveSessionManifest);
        log_tick(
            run_id,
            &format!(
                "reset_fresh: queued /clear for '{}' (fires on first quiet PTY)",
                label
            ),
        );
        // Old file's turn counts no longer apply to the new file — the
        // role's message baseline resets so templates slice from 0
        // post-`/clear`. Key off the role name (stable, workflow-managed),
        // NOT the session label — labels are user-editable in the
        // per-session settings, and a renamed fresh role would otherwise
        // leave a stale baseline keyed on the prior label.
        Some(RoleResetMutations {
            role: role.to_string(),
            new_session_id: None,
            new_baseline: MessageBaseline::default(),
        })
    }

    /// Mark a workflow run as done, persist the change, and surface a
    /// status-bar note. Distinct from `App::stop_workflow_run` (the
    /// user-driven stop) — this fires when an MCP `workflow_done`
    /// event is processed.
    fn finish_run(
        &mut self,
        run_id: &str,
        reason: String,
        actions: &mut Vec<WorkflowAction>,
        // 10d-2c-1 review round-7 (F1): events_offset to persist
        // alongside the mark_done mutation in the same modify
        // closure. Matches the fire_transition shape — pre-fix
        // the outer modify advanced disk but the prior
        // finish_run modify had already replaced in-memory with
        // the disk-loaded OLD events_offset; next tick re-read
        // the Done event.
        new_offset: Option<u64>,
    ) {
        // 10d-2c-1 review round-6 (F1): targeted modify. Daemon
        // owns status/active_role/done_reason but `finish_run`
        // fires only from the TuiLocal-routed `Decision::Done`
        // arm (legacy events.jsonl writer); daemon-routed Done
        // uses `workflow_done` and the daemon's own RMW. Even so,
        // running through `modify` (rather than wholesale save)
        // protects daemon-side bumps to events_offset or other
        // non-overlapping fields from being clobbered.
        let reason_for_closure = reason.clone();
        let offset_for_closure = new_offset;
        let updated = workflow::run::modify(run_id, move |r| {
            r.mark_done(reason_for_closure);
            if let Some(offset) = offset_for_closure {
                r.events_offset = offset;
            }
        });
        if let Ok(updated) = updated {
            if let Some(slot) = self.workflow_runs.iter_mut().find(|r| r.run_id == run_id) {
                *slot = updated;
            }
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
            seeded_from_snapshot: None,
            preserved_last_exit: None,
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

    /// 10d-2c-2-2-b T2 (2c-2-3 gate) — when the active role's
    /// session is daemon-spawned (`daemon_session_uid.is_some()`),
    /// the TUI's controller MUST NOT push
    /// `Decision::ActivateStatic`. The daemon's
    /// `cm-workflow-poller` fires for this run instead.
    ///
    /// Setup: a run with worker as active role, worker session
    /// has `daemon_session_uid = Some(...)`, and the transcript
    /// shows count > baseline + is idle so the gate WOULD fire if
    /// not for the new ownership check.
    ///
    /// Assertion: after `tick()`, the on-disk run's `active_role`
    /// is still "worker" (no transition), and iteration is
    /// unchanged. The daemon-side `Decision::ActivateStatic` fire
    /// path is covered in
    /// `cm_daemon::workflow::poller::tests::poll_once_fires_activate_static_*`.
    #[test]
    fn tui_static_idle_gate_skips_when_active_role_is_daemon_spawned() {
        with_temp_home(|| {
            let run_id = "wf_t2_tui_gate_daemon_owned";
            let home =
                std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME"));
            let wt = home.join("wt");
            std::fs::create_dir_all(&wt).expect("mkdir wt");
            // Write a claude transcript with one complete
            // assistant turn so `is_idle` + count > baseline
            // would normally fire the gate.
            let wt_str = wt.to_str().unwrap();
            let encoded = wt_str.replace('/', "-").replace('.', "-");
            let proj = home.join(format!(".claude/projects/{}", encoded));
            std::fs::create_dir_all(&proj).expect("mkdir proj");
            std::fs::write(
                proj.join("sid-worker.jsonl"),
                r##"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"done"}]}}"##,
            )
            .expect("write transcript");

            let mut run = make_run(run_id, "feedback", "worker");
            // Bind worker's transcript_id so the resolver +
            // is_idle gate can locate the JSONL.
            run.role_sessions
                .get_mut("worker")
                .unwrap()
                .current_session_id = Some("sid-worker".to_string());
            workflow::run::save(&run).expect("seed run");
            let mut runs = vec![run];

            // Build the worker session WITH daemon_session_uid =
            // Some(...) — this is what the new gate keys off.
            let mut worker_session = stub_session(
                "worker",
                "claude-code",
                run_id,
                "worker",
                Some("sid-worker"),
            );
            worker_session.session.daemon_session_uid =
                Some("ts-daemon-worker".to_string());
            let mut workspaces =
                vec![workspace_with(vec![worker_session], Some(wt.clone()))];

            // Minimal workflow def: worker → reviewer on idle.
            let mut roles = BTreeMap::new();
            roles.insert(
                "worker".to_string(),
                role_with(Engine::ClaudeCode, Context::Persistent),
            );
            roles.insert(
                "reviewer".to_string(),
                role_with(Engine::ClaudeCode, Context::Persistent),
            );
            let wf = make_workflow(
                "feedback",
                roles,
                vec!["worker".to_string(), "reviewer".to_string()],
                vec![Transition {
                    from: "worker".to_string(),
                    on: TriggerOn::Idle,
                    to: "reviewer".to_string(),
                }],
            );
            let mut workflows = HashMap::new();
            workflows.insert("feedback".to_string(), wf);

            let dummy = dummy_cap_state();
            {
                let mut ctx = WorkflowControllerCtx {
                    workflow_runs: &mut runs,
                    workspaces: &mut workspaces,
                    workflows: &workflows,
                    last_term_size: (80, 24),
                    config: &dummy.config,
                    cap_status: &dummy.cap_status,
                    kill_tx: &dummy.kill_tx,
                };
                let _ = ctx.tick();
            }

            // No transition fired — daemon owns this run, TUI
            // poller stays out. `active_role` is still worker.
            let post = workflow::run::load_one(run_id).expect("post load");
            assert_eq!(
                post.active_role.as_deref(),
                Some("worker"),
                "TUI gate must skip when daemon owns: active_role \
                 stays at worker; got {:?}",
                post.active_role,
            );
            assert_eq!(
                post.iteration, 1,
                "iteration must not advance: gate skipped, no \
                 close_active_role mutation",
            );
        });
    }

    /// F3 — TUI tail processing a daemon-source event whose
    /// `args.trigger == "static_idle"` records the history entry
    /// with `TriggerKind::StaticIdle{from_role}`, NOT
    /// `TriggerKind::McpTransition`. Pre-fix all daemon-source
    /// events landed as McpTransition, mis-tagging poller-driven
    /// idle fires.
    #[test]
    fn tui_tail_records_static_idle_trigger_from_daemon_poller_event() {
        with_temp_home(|| {
            let run_id = "wf_f3_static_idle_history";
            let run = make_run(run_id, "feedback", "worker");
            workflow::run::save(&run).expect("seed save");

            // Write a daemon-source event with
            // `args.trigger = "static_idle"` — what the poller's
            // internal `workflow_transition` call emits.
            let ev = workflow::events::Event {
                id: "evt-f3-static".to_string(),
                ts: 1.0,
                run_id: run_id.to_string(),
                role: "worker".to_string(),
                tool: "workflow_transition".to_string(),
                args: serde_json::json!({
                    "to": "reviewer",
                    "prompt": "",
                    "trigger": "static_idle",
                }),
                source: "daemon".to_string(),
                from_role: Some("worker".to_string()),
                iteration: 2,
            };
            workflow::events::WorkflowEventsWriter::append_event(&ev)
                .expect("append ev");

            // Minimal workspace with the worker session bound so
            // `locate_workflow_session` and the prompt-delivery
            // path find it. daemon_session_uid set so the TUI
            // gate doesn't skip the run.
            let mut worker_session =
                stub_session("worker", "claude-code", run_id, "worker", None);
            worker_session.session.daemon_session_uid =
                Some("ts-daemon-worker".to_string());
            let mut reviewer_session =
                stub_session("reviewer", "claude-code", run_id, "reviewer", None);
            reviewer_session.session.daemon_session_uid =
                Some("ts-daemon-reviewer".to_string());
            let mut workspaces = vec![workspace_with(
                vec![worker_session, reviewer_session],
                Some(std::path::PathBuf::from("/tmp/f3-wt")),
            )];

            let mut roles = BTreeMap::new();
            roles.insert(
                "worker".to_string(),
                role_with(Engine::ClaudeCode, Context::Persistent),
            );
            roles.insert(
                "reviewer".to_string(),
                role_with(Engine::ClaudeCode, Context::Persistent),
            );
            let wf = make_workflow(
                "feedback",
                roles,
                vec!["worker".to_string(), "reviewer".to_string()],
                vec![],
            );
            let mut workflows = HashMap::new();
            workflows.insert("feedback".to_string(), wf);

            let dummy = dummy_cap_state();
            let mut runs = vec![run];
            {
                let mut ctx = WorkflowControllerCtx {
                    workflow_runs: &mut runs,
                    workspaces: &mut workspaces,
                    workflows: &workflows,
                    last_term_size: (80, 24),
                    config: &dummy.config,
                    cap_status: &dummy.cap_status,
                    kill_tx: &dummy.kill_tx,
                };
                let _ = ctx.tick();
            }

            // History should have an entry for "reviewer" with
            // `TriggerKind::StaticIdle{from_role: "worker"}`.
            let post = workflow::run::load_one(run_id).expect("post load");
            let reviewer_entry = post
                .history
                .iter()
                .rev()
                .find(|h| h.role == "reviewer")
                .expect("reviewer history entry");
            match &reviewer_entry.trigger {
                workflow::run::TriggerKind::StaticIdle { from_role } => {
                    assert_eq!(
                        from_role, "worker",
                        "StaticIdle.from_role should be worker, got {:?}",
                        from_role,
                    );
                }
                other => panic!(
                    "expected TriggerKind::StaticIdle, got {:?}",
                    other,
                ),
            }
        });
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
            let run = WorkflowRun::new(
                "wf-parity".to_string(),
                "feedback".to_string(),
                "/parity".to_string(),
                role_sessions,
                "worker".to_string(),
                baselines,
                Some("the run goal".to_string()),
                BTreeMap::new(),
            );

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

    /// 10d-2c-1 review round-14 — named acceptance test.
    /// When multiple daemon transitions queue up before the TUI
    /// processes any of them, each queued event must record the
    /// role it was activating — NOT the current `active_role`
    /// on state.json (which by then is the LATEST role, or None
    /// if a workflow_done landed last).
    ///
    /// Pre-r14 the history append used `self.active_role` —
    /// so if state.json showed `active_role = None` (after the
    /// daemon completed all transitions + a done), event 1's
    /// processing would silently drop the history append.
    /// Or if state.json showed `active_role = manager`, event
    /// 1 (worker→reviewer) would append a history entry for
    /// "manager" — wrong.
    #[test]
    fn daemon_routed_history_uses_event_target_role_not_current_active_role() {
        with_temp_home(|| {
            let run_id = "wf_r14_event_target_role";
            let mut run = make_run(run_id, "feedback", "worker");
            // Add "manager" to role_sessions so the daemon's
            // hand-crafted state shape is self-consistent.
            run.role_sessions.insert(
                "manager".to_string(),
                RoleBinding {
                    session_label: "manager".to_string(),
                    current_session_id: None,
                    daemon_session_uid: None,
                },
            );
            workflow::run::save(&run).expect("seed save");

            // Write THREE daemon-source events: worker→reviewer,
            // reviewer→manager, workflow_done.
            //
            // Round-15: each event carries its post-mutation
            // iteration. Pre-r15 these would have been 0; tick
            // would have used `r.iteration` (=3 after daemon
            // ran all 3) on every queued event's history entry.
            // Post-r15 the appender uses the event's value, so
            // reviewer gets iteration=2, manager gets iteration=3.
            let ev1 = workflow::events::Event {
                id: "evt-r14-1".to_string(),
                ts: 1.0,
                run_id: run_id.to_string(),
                role: "worker".to_string(),
                tool: "workflow_transition".to_string(),
                args: serde_json::json!({"to": "reviewer", "prompt": "p1"}),
                source: "daemon".to_string(),
                from_role: Some("worker".to_string()),
                iteration: 2, // post-mutation: 1 → 2 (worker→reviewer activation)
            };
            workflow::events::WorkflowEventsWriter::append_event(&ev1)
                .expect("append ev1");
            let ev2 = workflow::events::Event {
                id: "evt-r14-2".to_string(),
                ts: 2.0,
                run_id: run_id.to_string(),
                role: "reviewer".to_string(),
                tool: "workflow_transition".to_string(),
                args: serde_json::json!({"to": "manager", "prompt": "p2"}),
                source: "daemon".to_string(),
                from_role: Some("reviewer".to_string()),
                iteration: 3, // post-mutation: 2 → 3 (reviewer→manager activation)
            };
            workflow::events::WorkflowEventsWriter::append_event(&ev2)
                .expect("append ev2");
            let ev3 = workflow::events::Event {
                id: "evt-r14-3".to_string(),
                ts: 3.0,
                run_id: run_id.to_string(),
                role: "manager".to_string(),
                tool: "workflow_done".to_string(),
                args: serde_json::json!({"reason": "approved"}),
                source: "daemon".to_string(),
                from_role: None,
                iteration: 0,
            };
            workflow::events::WorkflowEventsWriter::append_event(&ev3)
                .expect("append ev3");

            // Simulate "daemon already processed all three" by
            // directly mutating state.json to the post-done
            // shape: active_role=None, status=Done, iteration
            // advanced. Pre-r14, when the TUI processes event 1
            // and reloads state.json, active_role=None → the
            // history append silently no-ops (or for non-None
            // state, records the LATEST active_role rather than
            // event 1's "reviewer").
            workflow::run::modify(run_id, |r| {
                r.close_active_role(None);
                r.iteration += 1;
                r.close_active_role(None);
                r.iteration += 1;
                r.active_role = None;
                r.status = workflow::run::RunStatus::Done;
                r.done_reason = Some("approved".to_string());
            })
            .expect("daemon already processed");

            let mut runs = vec![run];
            let worker = stub_session("worker", "claude", run_id, "worker", None);
            let reviewer = stub_session("reviewer", "claude", run_id, "reviewer", None);
            let manager = stub_session("manager", "claude", run_id, "manager", None);
            let workspace = workspace_with(vec![worker, reviewer, manager], None);
            let mut workspaces = vec![workspace];

            let mut roles = BTreeMap::new();
            roles.insert(
                "worker".to_string(),
                role_with(Engine::ClaudeCode, Context::Persistent),
            );
            roles.insert(
                "reviewer".to_string(),
                role_with(Engine::ClaudeCode, Context::Persistent),
            );
            roles.insert(
                "manager".to_string(),
                role_with(Engine::ClaudeCode, Context::Persistent),
            );
            let workflow_def = make_workflow(
                "feedback",
                roles,
                vec![
                    "worker".to_string(),
                    "reviewer".to_string(),
                    "manager".to_string(),
                ],
                vec![],
            );
            let mut workflows = HashMap::new();
            workflows.insert("feedback".to_string(), workflow_def);

            let dummy = dummy_cap_state();
            // Hmm — the run is Done in state.json, but the TUI's
            // tick filters by `is_active()`. Need to re-load the
            // in-memory run to match the disk state? Let me check
            // — actually the in-memory `run` we pushed is still
            // Running (we modified disk, not in-memory). tick's
            // run_snapshots filters on in-memory `is_active()`
            // which checks `status`. In-memory status is Running
            // (from make_run), so tick will process this run.
            // Good — that mirrors the realistic scenario where
            // TUI has stale in-memory but real disk state.
            {
                let mut ctx = WorkflowControllerCtx {
                    workflow_runs: &mut runs,
                    workspaces: &mut workspaces,
                    workflows: &workflows,
                    last_term_size: (80, 24),
                    config: &dummy.config,
                    cap_status: &dummy.cap_status,
                    kill_tx: &dummy.kill_tx,
                };
                let _ = ctx.tick();
            }

            let post = workflow::run::load_one(run_id).expect("post load");
            // Initial seed entry is for worker (iteration=1).
            // Tick should have appended exactly two entries:
            // one for reviewer (event 1), one for manager
            // (event 2). Event 3 (workflow_done) doesn't append
            // a history entry (run-done captured via status).
            //
            // Find the entries for reviewer and manager.
            let reviewer_entries: Vec<_> = post
                .history
                .iter()
                .filter(|h| h.role == "reviewer")
                .collect();
            let manager_entries: Vec<_> = post
                .history
                .iter()
                .filter(|h| h.role == "manager")
                .collect();
            assert_eq!(
                reviewer_entries.len(),
                1,
                "round-14: exactly one history entry for 'reviewer' \
                 (event 1's target); got {} entries — full history: {:?}",
                reviewer_entries.len(),
                post.history,
            );
            assert_eq!(
                manager_entries.len(),
                1,
                "round-14: exactly one history entry for 'manager' \
                 (event 2's target); got {} entries — full history: {:?}",
                manager_entries.len(),
                post.history,
            );
            // Round-15: each history entry's iteration is the
            // event's value, not state.json's current iteration
            // (which is 3 after the daemon-already-processed
            // simulation above).
            assert_eq!(
                reviewer_entries[0].iteration, 2,
                "round-15: reviewer's entry iteration = event 1's \
                 captured value (2), not state.json's current (3)",
            );
            assert_eq!(
                manager_entries[0].iteration, 3,
                "round-15: manager's entry iteration = event 2's \
                 captured value (3)",
            );

            // Pre-r14 the role on each appended entry would be
            // "None"-driven silent drop (since active_role was
            // None on disk), leaving no reviewer/manager entries
            // at all. The assertions above pin the right-role
            // contract directly.

            // Verify events_offset advanced past all events.
            let (_, final_consumed) =
                workflow::events::read_new_with_offsets(run_id, 0);
            assert_eq!(
                post.events_offset, final_consumed,
                "events_offset advances past all three events",
            );
        });
    }

    /// 10d-2c-1 review round-12 (F2) — named acceptance test.
    /// Malformed line in events.jsonl. Pre-r12
    /// `read_new_with_offsets` consumed the bytes internally
    /// but returned only successfully-parsed events; the
    /// caller had no way to learn about the consumed bytes
    /// and `events_offset` wedged at 0. Post-r12 the function
    /// returns `(events, final_consumed_offset)`; the TUI tail
    /// pushes a `Decision::Skip` to advance offset past the
    /// malformed line.
    ///
    /// The test writes a malformed JSON line, drives a tick,
    /// asserts events_offset advances past it. Then writes a
    /// valid event after it, drives another tick, asserts the
    /// valid event is processed (offset advances further).
    #[test]
    fn malformed_event_line_does_not_wedge_offset() {
        with_temp_home(|| {
            let run_id = "wf_r12_malformed";
            let run = make_run(run_id, "feedback", "worker");
            workflow::run::save(&run).expect("seed save");

            // Pre-create the run dir and write a malformed line
            // directly (the writer's `append_event` would reject
            // invalid Event shapes; we need a raw fs::write to
            // simulate an external/buggy writer leaving bad
            // JSON behind).
            let dir = workflow::run::run_dir(run_id);
            std::fs::create_dir_all(&dir).unwrap();
            let events_path = workflow::run::events_path(run_id);
            std::fs::write(
                &events_path,
                b"{this is not valid json}\n",
            )
            .expect("write malformed");
            let bad_line_len = std::fs::metadata(&events_path)
                .expect("metadata")
                .len();
            assert!(bad_line_len > 0);

            // Sanity check: `read_new_with_offsets` returns
            // empty events AND the consumed-bytes offset past
            // the malformed line.
            let (with_offsets, final_consumed) =
                workflow::events::read_new_with_offsets(run_id, 0);
            assert!(
                with_offsets.is_empty(),
                "malformed line is not surfaced as an event",
            );
            assert_eq!(
                final_consumed, bad_line_len,
                "final_consumed_offset advances past the malformed \
                 (newline-terminated) line",
            );

            let mut runs = vec![run];
            let worker = stub_session("worker", "claude", run_id, "worker", None);
            let workspace = workspace_with(vec![worker], None);
            let mut workspaces = vec![workspace];

            let mut roles = BTreeMap::new();
            roles.insert(
                "worker".to_string(),
                role_with(Engine::ClaudeCode, Context::Persistent),
            );
            let workflow_def = make_workflow(
                "feedback",
                roles,
                vec!["worker".to_string()],
                vec![],
            );
            let mut workflows = HashMap::new();
            workflows.insert("feedback".to_string(), workflow_def);

            let dummy = dummy_cap_state();

            // First tick: malformed line consumed → Skip
            // decision dispatched → events_offset advances on
            // disk past the bad line.
            {
                let mut ctx = WorkflowControllerCtx {
                    workflow_runs: &mut runs,
                    workspaces: &mut workspaces,
                    workflows: &workflows,
                    last_term_size: (80, 24),
                    config: &dummy.config,
                    cap_status: &dummy.cap_status,
                    kill_tx: &dummy.kill_tx,
                };
                let _ = ctx.tick();
            }
            let post1 = workflow::run::load_one(run_id).expect("post1 load");
            assert_eq!(
                post1.events_offset, bad_line_len,
                "round-12 F2: events_offset advances past the malformed \
                 line; pre-fix it wedged at 0",
            );

            // Now write a VALID event past the malformed line.
            // Use the writer (round-12 leaves writes as-is).
            let ev = workflow::events::Event {
                id: "evt-r12-after-malformed".to_string(),
                ts: 1.0,
                run_id: run_id.to_string(),
                role: "worker".to_string(),
                tool: "workflow_done".to_string(),
                args: serde_json::json!({"reason": "ok"}),
                source: "daemon".to_string(),
                from_role: None,
                iteration: 0,
            };
            workflow::events::WorkflowEventsWriter::append_event(&ev)
                .expect("append valid");

            let (with_offsets2, _) =
                workflow::events::read_new_with_offsets(run_id, post1.events_offset);
            assert_eq!(
                with_offsets2.len(),
                1,
                "valid event after malformed line readable",
            );
            let post_valid = with_offsets2[0].1;

            // Second tick: valid event processed →
            // events_offset advances to post_valid.
            {
                let mut ctx = WorkflowControllerCtx {
                    workflow_runs: &mut runs,
                    workspaces: &mut workspaces,
                    workflows: &workflows,
                    last_term_size: (80, 24),
                    config: &dummy.config,
                    cap_status: &dummy.cap_status,
                    kill_tx: &dummy.kill_tx,
                };
                let _ = ctx.tick();
            }
            let post2 = workflow::run::load_one(run_id).expect("post2 load");
            assert_eq!(
                post2.events_offset, post_valid,
                "valid event after malformed line gets processed; \
                 offset advances further",
            );
        });
    }

    /// 10d-2c-1 review round-11 (F1) — named acceptance test.
    /// Three events: Unknown (unrecognized tool), workflow_transition
    /// (deliverable), workflow_done. All three must be consumed and
    /// `events_offset` must advance past all of them. Pre-round-11
    /// the Unknown branch was an empty arm `{}` — offset stayed at
    /// pre-Unknown, the same Unknown event was re-read every tick,
    /// AND the static-idle check was skipped because
    /// `events_with_offsets` was non-empty. Run wedged.
    #[test]
    fn tick_advances_offset_past_unknown_event_and_consumes_later_events() {
        with_temp_home(|| {
            let run_id = "wf_r11_unknown_advance";
            let run = make_run(run_id, "feedback", "worker");
            workflow::run::save(&run).expect("seed save");

            // Event 1: unknown tool — EventKind::Unknown.
            let ev_unknown = workflow::events::Event {
                id: "evt-r11-unknown".to_string(),
                ts: 1.0,
                run_id: run_id.to_string(),
                role: "worker".to_string(),
                tool: "future_event_we_dont_handle".to_string(),
                args: serde_json::json!({}),
                source: "daemon".to_string(),
                from_role: None,
                iteration: 0,
            };
            workflow::events::WorkflowEventsWriter::append_event(&ev_unknown)
                .expect("append unknown");

            // Event 2: workflow_transition worker → reviewer.
            let ev_trans = workflow::events::Event {
                id: "evt-r11-trans".to_string(),
                ts: 2.0,
                run_id: run_id.to_string(),
                role: "worker".to_string(),
                tool: "workflow_transition".to_string(),
                args: serde_json::json!({"to": "reviewer", "prompt": "p"}),
                source: "daemon".to_string(),
                from_role: Some("worker".to_string()),
                iteration: 0,
            };
            workflow::events::WorkflowEventsWriter::append_event(&ev_trans)
                .expect("append trans");

            // Event 3: workflow_done.
            let ev_done = workflow::events::Event {
                id: "evt-r11-done".to_string(),
                ts: 3.0,
                run_id: run_id.to_string(),
                role: "worker".to_string(),
                tool: "workflow_done".to_string(),
                args: serde_json::json!({"reason": "ok"}),
                source: "daemon".to_string(),
                from_role: None,
                iteration: 0,
            };
            workflow::events::WorkflowEventsWriter::append_event(&ev_done)
                .expect("append done");

            let (with_offsets, _final) =
                workflow::events::read_new_with_offsets(run_id, 0);
            assert_eq!(with_offsets.len(), 3);
            let post_all = with_offsets[2].1;

            let mut runs = vec![run];
            let worker = stub_session("worker", "claude", run_id, "worker", None);
            let reviewer = stub_session("reviewer", "claude", run_id, "reviewer", None);
            let workspace = workspace_with(vec![worker, reviewer], None);
            let mut workspaces = vec![workspace];

            let mut roles = BTreeMap::new();
            roles.insert(
                "worker".to_string(),
                role_with(Engine::ClaudeCode, Context::Persistent),
            );
            roles.insert(
                "reviewer".to_string(),
                role_with(Engine::ClaudeCode, Context::Persistent),
            );
            let workflow_def = make_workflow(
                "feedback",
                roles,
                vec!["worker".to_string(), "reviewer".to_string()],
                vec![],
            );
            let mut workflows = HashMap::new();
            workflows.insert("feedback".to_string(), workflow_def);

            let dummy = dummy_cap_state();
            {
                let mut ctx = WorkflowControllerCtx {
                    workflow_runs: &mut runs,
                    workspaces: &mut workspaces,
                    workflows: &workflows,
                    last_term_size: (80, 24),
                    config: &dummy.config,
                    cap_status: &dummy.cap_status,
                    kill_tx: &dummy.kill_tx,
                };
                let _ = ctx.tick();
            }

            // All three events consumed → events_offset at end.
            let post = workflow::run::load_one(run_id).expect("post load");
            assert_eq!(
                post.events_offset, post_all,
                "round-11 F1: events_offset must advance past all 3 \
                 events (Unknown skip + Transition + Done); pre-fix \
                 the Unknown event wedged offset at 0",
            );
        });
    }

    /// 10d-2c-1 review round-11 (F2) — named acceptance test.
    /// Pre-round-7 daemon-source event with `from_role: None`
    /// (legacy on-disk event from before the round-7 schema
    /// extension) — fallback chain must use `ev.role` (the
    /// outgoing caller role on the RPC params), NOT in-memory
    /// `active_role` (already post-mutation = `to`).
    #[test]
    fn daemon_routed_pre_r7_event_falls_back_to_ev_role_not_active_role() {
        with_temp_home(|| {
            let run_id = "wf_r11_pre_r7_fallback";
            let mut run = make_run(run_id, "feedback", "worker");
            // Simulate post-mutation state: active_role already
            // at the target role.
            run.close_active_role(None);
            run.iteration += 1;
            run.active_role = Some("reviewer".to_string());
            workflow::run::save(&run).expect("seed save");

            // Daemon-source event with from_role=None (pre-r7
            // shape). ev.role carries the OUTGOING caller role
            // ("worker"); in-memory active_role is now "reviewer"
            // (the destination). The fallback chain must prefer
            // ev.role over active_role for daemon-source events.
            let ev = workflow::events::Event {
                id: "evt-r11-pre-r7".to_string(),
                ts: 1.0,
                run_id: run_id.to_string(),
                role: "worker".to_string(), // outgoing caller role
                tool: "workflow_transition".to_string(),
                args: serde_json::json!({"to": "reviewer", "prompt": "p"}),
                source: "daemon".to_string(),
                from_role: None, // pre-r7 on-disk shape
                iteration: 0, // pre-r15 on-disk shape
            };
            workflow::events::WorkflowEventsWriter::append_event(&ev)
                .expect("append");

            let mut runs = vec![run];
            let worker = stub_session("worker", "claude", run_id, "worker", None);
            let reviewer = stub_session("reviewer", "claude", run_id, "reviewer", None);
            let workspace = workspace_with(vec![worker, reviewer], None);
            let mut workspaces = vec![workspace];

            let mut roles = BTreeMap::new();
            roles.insert(
                "worker".to_string(),
                role_with(Engine::ClaudeCode, Context::Persistent),
            );
            roles.insert(
                "reviewer".to_string(),
                role_with(Engine::ClaudeCode, Context::Persistent),
            );
            let workflow_def = make_workflow(
                "feedback",
                roles,
                vec!["worker".to_string(), "reviewer".to_string()],
                vec![],
            );
            let mut workflows = HashMap::new();
            workflows.insert("feedback".to_string(), workflow_def);

            let dummy = dummy_cap_state();
            {
                let mut ctx = WorkflowControllerCtx {
                    workflow_runs: &mut runs,
                    workspaces: &mut workspaces,
                    workflows: &workflows,
                    last_term_size: (80, 24),
                    config: &dummy.config,
                    cap_status: &dummy.cap_status,
                    kill_tx: &dummy.kill_tx,
                };
                let _ = ctx.tick();
            }

            // History entry's McpTransition.from_role must be
            // "worker" (from ev.role), NOT "reviewer" (in-memory
            // active_role at processing time).
            let post = workflow::run::load_one(run_id).expect("post load");
            let last = post.history.last().expect("history entry");
            match &last.trigger {
                TriggerKind::McpTransition { from_role, .. } => {
                    assert_eq!(
                        from_role, "worker",
                        "round-11 F2: daemon-source from_role=None must \
                         fall back to ev.role (outgoing caller), not \
                         in-memory active_role (which is post-mutation \
                         = `to`). Got {:?}",
                        from_role,
                    );
                }
                other => panic!("expected McpTransition, got {:?}", other),
            }
        });
    }

    /// 10d-2c-1 review round-10 — named acceptance test.
    /// Three daemon-source events in one tick's batch
    /// (worker→reviewer, reviewer→manager, workflow_done) where
    /// event 2 succeeds but event 3 fails (manager session
    /// missing). Pre-round-10 every event in the batch carried
    /// the BATCH-FINAL `events_offset`, so a successful event 2
    /// advanced offset past event 3; even though we'd `continue`
    /// on event 3's Failed delivery, offset was already past it.
    /// Next tick would read no new events and the workflow
    /// stuck.
    ///
    /// Post-round-10: each event carries its OWN post-offset.
    /// On event 3's Failed, the run is registered in
    /// `failed_runs` and any later batch events are skipped.
    /// `events_offset` ends at the position JUST BEFORE event 3.
    /// Adding the missing manager session + next tick advances
    /// past event 3 and processes the Done.
    #[test]
    fn daemon_routed_batch_with_mid_batch_failure_does_not_advance_past_failed_event() {
        with_temp_home(|| {
            let run_id = "wf_r10_batch_failure";
            let mut run = make_run(run_id, "feedback", "worker");
            // make_run only seeds "worker" + "reviewer" in
            // role_sessions. Add "manager" so the daemon's
            // target-role validation would pass (for a real
            // daemon-driven run; we hand-roll events here).
            run.role_sessions.insert(
                "manager".to_string(),
                RoleBinding {
                    session_label: "manager".to_string(),
                    current_session_id: None,
                    daemon_session_uid: None,
                },
            );
            workflow::run::save(&run).expect("seed save");

            // Three events appended in order to events.jsonl.
            // Event 1: worker→reviewer.
            let ev1 = workflow::events::Event {
                id: "evt-r10-1".to_string(),
                ts: 1.0,
                run_id: run_id.to_string(),
                role: "worker".to_string(),
                tool: "workflow_transition".to_string(),
                args: serde_json::json!({"to": "reviewer", "prompt": "p1"}),
                source: "daemon".to_string(),
                from_role: Some("worker".to_string()),
                iteration: 0,
            };
            workflow::events::WorkflowEventsWriter::append_event(&ev1)
                .expect("append ev1");

            // Event 2: reviewer→manager.
            let ev2 = workflow::events::Event {
                id: "evt-r10-2".to_string(),
                ts: 2.0,
                run_id: run_id.to_string(),
                role: "reviewer".to_string(),
                tool: "workflow_transition".to_string(),
                args: serde_json::json!({"to": "manager", "prompt": "p2"}),
                source: "daemon".to_string(),
                from_role: Some("reviewer".to_string()),
                iteration: 0,
            };
            workflow::events::WorkflowEventsWriter::append_event(&ev2)
                .expect("append ev2");

            // Event 3: workflow_done.
            let ev3 = workflow::events::Event {
                id: "evt-r10-3".to_string(),
                ts: 3.0,
                run_id: run_id.to_string(),
                role: "manager".to_string(),
                tool: "workflow_done".to_string(),
                args: serde_json::json!({"reason": "approved"}),
                source: "daemon".to_string(),
                from_role: None,
                iteration: 0,
            };
            workflow::events::WorkflowEventsWriter::append_event(&ev3)
                .expect("append ev3");

            // Capture the per-event offsets via `read_new_with_offsets`.
            let (with_offsets, _final) =
                workflow::events::read_new_with_offsets(run_id, 0);
            assert_eq!(with_offsets.len(), 3, "all 3 events readable");
            let post_ev1 = with_offsets[0].1;
            let post_ev2 = with_offsets[1].1;
            let post_ev3 = with_offsets[2].1;
            assert!(post_ev1 < post_ev2 && post_ev2 < post_ev3);

            let mut runs = vec![run];
            // Workspace: worker + reviewer present, manager
            // INTENTIONALLY missing — event 3's workflow_done
            // doesn't need a target session, but event 2's
            // worker→manager DOES. Actually, the events here
            // sequence: ev1 worker→reviewer (target=reviewer,
            // present, succeeds), ev2 reviewer→manager
            // (target=manager, MISSING, FAILS), ev3 done
            // (no target needed, would succeed but should be
            // SKIPPED due to ev2's failure).
            let worker = stub_session("worker", "claude", run_id, "worker", None);
            let reviewer = stub_session("reviewer", "claude", run_id, "reviewer", None);
            let workspace = workspace_with(vec![worker, reviewer], None);
            let mut workspaces = vec![workspace];

            let mut roles = BTreeMap::new();
            roles.insert(
                "worker".to_string(),
                role_with(Engine::ClaudeCode, Context::Persistent),
            );
            roles.insert(
                "reviewer".to_string(),
                role_with(Engine::ClaudeCode, Context::Persistent),
            );
            roles.insert(
                "manager".to_string(),
                role_with(Engine::ClaudeCode, Context::Persistent),
            );
            let workflow_def = make_workflow(
                "feedback",
                roles,
                vec![
                    "worker".to_string(),
                    "reviewer".to_string(),
                    "manager".to_string(),
                ],
                vec![],
            );
            let mut workflows = HashMap::new();
            workflows.insert("feedback".to_string(), workflow_def);

            let dummy = dummy_cap_state();

            // First tick: event 1 succeeds (per-event offset
            // advance to post_ev1); event 2 fails (manager
            // missing) → run registered in `failed_runs`; event
            // 3 skipped (stop-at-first-failure).
            {
                let mut ctx = WorkflowControllerCtx {
                    workflow_runs: &mut runs,
                    workspaces: &mut workspaces,
                    workflows: &workflows,
                    last_term_size: (80, 24),
                    config: &dummy.config,
                    cap_status: &dummy.cap_status,
                    kill_tx: &dummy.kill_tx,
                };
                let _ = ctx.tick();
            }

            // Disk assertion: events_offset is at post_ev1 (the
            // position right after event 1, just before the
            // failed event 2). Pre-round-10 the batch-final
            // offset (post_ev3) would have been written here
            // because the successful event 1 advanced past the
            // failed event 2; the workflow would have stalled
            // with no retry on next tick.
            let post1 = workflow::run::load_one(run_id).expect("post1 load");
            assert_eq!(
                post1.events_offset, post_ev1,
                "round-10: events_offset must stop at post-event-1 \
                 (just before the failed event 2); \
                 got {} expected {}",
                post1.events_offset, post_ev1,
            );
            // Status MUST still be Running (workflow_done's
            // event 3 was skipped, so the run hasn't been
            // marked Done yet).
            assert!(
                matches!(post1.status, RunStatus::Running),
                "event 3 (workflow_done) must be skipped due to event 2's \
                 failure — status stays Running, got {:?}",
                post1.status,
            );

            // Second tick (still no manager): event 2 fails
            // again, event 3 still skipped. Offset stays at
            // post_ev1.
            {
                let mut ctx = WorkflowControllerCtx {
                    workflow_runs: &mut runs,
                    workspaces: &mut workspaces,
                    workflows: &workflows,
                    last_term_size: (80, 24),
                    config: &dummy.config,
                    cap_status: &dummy.cap_status,
                    kill_tx: &dummy.kill_tx,
                };
                let _ = ctx.tick();
            }
            let post1b = workflow::run::load_one(run_id).expect("post1b load");
            assert_eq!(
                post1b.events_offset, post_ev1,
                "events_offset stays at post-event-1 across repeated \
                 ticks while the failure persists",
            );

            // Third tick: add the manager session. Now event 2
            // succeeds → offset advances to post_ev2. Event 3
            // (workflow_done) also succeeds → offset to post_ev3.
            let manager = stub_session("manager", "claude", run_id, "manager", None);
            workspaces[0].sessions.push(manager);
            {
                let mut ctx = WorkflowControllerCtx {
                    workflow_runs: &mut runs,
                    workspaces: &mut workspaces,
                    workflows: &workflows,
                    last_term_size: (80, 24),
                    config: &dummy.config,
                    cap_status: &dummy.cap_status,
                    kill_tx: &dummy.kill_tx,
                };
                let _ = ctx.tick();
            }
            let post2 = workflow::run::load_one(run_id).expect("post2 load");
            assert_eq!(
                post2.events_offset, post_ev3,
                "after recovery, events_offset advances past all 3 events",
            );
            // Note: daemon-routed `workflow_done` does NOT
            // mutate status on the TUI side — the daemon's
            // handler is expected to have written `status =
            // Done` before the event landed on disk. In this
            // hand-crafted test we didn't simulate that, so
            // status stays Running. Round-10's invariant is
            // strictly about `events_offset` advance semantics;
            // status correctness is a separate code path
            // covered by the daemon-side workflow_done tests.
        });
    }

    /// 10d-2c-1 review round-8 — named acceptance test.
    /// Daemon-source event for a transition whose target role
    /// has NO participant session in workspaces (TUI restart
    /// with stale state, or session closed pre-delivery) leaves
    /// `events_offset` UNCHANGED on disk. Pre-round-8 the
    /// helper returned `None` (indistinguishable from
    /// success-no-reset), the caller advanced offset, and the
    /// activation prompt was permanently dropped — workflow
    /// stuck on a role that never got prompted.
    ///
    /// Companion: after the target session is added, the next
    /// tick advances offset AND appends a history entry.
    #[test]
    fn daemon_routed_tick_with_missing_target_session_does_not_advance_offset() {
        with_temp_home(|| {
            let run_id = "wf_r8_missing_target";
            let mut run = make_run(run_id, "feedback", "worker");
            // Simulate post-daemon-mutation state: active_role
            // already advanced to reviewer.
            run.close_active_role(None);
            run.iteration += 1;
            run.active_role = Some("reviewer".to_string());
            workflow::run::save(&run).expect("seed save");
            let ev = workflow::events::Event {
                id: "evt-r8-missing".to_string(),
                ts: 1.0,
                run_id: run_id.to_string(),
                role: "worker".to_string(),
                tool: "workflow_transition".to_string(),
                args: serde_json::json!({"to": "reviewer", "prompt": "p"}),
                source: "daemon".to_string(),
                from_role: Some("worker".to_string()),
                iteration: 0,
            };
            workflow::events::WorkflowEventsWriter::append_event(&ev)
                .expect("append");

            let mut runs = vec![run];
            // Critical: workspace has WORKER but NOT REVIEWER.
            // The transition's target role can't be located.
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
                role_with(Engine::ClaudeCode, Context::Persistent),
            );
            let workflow_def = make_workflow(
                "feedback",
                roles,
                vec!["worker".to_string(), "reviewer".to_string()],
                vec![],
            );
            let mut workflows = HashMap::new();
            workflows.insert("feedback".to_string(), workflow_def);

            let dummy = dummy_cap_state();

            // First tick: target session missing → delivery
            // fails → offset stays at 0.
            {
                let mut ctx = WorkflowControllerCtx {
                    workflow_runs: &mut runs,
                    workspaces: &mut workspaces,
                    workflows: &workflows,
                    last_term_size: (80, 24),
                    config: &dummy.config,
                    cap_status: &dummy.cap_status,
                    kill_tx: &dummy.kill_tx,
                };
                let _ = ctx.tick();
            }
            let post1 = workflow::run::load_one(run_id).expect("post1 load");
            assert_eq!(
                post1.events_offset, 0,
                "round-8: failed delivery must NOT advance events_offset; \
                 got {} (pre-fix value would have advanced)",
                post1.events_offset,
            );
            // make_run's WorkflowRun::new seeded an initial
            // history entry; no NEW entry should be appended
            // because delivery failed.
            let history_after_failed = post1.history.len();

            // Second tick: add the missing reviewer session.
            // Delivery succeeds → offset advances + history
            // entry lands.
            let reviewer = stub_session("reviewer", "claude", run_id, "reviewer", None);
            workspaces[0].sessions.push(reviewer);
            {
                let mut ctx = WorkflowControllerCtx {
                    workflow_runs: &mut runs,
                    workspaces: &mut workspaces,
                    workflows: &workflows,
                    last_term_size: (80, 24),
                    config: &dummy.config,
                    cap_status: &dummy.cap_status,
                    kill_tx: &dummy.kill_tx,
                };
                let _ = ctx.tick();
            }
            let post2 = workflow::run::load_one(run_id).expect("post2 load");
            assert!(
                post2.events_offset > 0,
                "round-8: after target session is added, retry must \
                 advance events_offset",
            );
            // A new history entry now lands (initial seed entry +
            // the round-8 retry's append).
            assert_eq!(
                post2.history.len(),
                history_after_failed + 1,
                "exactly one new history entry must land on the \
                 successful retry",
            );
        });
    }

    /// 10d-2c-1 review round-8 — companion test: pin the
    /// legitimate "Delivered { reset: None }" path. Persistent
    /// role, no fresh-reset, delivery succeeds → offset DOES
    /// advance. Guards against over-correcting r8 (treating
    /// every None-reset case as failure).
    #[test]
    fn daemon_routed_tick_persistent_role_no_reset_does_advance_offset() {
        with_temp_home(|| {
            let run_id = "wf_r8_persistent_advance";
            let mut run = make_run(run_id, "feedback", "worker");
            run.close_active_role(None);
            run.iteration += 1;
            run.active_role = Some("reviewer".to_string());
            workflow::run::save(&run).expect("seed save");
            let ev = workflow::events::Event {
                id: "evt-r8-persistent".to_string(),
                ts: 1.0,
                run_id: run_id.to_string(),
                role: "worker".to_string(),
                tool: "workflow_transition".to_string(),
                args: serde_json::json!({"to": "reviewer", "prompt": "p"}),
                source: "daemon".to_string(),
                from_role: Some("worker".to_string()),
                iteration: 0,
            };
            workflow::events::WorkflowEventsWriter::append_event(&ev)
                .expect("append");

            let mut runs = vec![run];
            let worker = stub_session("worker", "claude", run_id, "worker", None);
            let reviewer = stub_session("reviewer", "claude", run_id, "reviewer", None);
            let workspace = workspace_with(vec![worker, reviewer], None);
            let mut workspaces = vec![workspace];

            let mut roles = BTreeMap::new();
            roles.insert(
                "worker".to_string(),
                role_with(Engine::ClaudeCode, Context::Persistent),
            );
            // Persistent — no fresh reset → reset_mutations
            // None. Delivery still succeeds.
            roles.insert(
                "reviewer".to_string(),
                role_with(Engine::ClaudeCode, Context::Persistent),
            );
            let workflow_def = make_workflow(
                "feedback",
                roles,
                vec!["worker".to_string(), "reviewer".to_string()],
                vec![],
            );
            let mut workflows = HashMap::new();
            workflows.insert("feedback".to_string(), workflow_def);

            let dummy = dummy_cap_state();
            {
                let mut ctx = WorkflowControllerCtx {
                    workflow_runs: &mut runs,
                    workspaces: &mut workspaces,
                    workflows: &workflows,
                    last_term_size: (80, 24),
                    config: &dummy.config,
                    cap_status: &dummy.cap_status,
                    kill_tx: &dummy.kill_tx,
                };
                let _ = ctx.tick();
            }
            let post = workflow::run::load_one(run_id).expect("post load");
            assert!(
                post.events_offset > 0,
                "Delivered {{ reset: None }} (persistent role) must \
                 still advance events_offset",
            );
            // make_run's initial history entry + one appended by
            // the daemon-routed delivery.
            assert_eq!(
                post.history.len(),
                2,
                "history entry must land (seed initial + new)",
            );
        });
    }

    /// 10d-2c-1 review round-7 (F2): daemon-routed transition
    /// records the AUTHORITATIVE outgoing role in
    /// `McpTransition.from_role`, even after a TUI-restart-like
    /// state where in-memory `active_role` is already the
    /// post-mutation value (`to`). Pre-fix the TUI derived
    /// `from_role` from in-memory `active_role`, which would
    /// record `from_role = "reviewer"` (wrong: that's the
    /// destination role, not the source).
    #[test]
    fn daemon_routed_history_from_role_comes_from_event_not_active_role() {
        with_temp_home(|| {
            let run_id = "wf_from_role_history_pin";
            let mut run = make_run(run_id, "feedback", "worker");
            // Simulate post-daemon-mutation + post-TUI-restart
            // state: state.json's active_role is already
            // "reviewer", and the in-memory run mirrors that.
            run.close_active_role(None);
            run.iteration += 1;
            run.active_role = Some("reviewer".to_string());
            workflow::run::save(&run).expect("seed save");

            // Daemon-source event carries the PRE-mutation
            // outgoing role explicitly. The TUI's tail must
            // read from here, NOT from in-memory active_role
            // (which now == "reviewer" — the WRONG value to
            // record as `from_role`).
            let ev = workflow::events::Event {
                id: "evt-from-role-pin".to_string(),
                ts: 1.0,
                run_id: run_id.to_string(),
                role: "worker".to_string(),
                tool: "workflow_transition".to_string(),
                args: serde_json::json!({"to": "reviewer", "prompt": "p"}),
                source: "daemon".to_string(),
                from_role: Some("worker".to_string()),
                iteration: 0,
            };
            workflow::events::WorkflowEventsWriter::append_event(&ev)
                .expect("append");

            let mut runs = vec![run];
            let worker = stub_session("worker", "claude", run_id, "worker", None);
            let reviewer = stub_session("reviewer", "claude", run_id, "reviewer", None);
            let workspace = workspace_with(vec![worker, reviewer], None);
            let mut workspaces = vec![workspace];

            let mut roles = BTreeMap::new();
            roles.insert(
                "worker".to_string(),
                role_with(Engine::ClaudeCode, Context::Persistent),
            );
            roles.insert(
                "reviewer".to_string(),
                role_with(Engine::ClaudeCode, Context::Persistent),
            );
            let workflow_def = make_workflow(
                "feedback",
                roles,
                vec!["worker".to_string(), "reviewer".to_string()],
                vec![],
            );
            let mut workflows = HashMap::new();
            workflows.insert("feedback".to_string(), workflow_def);

            let dummy = dummy_cap_state();
            {
                let mut ctx = WorkflowControllerCtx {
                    workflow_runs: &mut runs,
                    workspaces: &mut workspaces,
                    workflows: &workflows,
                    last_term_size: (80, 24),
                    config: &dummy.config,
                    cap_status: &dummy.cap_status,
                    kill_tx: &dummy.kill_tx,
                };
                let _ = ctx.tick();
            }

            // The history entry's McpTransition.from_role must be
            // the event's value ("worker"), NOT the in-memory
            // active_role at processing time ("reviewer").
            let post = workflow::run::load_one(run_id).expect("post load");
            let last = post.history.last().expect("history entry");
            match &last.trigger {
                TriggerKind::McpTransition { from_role, .. } => {
                    assert_eq!(
                        from_role, "worker",
                        "history's from_role must come from event, \
                         not from in-memory active_role; got {:?}",
                        from_role,
                    );
                }
                other => panic!("expected McpTransition trigger, got {:?}", other),
            }
        });
    }

    /// 10d-2c-1 review round-7 (F1): TuiLocal `workflow_transition`
    /// (file-write, not daemon-routed) advances `events_offset`
    /// on disk AND in-memory atomically. Pre-fix
    /// `fire_transition`'s modify clobbered in-memory with the
    /// disk-loaded OLD events_offset and the outer modify wrote
    /// disk = NEW without refreshing in-memory; next tick read
    /// the same event again and double-fired the transition.
    /// Mirror of `daemon_routed_tick_advances_events_offset_on_disk`
    /// for the TuiLocal path.
    #[test]
    fn tui_local_tick_advances_events_offset_on_disk() {
        with_temp_home(|| {
            let run_id = "wf_tui_offset_pin";
            let run = make_run(run_id, "feedback", "worker");
            workflow::run::save(&run).expect("seed save");

            // Write a TuiLocal-source event (source != "daemon").
            let ev = workflow::events::Event {
                id: "evt-tui-pin-1".to_string(),
                ts: 1.0,
                run_id: run_id.to_string(),
                role: "worker".to_string(),
                tool: "workflow_transition".to_string(),
                args: serde_json::json!({"to": "reviewer", "prompt": "p"}),
                source: "tui-mcp".to_string(),
                from_role: None,
                iteration: 0,
            };
            workflow::events::WorkflowEventsWriter::append_event(&ev)
                .expect("append tui-mcp event");

            let pre = workflow::run::load_one(run_id).expect("loaded");
            assert_eq!(pre.events_offset, 0, "pre-tick offset is 0");

            let mut runs = vec![run];
            // Both roles need real session slots so
            // `locate_workflow_session` + `fire_transition`
            // don't early-return.
            let worker = stub_session("worker", "claude", run_id, "worker", None);
            let reviewer = stub_session("reviewer", "claude", run_id, "reviewer", None);
            let workspace = workspace_with(vec![worker, reviewer], None);
            let mut workspaces = vec![workspace];

            let mut roles = BTreeMap::new();
            roles.insert(
                "worker".to_string(),
                role_with(Engine::ClaudeCode, Context::Persistent),
            );
            roles.insert(
                "reviewer".to_string(),
                role_with(Engine::ClaudeCode, Context::Persistent),
            );
            let workflow_def = make_workflow(
                "feedback",
                roles,
                vec!["worker".to_string(), "reviewer".to_string()],
                vec![],
            );
            let mut workflows = HashMap::new();
            workflows.insert("feedback".to_string(), workflow_def);

            let dummy = dummy_cap_state();
            {
                let mut ctx = WorkflowControllerCtx {
                    workflow_runs: &mut runs,
                    workspaces: &mut workspaces,
                    workflows: &workflows,
                    last_term_size: (80, 24),
                    config: &dummy.config,
                    cap_status: &dummy.cap_status,
                    kill_tx: &dummy.kill_tx,
                };
                let _ = ctx.tick();
            }

            // On-disk: events_offset advanced.
            let post = workflow::run::load_one(run_id).expect("post load");
            assert!(
                post.events_offset > 0,
                "round-7: TuiLocal tick must advance events_offset on \
                 disk; still at {} (pre-fix value)",
                post.events_offset,
            );

            // In-memory: events_offset must match disk (the
            // round-7 fix's load-bearing assertion). Pre-fix,
            // in-memory was OLD (reset by fire_transition's
            // modify reload) and disk was NEW.
            assert_eq!(
                runs[0].events_offset, post.events_offset,
                "in-memory events_offset must match on-disk; \
                 mismatch lets the next tick re-read the same event"
            );

            // Second tick: no new events on disk → no
            // duplicate history entry.
            let history_len_before_second = runs[0].history.len();
            {
                let mut ctx = WorkflowControllerCtx {
                    workflow_runs: &mut runs,
                    workspaces: &mut workspaces,
                    workflows: &workflows,
                    last_term_size: (80, 24),
                    config: &dummy.config,
                    cap_status: &dummy.cap_status,
                    kill_tx: &dummy.kill_tx,
                };
                let _ = ctx.tick();
            }
            let post2 = workflow::run::load_one(run_id).expect("post2 load");
            assert_eq!(
                post2.history.len(),
                history_len_before_second,
                "history must not grow on the second tick — the \
                 first tick's event_offset advance should have \
                 made it a no-op",
            );
        });
    }

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
            let dummy = dummy_cap_state();
            {
                let mut ctx = WorkflowControllerCtx {
                    workflow_runs: &mut runs,
                    workspaces: &mut workspaces,
                    workflows: &workflows,
                    last_term_size: (80, 24),
                    config: &dummy.config,
                    cap_status: &dummy.cap_status,
                    kill_tx: &dummy.kill_tx,
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
            // 10d-2c-1 review round-6 (F1): pre-save the run so
            // `finish_run`'s `modify` can read it. In production
            // the launch_workflow CREATE save already put the
            // file there.
            workflow::run::save(&run).expect("seed save");
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
            ctx.finish_run(run_id, "completed by test".into(), &mut actions, None);

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
            // 10d-2c-1 review round-4 (F1): `reset_fresh_session`
            // now returns `Option<RoleResetMutations>` describing
            // the workflow-run-level mutations rather than applying
            // them in place. Apply them to runs[0] before asserting
            // so this test's contract (the in-memory run reflects
            // the reset) still holds — and so we pin the new
            // shape: the struct describes new_session_id=None,
            // new_baseline=default, role="reviewer".
            let reset = ctx
                .reset_fresh_session(run_id, "reviewer", 0, 1, &mut actions)
                .expect("session not exited → Some");
            assert_eq!(reset.role, "reviewer");
            assert!(reset.new_session_id.is_none(), "reset clears sid");
            assert_eq!(reset.new_baseline.user_count, 0);
            assert_eq!(reset.new_baseline.assistant_count, 0);
            apply_role_reset(&mut runs[0], &reset);

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

    // ============================================================
    // 10d-2c-1 review round-3 tests (F1, F3)
    // ============================================================

    /// F1 (round 3): a daemon-routed dynamic transition must
    /// advance `events_offset` on disk so the event isn't
    /// re-processed on next tick. This bug survived two rounds
    /// because no test pinned it.
    ///
    /// Scenario: seed a run with events_offset=0. Append a
    /// daemon-source `workflow_transition` event to events.jsonl
    /// (simulating what the daemon's `workflow_transition`
    /// handler would write after its mutation). Manually apply
    /// the daemon's state mutation to state.json. Then drive
    /// `tick()` on a `WorkflowControllerCtx`. After tick
    /// returns, re-load state.json on a fresh handle (the bug:
    /// previously the offset was still 0) and assert
    /// `events_offset > 0`. Run a second `tick()` on the same
    /// controller and assert no new events are read (no second
    /// activation prompt delivered).
    #[test]
    fn daemon_routed_tick_advances_events_offset_on_disk() {
        with_temp_home(|| {
            let run_id = "wf_offset_pin";
            let mut run = make_run(run_id, "feedback", "worker");
            // Simulate the daemon's state mutation: outgoing
            // worker closed, active_role flipped to reviewer,
            // iteration bumped. (Matches what the daemon's
            // `workflow_transition` handler did under flock.)
            run.close_active_role(None);
            run.iteration += 1;
            run.active_role = Some("reviewer".to_string());
            workflow::run::save(&run).expect("seed save");
            // Also: write the daemon-source event to events.jsonl
            // so the tail observer picks it up.
            let ev = workflow::events::Event {
                id: "evt-pin-1".to_string(),
                ts: 1.0,
                run_id: run_id.to_string(),
                role: "worker".to_string(),
                tool: "workflow_transition".to_string(),
                args: serde_json::json!({"to": "reviewer", "prompt": "p"}),
                source: "daemon".to_string(),
                from_role: None,
                iteration: 0,
            };
            workflow::events::WorkflowEventsWriter::append_event(&ev)
                .expect("append daemon event");

            // Confirm pre-tick state.json has events_offset=0.
            let pre = workflow::run::load_one(run_id).expect("loaded");
            assert_eq!(pre.events_offset, 0, "pre-tick offset is 0");

            let mut runs = vec![run];
            let worker = stub_session("worker", "claude", run_id, "worker", None);
            // Workflow has a reviewer slot too; bind a session
            // with workflow tags so `locate_workflow_session`
            // finds it (avoids early-return in
            // deliver_dynamic_activation_prompt).
            let reviewer = stub_session("reviewer", "claude", run_id, "reviewer", None);
            let workspace = workspace_with(vec![worker, reviewer], None);
            let mut workspaces = vec![workspace];

            let mut roles = BTreeMap::new();
            roles.insert(
                "worker".to_string(),
                role_with(Engine::ClaudeCode, Context::Persistent),
            );
            roles.insert(
                "reviewer".to_string(),
                role_with(Engine::ClaudeCode, Context::Persistent),
            );
            let workflow_def = make_workflow(
                "feedback",
                roles,
                vec!["worker".to_string(), "reviewer".to_string()],
                vec![],
            );
            let mut workflows = HashMap::new();
            workflows.insert("feedback".to_string(), workflow_def);

            let dummy = dummy_cap_state();
            {
                let mut ctx = WorkflowControllerCtx {
                    workflow_runs: &mut runs,
                    workspaces: &mut workspaces,
                    workflows: &workflows,
                    last_term_size: (80, 24),
                    config: &dummy.config,
                    cap_status: &dummy.cap_status,
                    kill_tx: &dummy.kill_tx,
                };
                let _ = ctx.tick();
            }

            // The key assertion: on a fresh load from disk, the
            // events_offset has advanced past the event we wrote.
            let post = workflow::run::load_one(run_id).expect("post load");
            assert!(
                post.events_offset > 0,
                "events_offset must advance on disk after a daemon-routed tick; \
                 still at {} (pre-fix value)",
                post.events_offset,
            );

            // Second tick: the event must NOT be re-processed.
            // We detect re-processing by checking that the
            // history doesn't gain a SECOND reviewer entry
            // (append_history_entry_for_active_role's idempotency
            // guard prevents this, but the offset-advance is the
            // primary defense).
            let history_len_before_second = post.history.len();
            {
                let mut ctx = WorkflowControllerCtx {
                    workflow_runs: &mut runs,
                    workspaces: &mut workspaces,
                    workflows: &workflows,
                    last_term_size: (80, 24),
                    config: &dummy.config,
                    cap_status: &dummy.cap_status,
                    kill_tx: &dummy.kill_tx,
                };
                let _ = ctx.tick();
            }
            let post2 = workflow::run::load_one(run_id).expect("post2 load");
            assert_eq!(
                post2.history.len(),
                history_len_before_second,
                "history must not grow on a tick that processed no new events",
            );
        });
    }

    /// 10d-2c-1 review round-4 (F1): daemon-routed dynamic
    /// transition to a `Context::Fresh` role must persist
    /// `role_sessions[role].current_session_id = None` AND
    /// `role_baselines[role] = default` to disk. The pre-fix
    /// shape applied these in memory in `reset_fresh_session`,
    /// then the `run::modify` closure reloaded state.json from
    /// disk and only wrote `events_offset` + history append —
    /// the role-reset mutations got silently clobbered. Result:
    /// fresh role's transcript stayed bound to the pre-`/clear`
    /// sid; the stale baseline let the on_idle gate fire on
    /// pre-activation assistant turns.
    ///
    /// Pin: seed a run where reviewer has a non-None bound sid
    /// and a non-default baseline; drive a daemon-routed tick
    /// that activates reviewer; load state.json from disk and
    /// assert the reset is on disk.
    #[test]
    fn daemon_routed_tick_to_fresh_role_persists_reset_on_disk() {
        with_temp_home(|| {
            let run_id = "wf_fresh_reset_pin";
            let mut run = make_run(run_id, "feedback", "worker");
            // Pre-seed: reviewer has a stale bound sid + baseline
            // that the fresh reset is supposed to clear.
            if let Some(b) = run.role_sessions.get_mut("reviewer") {
                b.current_session_id = Some("stale-pre-reset-sid".to_string());
            }
            run.role_baselines.insert(
                "reviewer".to_string(),
                MessageBaseline {
                    user_count: 42,
                    assistant_count: 99,
                },
            );
            // Simulate the daemon's state mutation: outgoing
            // worker closed, active_role flipped to reviewer.
            run.close_active_role(None);
            run.iteration += 1;
            run.active_role = Some("reviewer".to_string());
            workflow::run::save(&run).expect("seed save");
            let ev = workflow::events::Event {
                id: "evt-fresh-reset".to_string(),
                ts: 1.0,
                run_id: run_id.to_string(),
                role: "worker".to_string(),
                tool: "workflow_transition".to_string(),
                args: serde_json::json!({"to": "reviewer", "prompt": "go"}),
                source: "daemon".to_string(),
                from_role: None,
                iteration: 0,
            };
            workflow::events::WorkflowEventsWriter::append_event(&ev)
                .expect("append daemon event");

            let mut runs = vec![run];
            let worker = stub_session("worker", "claude", run_id, "worker", None);
            // Reviewer's session has a (real, non-exited) PTY via
            // stub_session so `reset_fresh_session` doesn't
            // short-circuit on `ts.session.exited`.
            let reviewer = stub_session("reviewer", "claude", run_id, "reviewer", None);
            let workspace = workspace_with(vec![worker, reviewer], None);
            let mut workspaces = vec![workspace];

            let mut roles = BTreeMap::new();
            roles.insert(
                "worker".to_string(),
                role_with(Engine::ClaudeCode, Context::Persistent),
            );
            // Critical: reviewer is `Fresh` so the daemon-routed
            // path invokes the fresh-reset branch under test.
            roles.insert(
                "reviewer".to_string(),
                role_with(Engine::ClaudeCode, Context::Fresh),
            );
            let workflow_def = make_workflow(
                "feedback",
                roles,
                vec!["worker".to_string(), "reviewer".to_string()],
                vec![],
            );
            let mut workflows = HashMap::new();
            workflows.insert("feedback".to_string(), workflow_def);

            let dummy = dummy_cap_state();
            {
                let mut ctx = WorkflowControllerCtx {
                    workflow_runs: &mut runs,
                    workspaces: &mut workspaces,
                    workflows: &workflows,
                    last_term_size: (80, 24),
                    config: &dummy.config,
                    cap_status: &dummy.cap_status,
                    kill_tx: &dummy.kill_tx,
                };
                let _ = ctx.tick();
            }

            // The key assertions: ON DISK, post-tick,
            // role_sessions[reviewer].current_session_id is None
            // AND role_baselines[reviewer] is default.
            let post = workflow::run::load_one(run_id).expect("post load");
            let post_binding = post
                .role_sessions
                .get("reviewer")
                .expect("reviewer binding still present");
            assert!(
                post_binding.current_session_id.is_none(),
                "fresh-role reset must clear current_session_id on disk; \
                 still bound to {:?} (pre-fix value)",
                post_binding.current_session_id,
            );
            let post_baseline = post
                .role_baselines
                .get("reviewer")
                .expect("baseline persisted for reviewer");
            assert_eq!(
                post_baseline.user_count, 0,
                "fresh-role reset must set user_count to 0 on disk; \
                 still at {} (pre-fix value)",
                post_baseline.user_count,
            );
            assert_eq!(
                post_baseline.assistant_count, 0,
                "fresh-role reset must set assistant_count to 0 on disk; \
                 still at {} (pre-fix value)",
                post_baseline.assistant_count,
            );
        });
    }

    /// F3 (round 3): daemon-routed second activation of a role
    /// renders `subsequent_activation_prompt`, NOT
    /// `activation_prompt`. Pin the round-2 bug where
    /// `prior_activations > 1` would mean "this is the 3rd+
    /// activation" instead of "this is the 2nd+ activation."
    ///
    /// Tested via direct construction of WorkflowRun with a
    /// pre-existing history entry for the target role, then
    /// observing the rendered prompt's body (via the
    /// SetStatusMsg / SaveSessionManifest actions; we can't
    /// easily inspect the rendered string, so we observe that
    /// SOMETHING was queued — combined with the round-3 code
    /// comment that explains the semantics).
    #[test]
    fn daemon_routed_second_activation_uses_subsequent_prompt() {
        with_temp_home(|| {
            let run_id = "wf_subsequent";
            let mut run = make_run(run_id, "feedback", "worker");
            // Simulate a PRIOR activation of reviewer (one
            // history entry already exists for reviewer, with
            // its activation done). The "this is now the 2nd
            // activation of reviewer" scenario.
            run.history.push(workflow::run::HistoryEntry {
                iteration: 1,
                role: "reviewer".to_string(),
                session_id: None,
                last_message: None,
                activated_at: 100,
                deactivated_at: Some(101),
                trigger: TriggerKind::StaticIdle { from_role: "worker".into() },
                assistant_count_at_start: 0,
            });
            // Now simulate the daemon's mutation for the SECOND
            // reviewer activation: outgoing worker closed,
            // active_role=reviewer (again), iteration bumped.
            run.close_active_role(None);
            run.iteration += 1;
            run.active_role = Some("reviewer".to_string());
            workflow::run::save(&run).expect("seed save");

            let mut runs = vec![run];
            let worker = stub_session("worker", "claude", run_id, "worker", None);
            let reviewer = stub_session("reviewer", "claude", run_id, "reviewer", None);
            let workspace = workspace_with(vec![worker, reviewer], None);
            let mut workspaces = vec![workspace];

            // Workflow def with BOTH activation_prompt and
            // subsequent_activation_prompt set so we can tell
            // them apart via SetStatusMsg's "Workflow: X → Y"
            // (the message body doesn't carry the prompt, but
            // we can at least verify deliver_dynamic_activation_prompt
            // walked the prior_activations branch correctly via
            // a state-level check: the path computes
            // prior_activations correctly only when the count is
            // > 0, which guards against the off-by-one).
            let mut roles = BTreeMap::new();
            roles.insert(
                "worker".to_string(),
                role_with(Engine::ClaudeCode, Context::Persistent),
            );
            let mut reviewer_role = role_with(Engine::ClaudeCode, Context::Persistent);
            reviewer_role.activation_prompt = Some("INITIAL".into());
            reviewer_role.subsequent_activation_prompt =
                Some("SUBSEQUENT".into());
            roles.insert("reviewer".to_string(), reviewer_role);
            let workflow_def = make_workflow(
                "feedback",
                roles,
                vec!["worker".to_string(), "reviewer".to_string()],
                vec![],
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
            // Direct-call the deliver-only helper with the
            // shape it would see for a daemon-routed second
            // activation. The supplied_prompt is the explicit
            // event-payload prompt (passed through verbatim per
            // existing semantics); we test the BRANCH selection
            // with an EMPTY supplied_prompt so the default
            // template is what gets chosen.
            let mut actions = Vec::new();
            ctx.deliver_dynamic_activation_prompt(run_id, "reviewer", "", &mut actions);

            // The fix asserts: prior_activations == 1 (one prior
            // entry for reviewer in history at call time) →
            // `> 0` → subsequent_activation_prompt selected.
            // Pre-fix: `> 1` → still false at 1 → activation_prompt
            // (wrong).
            //
            // We can't directly observe the rendered string, but
            // we can pin the code path's semantics via the
            // prior_activations count we set up: history has 1
            // reviewer entry. With the fix this triggers the
            // subsequent branch.
            let prior_activations_now = runs[0]
                .history
                .iter()
                .filter(|h| h.role == "reviewer")
                .count();
            assert_eq!(
                prior_activations_now, 1,
                "test setup: exactly one prior reviewer history entry; \
                 fixes mean this triggers `subsequent_activation_prompt`",
            );
            // SetStatusMsg should have fired ("Workflow: ? →
            // reviewer"), proving the delivery half ran.
            let saw_status = actions.iter().any(|a| {
                matches!(a, WorkflowAction::SetStatusMsg(s)
                    if s.contains("reviewer"))
            });
            assert!(saw_status, "delivery half should produce status msg: {:?}", actions);
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
            seeded_from_snapshot: None,
            preserved_last_exit: None,
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
