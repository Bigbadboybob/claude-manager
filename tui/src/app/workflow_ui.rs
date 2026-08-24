//! Workflow orchestration UI: launch/pause/stop flows, history rotation bindings, participant respawn.

use super::*;

pub(super) fn note_workflow_transcript_binding(
    runs: &mut [WorkflowRun],
    run_id: &str,
    role: &str,
    old_sid: Option<&str>,
    new_sid: &str,
) -> bool {
    let Some(run) = runs.iter_mut().find(|run| run.run_id == run_id) else {
        return false;
    };

    let mut changed = false;
    if let Some(binding) = run.role_sessions.get_mut(role) {
        if binding.current_session_id.as_deref() != Some(new_sid) {
            binding.current_session_id = Some(new_sid.to_string());
            changed = true;
        }
    }

    let rebound = old_sid.is_some() && old_sid != Some(new_sid);
    if rebound {
        run.role_baselines
            .insert(role.to_string(), workflow::run::MessageBaseline::default());
        changed = true;
    }

    if run.active_role.as_deref() == Some(role) {
        if let Some(entry) = run.history.last_mut() {
            if entry.role == role {
                if entry.session_id.as_deref() != Some(new_sid) {
                    entry.session_id = Some(new_sid.to_string());
                    changed = true;
                }
                if rebound && entry.assistant_count_at_start != 0 {
                    entry.assistant_count_at_start = 0;
                    changed = true;
                }
            }
        }
    }

    changed
}

/// 10d-2c-1 review round-6 (F1): apply the TUI-owned mutations
/// for a `/clear`-or-`/compact`-driven history rotation to a
/// `WorkflowRun`. Used by `apply_history_rotation` to keep the
/// in-memory + on-disk shape in lockstep — same field-level
/// updates applied via `workflow::run::modify` (so concurrent
/// daemon writes to active_role / iteration / status survive)
/// AND, if/when needed, against the in-memory slot via
/// `slice::from_mut`.
///
/// Fields touched (TUI-owned):
///   - `role_sessions[role].current_session_id` ← new sid.
///   - `role_baselines[role]` ← `MessageBaseline::default()`.
///   - Active role's last history entry, if it's for `role`:
///     `assistant_count_at_start = 0`, `session_id = Some(new_sid)`.
fn apply_history_rotation_to_run(run: &mut WorkflowRun, role: &str, new_sid: &str) {
    if let Some(b) = run.role_sessions.get_mut(role) {
        b.current_session_id = Some(new_sid.to_string());
    }
    run.role_baselines
        .insert(role.to_string(), workflow::run::MessageBaseline::default());
    if run.active_role.as_deref() == Some(role) {
        if let Some(h) = run.history.last_mut() {
            h.assistant_count_at_start = 0;
            h.session_id = Some(new_sid.to_string());
        }
    }
}

/// Map a TerminalSession's `session_type` ("claude" | "codex" | "bash") to the
/// `Engine` used by transcript readers. Workflow code must derive engine from
/// the actual bound session (not from the workflow TOML role spec) since the
/// user can bind, e.g., a codex session into a role declared as "claude-code".
/// Per-session info needed to resolve a `/clear` or `/compact` rotation
/// detected via `~/.claude/history.jsonl`. Built fresh each tick from
/// the current workspace state.
pub(crate) struct RotationBinding {
    pub wi: usize,
    pub si: usize,
    /// `(run_id, role)` when the session is a workflow participant,
    /// `None` otherwise (regular `A-n` / planning panes). The fix was
    /// to also include non-workflow sessions — they need rotation
    /// rebinds too so `read_session_output` doesn't stall on the
    /// pre-rotation transcript file.
    pub workflow: Option<(String, String)>,
    pub worktree: std::path::PathBuf,
}

/// Build the (sid → RotationBinding) map for every live Claude session.
/// Extracted from the rotation-resolve loop in `drain_terminal_events`
/// so it can be tested without spinning up an App. Skips sessions
/// without a `transcript_id` (nothing to rebind), without a worktree
/// (no project dir to scan), or whose `session_type` isn't `"claude"`
/// (Codex doesn't go through the history.jsonl rotation path).
pub(crate) fn collect_rotation_bindings(
    workspaces: &[Workspace],
) -> HashMap<String, RotationBinding> {
    let mut out: HashMap<String, RotationBinding> = HashMap::new();
    for (wi, ws) in workspaces.iter().enumerate() {
        let Some(wt) = ws.worktree_path.clone() else {
            continue;
        };
        for (si, ts) in ws.sessions.iter().enumerate() {
            if ts.session_type != "claude" {
                continue;
            }
            let Some(sid) = &ts.transcript_id else {
                continue;
            };
            let workflow = match (
                ts.workflow_run_id.as_deref(),
                ts.workflow_role.as_deref(),
            ) {
                (Some(rid), Some(role)) => Some((rid.to_string(), role.to_string())),
                _ => None,
            };
            out.insert(
                sid.clone(),
                RotationBinding {
                    wi,
                    si,
                    workflow,
                    worktree: wt.clone(),
                },
            );
        }
    }
    out
}

/// Kill the agent process inside `ts` and respawn it under the same PTY slot
/// with workflow MCP config + resume args, so the role can call
/// `workflow_transition` / `workflow_done`. The user-launched session was
/// started without `--mcp-config`; this is what gives it the workflow tools.
///
/// `session_id` is optional: when present, the new agent is started with
/// `--resume <sid>` and its transcript binding is preserved. When absent
/// (e.g. an idle pane that hasn't produced a transcript yet), we still
/// respawn so the new agent picks up the workflow env — it just starts
/// fresh, and the watcher binds whatever sid the new agent writes. Without
/// this respawn the agent's MCP subprocess never sees `CM_WORKFLOW_RUN_ID`,
/// and every `workflow_transition` / `workflow_done` / `workflow_reject_finding`
/// call from this role fails for the lifetime of the run.
///
/// Returns `None` on success, `Some(message)` on any failure (caller decides
/// whether to surface to the status bar). On failure the existing `Session`
/// is left untouched — we only swap if `Session::new` succeeds.
pub(crate) fn respawn_existing_with_workflow_mcp(
    ts: &mut TerminalSession,
    engine: &workflow::toml_schema::Engine,
    run_id: &str,
    role: &str,
    session_id: Option<&str>,
    worktree: Option<&Path>,
    cols: u16,
    rows: u16,
    config: &Config,
    cap_status: &crate::memory_cap::MemoryCapAvailability,
    host_pool: &crate::host_pool::HostPool,
    workspace_id: &str,
) -> Option<String> {
    // 12e-r6 F2: fail fast on a non-local pinned host BEFORE
    // any other work. The respawn would build local mcp_config
    // paths + run `spawn_agent_session` (local PTY); the
    // resulting Session would be local but get swapped in
    // over a TerminalSession whose `host_id` points at a
    // remote host. That mistag breaks every downstream
    // per-session RPC route. Order matters: this guard runs
    // BEFORE `kill_daemon_session_if_attached` so we don't
    // tear down the original session for no reason.
    if let Err(e) = guard_local_host_only(&ts.host_id, "workflow respawn") {
        return Some(format!(
            "Skipping respawn of {}: {}",
            role, e
        ));
    }
    let wt = match worktree {
        Some(wt) => wt,
        None => {
            return Some(format!(
                "Skipping reload of {}: worktree unknown — workflow MCP tools unavailable",
                role
            ));
        }
    };
    // Snapshot pre-spawn transcript files so the watcher can identify the
    // new one. For Codex this matters even with `--resume` because resume
    // writes a fresh rollout id; for Claude it's only needed when we're
    // spawning without `--resume` (no sid yet). Cheap to compute either
    // way.
    let pre_spawn_files = match engine {
        workflow::toml_schema::Engine::ClaudeCode => App::list_jsonl_files(wt),
        workflow::toml_schema::Engine::Codex => App::list_codex_sessions(wt),
    };
    // migrate-tui-local: route workflow respawn through the
    // daemon RPC so the replacement participant lands in
    // state.sessions (workflow context known at spawn time via
    // workflow_run_id/workflow_role). The kill_daemon_session_if_attached
    // call below fires FIRST so any existing daemon-side child
    // is reaped before we start the replacement.
    let session_type_str = engine.as_session_type();
    // Mint a FRESH uid for the replacement session rather than
    // reusing `ts.uid`. `kill_session` (daemon) only SIGKILLs and
    // defers registry removal to the reaper's on-exit callback, so
    // the uid stays in `state.sessions` for a beat after the kill
    // RPC returns. An immediate `start_session` reusing that same
    // uid loses the race and hits the daemon's collision guard
    // (`Conflict`, methods.rs start_session) — the spawn fails, the
    // swap below never runs, and `ts.session` is left holding the
    // just-killed Session. The foreground then renders a dead PTY
    // and drops every keystroke (frozen pane). A new uid sidesteps
    // the kill-then-reuse race entirely. `ts.uid` is updated to
    // match after the successful swap (see below).
    let new_uid = new_session_uid();
    App::kill_daemon_session_if_attached(host_pool, ts);
    // migrate-tui-local Issue 3: workflow respawn with a known
    // transcript_id (the role's current session_id) lets us
    // hand the deterministic claude path to the daemon. For
    // codex resumes a fresh rollout id is written post-spawn,
    // so pre_spawn_transcript_path returns None and the post-
    // spawn detector continues to handle that case.
    let pre_spawn_transcript = session_id.and_then(|sid| {
        pre_spawn_transcript_path(session_type_str, wt, sid)
    });
    let new_sess = match try_spawn_via_daemon_with_deps(
        host_pool,
        config,
        cap_status,
        &new_uid,
        workspace_id,
        wt,
        session_type_str,
        role,
        session_id,
        cols,
        rows,
        ts.task_id.as_deref(),
        Some(run_id),
        Some(role),
        &ts.host_id,
        pre_spawn_transcript.as_deref(),
        false, // global_perms
    ) {
        Some(Ok(s)) => s,
        Some(Err(e)) => {
            return Some(format!(
                "Reload failed for {}: {} — workflow MCP tools unavailable",
                role, e
            ));
        }
        None => {
            return Some(format!(
                "Reload failed for {}: try_spawn_via_daemon returned None for \
                 daemon-eligible engine {:?}",
                role, engine,
            ));
        }
    };
    // Swap the Session. migrate-tui-local: the explicit kill of
    // the prior daemon-side child happened up-front (before
    // try_spawn_via_daemon), so there's no detached-Drop window
    // here. The structural pin
    // (respawn_calls_kill_daemon_before_session_swap) still
    // matches because the kill call precedes this assignment.
    ts.session = new_sess;
    // Adopt the replacement session's fresh uid as this
    // TerminalSession's identity. The new daemon session is
    // registered under `new_uid` (the returned Session's
    // `daemon_session_uid` equals it), and the MCP config baked
    // into the spawn sets `CM_TUI_SESSION_ID = new_uid`. The
    // descendant-scope auth check compares `ts.uid == caller_uid`
    // (the agent's CM_TUI_SESSION_ID), so without this update the
    // respawned agent's `workflow_transition` / `workflow_done`
    // calls would fail auth. The workflow RoleBinding captures
    // `ts.session.daemon_session_uid` after this respawn returns,
    // so it picks up the new uid as well.
    ts.uid = new_uid;
    if session_id.is_some() {
        ts.transcript_id = session_id.map(|s| s.to_string());
        ts.pending_jsonl_files = match engine {
            workflow::toml_schema::Engine::ClaudeCode => None,
            workflow::toml_schema::Engine::Codex => Some(pre_spawn_files),
        };
    } else {
        ts.transcript_id = None;
        ts.pending_jsonl_files = Some(pre_spawn_files);
    }
    ts.pending_prompt = None;
    ts.pending_clear = None;
    ts.pending_enter = None;
    ts.last_delivery = None;
    ts.set_status(SessionStatus::Idle);
    // Kick a resize so the freshly-resumed agent (e.g. `claude
    // --resume`) repaints right away. The new Session's terminal
    // grid starts blank; the Resize msg drives a SIGWINCH to the
    // daemon PTY, prompting an immediate redraw instead of leaving
    // the pane blank until the agent's next spontaneous output.
    ts.session.resize(cols, rows);
    None
}

/// 10d-3 r1 F2: untag predicate used by `restore_sessions` before
/// the spawn loop. Returns `Some(cleaned_entry)` if the entry's
/// `workflow_run_id` is set AND not in `active_run_ids` — i.e. a
/// stale tag from a Detached/Done run that the manifest never got
/// to clean up. Returns `None` when the entry's tag is already
/// absent or still active; callers should pass the original
/// reference through to `spawn_restored_session` in that case.
///
/// Extracted as a free function so the test below pins the
/// predicate without constructing a full `App`. The behavior is
/// load-bearing — pre-r2 the untag ran AFTER spawn, so the spawned
/// agent inherited stale `CM_WORKFLOW_RUN_ID` / `CM_ROLE` env vars
/// pointing at a now-Detached run.
pub(crate) fn untag_stale_workflow(
    entry: &ManifestEntry,
    active_run_ids: &std::collections::HashSet<String>,
) -> Option<ManifestEntry> {
    match entry.workflow_run_id.as_deref() {
        Some(rid) if !active_run_ids.contains(rid) => {
            let mut e = entry.clone();
            e.workflow_run_id = None;
            e.workflow_role = None;
            e.hidden = false;
            Some(e)
        }
        _ => None,
    }
}

/// Title for the workflow picker dialog. Includes a count when any workflow
/// files failed to load, so the user has a hint that some entries are missing
/// from the picker list.
pub(crate) fn workflow_picker_title(error_count: usize) -> String {
    if error_count == 0 {
        " Pick workflow ".to_string()
    } else {
        format!(" Pick workflow ({} failed to load) ", error_count)
    }
}

/// Strip the directory-level `Io(NotFound)` from `load_all`'s error list and
/// stringify the rest. A missing `workflows/` directory (e.g. fresh install)
/// is not a "load failure" — it's an absent surface, already handled by the
/// existing "No workflows found in …" status. Treating it as a load error
/// would push the picker into "(1 failed to load)" mode on every bare repo.
pub(crate) fn filter_real_workflow_load_errors(
    workflows_dir: &Path,
    errs: Vec<(PathBuf, workflow::toml_schema::WorkflowError)>,
) -> Vec<(PathBuf, String)> {
    errs.into_iter()
        .filter(|(p, e)| {
            !(p == workflows_dir
                && matches!(
                    e,
                    workflow::toml_schema::WorkflowError::Io(io)
                        if io.kind() == std::io::ErrorKind::NotFound
                ))
        })
        .map(|(p, e)| (p, e.to_string()))
        .collect()
}

/// Routing decision for the `A-f` launch keybinding. The picker is normally
/// short-circuited when there are 0 or 1 valid workflows, but when load
/// errors exist we force-open it so the user sees which TOML files failed
/// — otherwise a typo silently drops a workflow with no hint.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WorkflowLaunchRouting {
    NoWorkflowsFound,
    LaunchOnly(String),
    OpenPicker(Vec<String>),
}

pub(crate) fn route_workflow_launch(
    valid_names: Vec<String>,
    has_load_errors: bool,
) -> WorkflowLaunchRouting {
    match (valid_names.len(), has_load_errors) {
        (0, false) => WorkflowLaunchRouting::NoWorkflowsFound,
        (1, false) => {
            WorkflowLaunchRouting::LaunchOnly(valid_names.into_iter().next().unwrap())
        }
        _ => WorkflowLaunchRouting::OpenPicker(valid_names),
    }
}

/// One-line summary for a single workflow load failure, rendered as a dim row
/// at the top of the picker dialog. Uses the file's basename to keep the row
/// short; the full path is logged to stderr at startup.
pub(crate) fn format_workflow_load_error(path: &Path, err: &str) -> String {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("<unknown>");
    let one_line = err.lines().next().unwrap_or("").trim();
    format!("⚠ {}: {}", name, one_line)
}

impl App {
    /// Drain new entries from `~/.claude/history.jsonl`. For each rotation-
    /// trigger entry (`/clear`, `/compact`) whose `sessionId` matches the
    /// bound sid of an active claude workflow role, find the new transcript
    /// file that was produced and rebind the role to it.
    pub(super) fn apply_history_rotations(&mut self) {
        // Drain new history.jsonl entries and route rotation triggers
        // (`/clear`, `/compact`) to the pending queue.
        if let Some(watcher) = self.history_watcher.as_mut() {
            let new_entries = watcher.poll();
            let now = Instant::now();
            for entry in &new_entries {
                if workflow::history::is_rotation_trigger(&entry.display) {
                    self.pending_rotations
                        .push((entry.session_id.clone(), entry.timestamp_ms, now));
                }
            }
        }
        if self.pending_rotations.is_empty() {
            return;
        }
        // Build (sid → binding) lookup for every claude session — both
        // workflow participants AND regular `A-n` / planning panes. The
        // pre-fix version only included workflow roles, which meant a
        // regular pane running `/clear` or `/compact` kept resolving to
        // the *old* transcript file forever and `read_session_output`
        // returned stale data on what looked like a healthy session.
        let bindings = collect_rotation_bindings(&self.workspaces);
        // Walk pending queue; resolve what we can, drop stale ones.
        let now = Instant::now();
        let max_age = Duration::from_secs(30);
        struct ResolvedRotation {
            wi: usize,
            si: usize,
            workflow: Option<(String, String)>,
            old_sid: String,
            new_sid: String,
        }
        let mut resolved: Vec<ResolvedRotation> = Vec::new();
        self.pending_rotations.retain(|(old_sid, ts_ms, first_seen)| {
            if now.duration_since(*first_seen) > max_age {
                return false;
            }
            let Some(binding) = bindings.get(old_sid) else {
                return true;
            };
            let Some(new_sid) =
                workflow::history::find_post_rotation_sid(&binding.worktree, *ts_ms)
            else {
                return true;
            };
            if &new_sid == old_sid {
                return false;
            }
            resolved.push(ResolvedRotation {
                wi: binding.wi,
                si: binding.si,
                workflow: binding.workflow.clone(),
                old_sid: old_sid.clone(),
                new_sid,
            });
            false
        });
        for r in &resolved {
            // Rebind via the helper so `generation` bumps. Without this,
            // a reader holding a cursor for the pre-rotation transcript
            // would skip messages in the new file (cursor offset N from
            // the old file applied to the new file).
            self.workspaces[r.wi].sessions[r.si]
                .rebind_transcript(Some(r.new_sid.clone()));
            // Sub-2b-1 review-r#2 #3: history rotation
            // changes the transcript file on disk; daemon
            // must learn the new path so its
            // `resolve_authorized_session` continues returning
            // the live file (and bumps its own `generation`
            // for cursor invalidation).
            if let Some(ws) = self.workspaces.get(r.wi) {
                if let Some(ts) = ws.sessions.get(r.si) {
                    Self::push_transcript_path_to_daemon_if_attached(&self.host_pool, ts, ws);
                }
            }
            // Workflow-specific bookkeeping only when the session is a
            // workflow participant — non-workflow rebinds just need the
            // transcript_id swap + generation bump above.
            if let Some((run_id, role)) = &r.workflow {
                // 10d-2c-1 review round-6 (F1): apply the TUI-owned
                // mutations through `modify` so concurrent daemon
                // writes (active_role / iteration / status /
                // events_offset) on the same run survive the RMW.
                // The closure is field-targeted: role_sessions[*],
                // role_baselines, and the active role's history-
                // last() correlation — all TUI territory. Daemon-
                // owned fields are untouched.
                let new_sid = r.new_sid.clone();
                let role_owned = role.clone();
                let updated = workflow::run::modify(run_id, move |run| {
                    apply_history_rotation_to_run(run, &role_owned, &new_sid);
                });
                if let Ok(updated) = updated {
                    if let Some(slot) = self
                        .workflow_runs
                        .iter_mut()
                        .find(|run| &run.run_id == run_id)
                    {
                        *slot = updated;
                    }
                    log_tick(
                        run_id,
                        &format!(
                            "history-rotation: role={} {} -> {}",
                            role, r.old_sid, r.new_sid
                        ),
                    );
                }
            }
        }
        if !resolved.is_empty() {
            self.save_session_manifest();
            self.set_status_msg("Session rotated (/clear or /compact)");
        }
    }

    /// Open the launch modal for a workflow, prefilled for the focused session.
    pub(super) fn open_workflow_launch(&mut self) {
        // migrate-tui-local Issue I: capture the cursor's task
        // scope alongside the workspace/session position. The
        // launching task_id threads through the modal → submit
        // action → `App::launch_workflow` → controller →
        // daemon `start_session` so a UI A-f on a tasked cursor
        // records `DaemonSession.task_id` for fresh
        // participants. Pre-fix only `(ws_idx, focused_si)` was
        // carried and the task scope was lost.
        let (wi, focused_si, cursor_task_id) = match self.cursor.clone() {
            Cursor::Session(wi, si) => {
                // A focused session may itself be tagged with a
                // task — capture that as the launching scope.
                let task = self
                    .workspaces
                    .get(wi)
                    .and_then(|w| w.sessions.get(si))
                    .and_then(|ts| ts.task_id.clone());
                (wi, Some(si), task)
            }
            Cursor::Workspace(wi) => (wi, None, None),
            Cursor::Task { ws_idx, task_id } => {
                // Focus on the first running session of the task so the
                // launch modal can prefill a reasonable worker slot.
                let si = self
                    .workspaces
                    .get(ws_idx)
                    .and_then(|w| {
                        w.sessions
                            .iter()
                            .position(|ts| ts.task_id.as_deref() == Some(task_id.as_str()))
                    });
                (ws_idx, si, Some(task_id))
            }
            Cursor::Backtest(_) => {
                self.set_status_msg(
                    "Workflows run on local sessions — backtest runs are cloud-only",
                );
                return;
            }
        };
        if wi >= self.workspaces.len() {
            self.set_status_msg("No workspace selected");
            return;
        }
        // Capture the STABLE workspace id now; a backend tick can reorder/remove
        // `workspaces` while the modal is open, so the launch/draw consumers
        // re-resolve from this id rather than trusting a frozen raw index.
        let ws_id = self.workspaces[wi].id.clone();
        // Same reason as `start_new_terminal_session`: workflow
        // participants are sibling sessions on the workspace, and a
        // push in flight will tombstone them on `PushComplete`.
        if self.workspaces[wi].is_pushing {
            self.set_status_msg("Workspace is being pushed to cloud, retry after");
            return;
        }

        let mut names: Vec<String> = self.workflows.keys().cloned().collect();
        names.sort();
        let has_load_errors = !self.workflow_load_errors.is_empty();
        match route_workflow_launch(names, has_load_errors) {
            WorkflowLaunchRouting::NoWorkflowsFound => {
                self.set_status_msg(&format!(
                    "No workflows found in {}",
                    workflow::toml_schema::workflows_dir().display()
                ));
            }
            WorkflowLaunchRouting::LaunchOnly(only) => {
                self.enter_workflow_launch_confirm(
                    ws_id,
                    focused_si,
                    only,
                    cursor_task_id,
                );
            }
            WorkflowLaunchRouting::OpenPicker(names) => {
                self.input_mode = InputMode::WorkflowPicker {
                    ws_id,
                    focused_si,
                    names,
                    selected: 0,
                    cursor_task_id,
                };
            }
        }
    }

    /// Build the per-role slot list for `wf_name` and enter the launch-confirm
    /// modal. Called both from the single-workflow fast path and after the
    /// user picks a workflow in `WorkflowPicker`.
    ///
    /// migrate-tui-local Issue I: `cursor_task_id` carries the
    /// launching task scope into the confirm modal so the
    /// downstream `SubmitAction::LaunchWorkflow` handler can
    /// forward it into `App::launch_workflow`.
    pub(super) fn enter_workflow_launch_confirm(
        &mut self,
        ws_id: String,
        focused_si: Option<usize>,
        wf_name: String,
        cursor_task_id: Option<String>,
    ) {
        let Some(wf) = self.workflows.get(&wf_name).cloned() else {
            self.set_status_msg(&format!(
                "Workflow '{}' not found (looked in {})",
                wf_name,
                workflow::toml_schema::workflows_dir().display()
            ));
            return;
        };

        // Phase 3 (doc/existing-session-binding.md): re-light the dormant
        // Existing-slot chooser. For each role ELIGIBLE for binding —
        // `Context::Persistent` AND `needs_mcp == false`, mirroring the daemon's
        // Phase 1 eligibility intent (in feedback.toml only the worker qualifies;
        // the reviewer is Fresh and the manager needs the workflow MCP) — offer
        // the focused workspace's DAEMON-OWNED sessions
        // (`daemon_session_uid.is_some()`) as `Existing(si)` options alongside
        // `New(engine)`. Ineligible roles, and any session without a daemon uid,
        // are offered/appear only as `New`. `New(engine)` stays index 0 (the
        // default), so a plain Enter fresh-spawns every role exactly as before —
        // binding is an explicit cycle-to-`Existing` choice. The old single
        // focused-session-as-worker binding (`focused_si`) is superseded: the
        // chooser now offers every eligible existing session in the workspace.
        let _ = focused_si;
        let ws_index = resolve_workspace_by_id(&self.workspaces, &ws_id);
        let mut slots = Vec::new();
        for role_name in wf.role_order.iter() {
            let role = &wf.roles[role_name];
            let mut options = vec![WorkflowSlotSource::New(role.engine.clone())];
            // Offer the OTHER engine too, so any role can be launched as "new
            // claude" or "new codex" regardless of its TOML default. The declared
            // engine stays at index 0 (the Enter default); the alternate is one
            // cycle away. The daemon honors a non-default pick via the
            // `role_engines` override (see `slots_to_role_engines`).
            let alt_engine = match role.engine {
                Engine::ClaudeCode => Engine::Codex,
                Engine::Codex => Engine::ClaudeCode,
            };
            options.push(WorkflowSlotSource::New(alt_engine));
            let eligible = role.context
                == workflow::toml_schema::Context::Persistent
                && !role.needs_mcp;
            if eligible {
                if let Some(ws) = ws_index.and_then(|i| self.workspaces.get(i)) {
                    for (si, sess) in ws.sessions.iter().enumerate() {
                        // Only DAEMON-OWNED sessions are bindable: the daemon's
                        // eligibility check requires `state.sessions` membership
                        // and we must forward the daemon uid, not the local UI
                        // handle. A purely TUI-local session is unbindable and
                        // must not be offered.
                        if sess.session.daemon_session_uid.is_some() {
                            options.push(WorkflowSlotSource::Existing(si));
                        }
                    }
                }
            }
            slots.push(WorkflowSlotChoice {
                role: role_name.clone(),
                options,
                option_index: 0,
            });
        }
        self.input_mode = InputMode::WorkflowLaunchConfirm {
            ws_id,
            workflow_name: wf_name,
            slots,
            active_slot: 0,
            goal: String::new(),
            cursor_task_id,
        };
    }

    /// Phase 4 §D/§E: launch a workflow through the daemon. Resolves the focused
    /// workspace's worktree + the active host's daemon socket, then calls the
    /// daemon `start_workflow` RPC (the daemon spawns participants, writes
    /// state.json, and drives the run). The TUI observes the result via
    /// `workflow_watch` / `manifest.watch` — it does not spawn or drive locally.
    pub(crate) fn launch_workflow_via_daemon(
        &mut self,
        ws_id: &str,
        workflow_name: &str,
        slots: &[WorkflowSlotChoice],
        goal: Option<String>,
        task_id: Option<String>,
    ) {
        // Re-resolve the stable id to a CURRENT index — a tick may have
        // reordered/removed workspaces while the launch modal was open.
        let Some(ws_index) = resolve_workspace_by_id(&self.workspaces, ws_id) else {
            self.set_status_msg("launch: workspace no longer exists");
            return;
        };
        let Some(ws) = self.workspaces.get(ws_index) else {
            self.set_status_msg("launch: invalid workspace");
            return;
        };
        let workspace_id = ws.id.clone();
        let Some(worktree) = ws.worktree_path.as_ref().map(|p| p.display().to_string())
        else {
            self.set_status_msg("launch: workspace has no worktree");
            return;
        };
        // Phase 3 (doc/existing-session-binding.md): map each `Existing(si)`
        // slot to `role -> daemon_session_uid`. We forward the DAEMON session
        // uid (`ws.sessions[si].session.daemon_session_uid`), NOT the local
        // `TerminalSession` UI handle — only daemon-owned sessions are bindable.
        // `New` slots (and any selected session that lost its daemon uid via a
        // mid-modal reconcile) contribute no entry, so the daemon fresh-spawns
        // those roles. Built while `ws` is still borrowed; the resulting map is
        // owned (cloned uids) so no borrow lingers.
        let session_uids: Vec<Option<String>> = ws
            .sessions
            .iter()
            .map(|s| s.session.daemon_session_uid.clone())
            .collect();
        // Host is a property of the WORKSPACE — its worktree lives on one host
        // and every session in it is pinned there. Resolve the launch target
        // from the workspace, NOT the global `active_host` (which only seeds the
        // NEXT new workspace). Using active_host here was the bug: A-f on a local
        // workspace while active_host=manager fired a doomed cross-host launch
        // (local worktree path + local uids sent to the remote daemon) and froze
        // the UI on the 150s start_workflow RPC over the flaky tunnel.
        let host_id = ws
            .sessions
            .first()
            .map(|s| s.host_id.clone())
            .unwrap_or_else(|| crate::hosts::HostId::local());
        let role_sessions = slots_to_role_sessions(slots, &session_uids);
        // Per-role "new claude" vs "new codex" overrides for fresh-spawned roles
        // (only the ones the operator cycled off their TOML default).
        let role_engines = slots_to_role_engines(slots);
        // Phase 5 (doc/remote-session-execution.md): A-f routes to the
        // WORKSPACE's host, ungated. `start_workflow` is already daemon-driven
        // — for a remote-hosted workspace the worktree path and the bound
        // session uids are the REMOTE daemon's own (Phase 3 created the
        // worktree + sessions there), so the launch is correct against that
        // host's socket. `for_host(&host_id)` below resolves that socket and
        // surfaces a clear "daemon unavailable" message if the host is
        // unreachable; the TUI then observes the run via the existing
        // workflow event / manifest streams. Local A-f is unchanged
        // (host_id == local → the local socket, exactly as before).
        let daemon_socket = match self.host_pool.for_host(&host_id) {
            Ok(h) => match h.socket_path() {
                Some(p) => p,
                None => {
                    self.set_status_msg("launch: daemon has no socket path");
                    return;
                }
            },
            Err(e) => {
                self.set_status_msg(&format!("launch: daemon unavailable: {e}"));
                return;
            }
        };
        match crate::client_session::rpc_start_workflow(
            &daemon_socket,
            &self.host_pool.operator_token_for(&host_id),
            workflow_name,
            &worktree,
            &workspace_id,
            goal.as_deref(),
            task_id.as_deref(),
            &role_sessions,
            &role_engines,
            self.last_term_size,
        ) {
            Ok(run_id) => {
                self.set_status_msg(&format!("Launched {} ({})", workflow_name, run_id))
            }
            Err(e) => self.set_status_msg(&format!("launch failed: {e}")),
        }
    }

    /// Called once per main loop iteration. The TUI is a pure observer
    /// (Phase 4 §E): the daemon poller drives every run; this pass only
    /// reconciles stopped/finished runs out of the in-memory view.
    pub fn tick_workflows(&mut self) {
        // Phase 4 §E: the TUI is a pure OBSERVER — the daemon poller drives every
        // run (decisions, delivery, history append). The TUI no longer ticks a
        // controller; run state arrives via `workflow_watch` (events.subscribe +
        // state.json reload). This pass only reconciles stopped/finished runs
        // out of the in-memory observer view.
        self.reconcile_stopped_workflow_runs();
    }

    /// Mark the focused session's workflow run as paused. No-op if the focused
    /// session isn't in a workflow or the run is already paused/done.
    ///
    /// Called when the user hits Ctrl-C on a participant session — the
    /// keystroke itself is still forwarded to the PTY so the agent receives
    /// the interrupt as it would in a normal terminal.
    pub(super) fn pause_focused_workflow(&mut self) {
        let run_id = match self.focused_session_run_id() {
            Some(id) => id,
            None => return,
        };
        // 10d-2c-1 review round-6 (F1): targeted modify under
        // flock — applies the pause field to whatever's on disk
        // (including any daemon-written advance since our in-mem
        // copy was loaded) and keeps the in-mem copy in sync via
        // the returned run.
        let updated = workflow::run::modify(&run_id, |r| {
            if matches!(r.status, workflow::RunStatus::Running) {
                r.set_paused(true);
            }
        });
        if let Ok(updated) = updated {
            if let Some(slot) = self.workflow_runs.iter_mut().find(|r| r.run_id == run_id) {
                *slot = updated;
            }
            self.set_status_msg("Workflow paused (A-u to resume)");
        }
    }

    pub(super) fn resume_workflow_for_cursor(&mut self) {
        let run_id = match self.focused_session_run_id() {
            Some(id) => id,
            None => {
                self.set_status_msg("Focused session is not in a workflow");
                return;
            }
        };
        // 10d-2c-1 review round-6 (F1): same shape as pause.
        let mut was_paused = false;
        let updated = workflow::run::modify(&run_id, |r| {
            if matches!(r.status, workflow::RunStatus::Paused) {
                r.set_paused(false);
                was_paused = true;
            }
        });
        if let Ok(updated) = updated {
            if let Some(slot) = self.workflow_runs.iter_mut().find(|r| r.run_id == run_id) {
                *slot = updated;
            }
            if was_paused {
                self.set_status_msg(&format!("Resumed workflow {}", run_id));
            } else {
                self.set_status_msg("Workflow is not paused");
            }
        }
    }

    /// Stop a workflow by run id.
    ///
    /// The workflow run is marked detached (no more transitions will fire) and
    /// the participating sessions have their workflow tags cleared so they
    /// behave like normal standalone sessions from here on. The sessions
    /// themselves stay open and their transcripts are preserved.
    pub(crate) fn stop_workflow_run(&mut self, run_id: &str) {
        // 10d-2c-1 review round-6 (F1): targeted modify so a
        // concurrent daemon write doesn't get clobbered. Detach
        // is final; the field-level set still applies cleanly on
        // top of whatever the daemon left.
        //
        // 10d-2c-1 review round-9: terminal-state guard.
        // [`apply_stop_workflow_status`] preserves `Done` so the
        // UI stop shortcut never overwrites a naturally-completed
        // run — matches the MCP `stop_workflow`'s guard predicate
        // (`daemon/src/control/methods.rs::stop_workflow`).
        let _ = workflow::run::modify(run_id, apply_stop_workflow_status);
        self.apply_local_workflow_stop_cleanup(run_id);
        self.set_status_msg("Workflow stopped");
    }

    /// 10d-3 r1 F1: shared local-cleanup helper used by both the
    /// A-o handler (`stop_workflow_run`) and the tick-level
    /// detector that notices a daemon-initiated `stop_workflow`
    /// (e.g. an MCP caller dropping a run while the TUI is open).
    ///
    /// Walks `self.workspaces`, clears `workflow_run_id` /
    /// `workflow_role` and unsets `hidden` on every session
    /// pointing at `run_id`, drops the run from
    /// `self.workflow_runs`, and persists the manifest. Does NOT
    /// write `state.json` — the caller decides whether to invoke
    /// `workflow::run::modify` first (UI path does) or whether the
    /// daemon already flipped status (tick path does).
    pub(crate) fn apply_local_workflow_stop_cleanup(&mut self, run_id: &str) {
        drop_run_from_in_mem(&mut self.workflow_runs, &mut self.workspaces, run_id);
        self.save_session_manifest();
    }

    /// 10d-3 r1 F1: tick-level reconciliation of TUI workflow
    /// state against the daemon's authoritative `state.json`.
    /// Called from the periodic tick. For each run in
    /// `self.workflow_runs`, peek at disk; if its on-disk status
    /// is no longer "active" (Detached or Done), run the local
    /// cleanup helper so the sidebar matches the daemon view
    /// without requiring the user to press `A-o`.
    ///
    /// Without this, a session whose run the daemon stopped via
    /// MCP `stop_workflow` would keep showing as a workflow
    /// participant until TUI restart, even though the agent's
    /// next workflow_transition would (correctly) be rejected as
    /// "run not active".
    pub(crate) fn reconcile_stopped_workflow_runs(&mut self) {
        let dropped =
            drop_inactive_runs_from_in_mem(&mut self.workflow_runs, &mut self.workspaces);
        if dropped > 0 {
            self.save_session_manifest();
        }
    }

    pub(super) fn open_workflow_history(&mut self) {
        let run_id = match self.focused_session_run_id() {
            Some(id) => id,
            None => {
                self.set_status_msg("Focused session is not in a workflow");
                return;
            }
        };
        self.input_mode = InputMode::WorkflowHistory { run_id };
    }

    pub(super) fn focused_session_run_id(&self) -> Option<String> {
        match self.cursor.clone() {
            Cursor::Session(wi, si) => self
                .workspaces
                .get(wi)
                .and_then(|w| w.sessions.get(si))
                .and_then(|s| s.workflow_run_id.clone()),
            Cursor::Task { ws_idx, task_id } => {
                // Return the first workflow run_id seen on any session tagged
                // with this task. Usually unique per task.
                self.workspaces.get(ws_idx).and_then(|w| {
                    w.sessions
                        .iter()
                        .filter(|ts| ts.task_id.as_deref() == Some(task_id.as_str()))
                        .find_map(|ts| ts.workflow_run_id.clone())
                })
            }
            Cursor::Workspace(_) => None,
            Cursor::Backtest(_) => None,
        }
    }
}

#[cfg(test)]
mod stop_workflow_terminal_guard_tests {
    //! 10d-2c-1 review round-9: pin the terminal-state guard in
    //! `apply_stop_workflow_status`. UI stop shortcut must not
    //! overwrite `Done` (naturally completed). Mirrors the MCP
    //! `stop_workflow`'s guard predicate so the two paths agree.
    use super::*;
    use crate::workflow::run::RunStatus;

    fn with_temp_home<F: FnOnce()>(f: F) -> tempfile::TempDir {
        let _g = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let orig = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", tmp.path()); }
        f();
        if let Some(o) = orig {
            unsafe { std::env::set_var("HOME", o); }
        } else {
            unsafe { std::env::remove_var("HOME"); }
        }
        tmp
    }

    fn seed_run(run_id: &str, status: RunStatus, done_reason: Option<&str>) {
        let mut run = WorkflowRun::new(
            run_id.to_string(),
            "feedback".to_string(),
            "task-key".to_string(),
            std::collections::BTreeMap::new(),
            "worker".to_string(),
            std::collections::BTreeMap::new(),
            None,
            std::collections::BTreeMap::new(),
            0,
        );
        run.status = status;
        run.done_reason = done_reason.map(str::to_string);
        workflow::run::save(&run).expect("seed save");
    }

    /// Named acceptance test: state.json with `status: Done` +
    /// `done_reason` survives a stop_workflow_run call. Pre-r9
    /// the closure body unconditionally ran `mark_detached()`,
    /// flipping `Done` → `Detached` and erasing the distinction
    /// between successful completion and operator abort.
    #[test]
    fn stop_workflow_run_noops_on_terminal_done() {
        let _tmp = with_temp_home(|| {
            let run_id = "wf_r9_done_preserved";
            seed_run(run_id, RunStatus::Done, Some("worker said done"));

            // Drive the same modify-closure shape stop_workflow_run
            // uses. We don't construct a full App; the fix lives
            // entirely in `apply_stop_workflow_status`, so a
            // focused state.json round-trip pins the behavior.
            let _ = workflow::run::modify(run_id, apply_stop_workflow_status);

            let post = workflow::run::load_one(run_id).expect("post load");
            assert!(
                matches!(post.status, RunStatus::Done),
                "Done must NOT be overwritten by stop; got {:?}",
                post.status,
            );
            assert_eq!(
                post.done_reason.as_deref(),
                Some("worker said done"),
                "done_reason must NOT be cleared by stop",
            );
        });
    }

    /// Companion test: state.json with `status: Running` is
    /// marked `Detached`. Guards against over-correcting r9
    /// (e.g. treating every non-Detached as terminal).
    #[test]
    fn stop_workflow_run_marks_detached_on_running() {
        let _tmp = with_temp_home(|| {
            let run_id = "wf_r9_running_detaches";
            seed_run(run_id, RunStatus::Running, None);

            let _ = workflow::run::modify(run_id, apply_stop_workflow_status);

            let post = workflow::run::load_one(run_id).expect("post load");
            assert!(
                matches!(post.status, RunStatus::Detached),
                "Running must transition to Detached on stop; got {:?}",
                post.status,
            );
        });
    }

    /// Companion: Paused also transitions to Detached. Captures
    /// the "anything-not-Done" branch.
    #[test]
    fn stop_workflow_run_marks_detached_on_paused() {
        let _tmp = with_temp_home(|| {
            let run_id = "wf_r9_paused_detaches";
            seed_run(run_id, RunStatus::Paused, None);

            let _ = workflow::run::modify(run_id, apply_stop_workflow_status);

            let post = workflow::run::load_one(run_id).expect("post load");
            assert!(
                matches!(post.status, RunStatus::Detached),
                "Paused must transition to Detached on stop; got {:?}",
                post.status,
            );
        });
    }
}

#[cfg(test)]
mod stop_workflow_local_cleanup_tests {
    //! 10d-3 r1 F1+F2 tests: pin the extracted local-cleanup
    //! helper (`untag_stale_workflow`) used by `restore_sessions`'
    //! pre-spawn untag, and pin the on-disk predicate that the
    //! tick-level reconciler relies on. Both fixes target the same
    //! drift: a `workflow_run_id` on a `ManifestEntry` or
    //! `TerminalSession` outliving the `WorkflowRun` it references.
    use super::*;
    use crate::workflow::run::RunStatus;

    fn with_temp_home<F: FnOnce()>(f: F) -> tempfile::TempDir {
        let _g = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let orig = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", tmp.path()); }
        f();
        if let Some(o) = orig {
            unsafe { std::env::set_var("HOME", o); }
        } else {
            unsafe { std::env::remove_var("HOME"); }
        }
        tmp
    }

    fn seed_run(run_id: &str, status: RunStatus) {
        let mut run = WorkflowRun::new(
            run_id.to_string(),
            "feedback".to_string(),
            "task-key".to_string(),
            std::collections::BTreeMap::new(),
            "worker".to_string(),
            std::collections::BTreeMap::new(),
            None,
            std::collections::BTreeMap::new(),
            0,
        );
        run.status = status;
        workflow::run::save(&run).expect("seed save");
    }

    fn entry_with_workflow(run_id: Option<&str>) -> ManifestEntry {
        ManifestEntry {
            color: None,
            memory_cap_soft_bytes: None,
            memory_cap_hard_bytes: None,
            cgroup_prefix: None,
            uid: "sess-u1".to_string(),
            managed_by_uid: None,
            generation: 0,
            label: "worker-test".to_string(),
            session_type: "claude-code".to_string(),
            transcript_id: None,
            hidden: run_id.is_some(),
            idle_timeout_secs: 0,
            burst_threshold: 0,
            workflow_run_id: run_id.map(str::to_string),
            workflow_role: run_id.map(|_| "worker".to_string()),
            continuous_task_id: None,
            task_id: None,
            notify_on_idle: false,
            global_perms: false,
            seeded_from_snapshot: None,
            last_exit: None,
            host_id: cm_daemon::host_id::HostId::local(),
        }
    }

    /// F2 predicate test: when `workflow_run_id` references a run
    /// that's NOT in the active set, the entry must be cloned and
    /// returned with the workflow tags cleared. This is the
    /// pre-spawn untag that prevents `spawn_restored_session` from
    /// reading a stale `CM_WORKFLOW_RUN_ID` into the agent's MCP
    /// env.
    #[test]
    fn untag_stale_workflow_clears_when_run_not_active() {
        let entry = entry_with_workflow(Some("wf_stale_42"));
        let mut active = std::collections::HashSet::new();
        active.insert("wf_some_other".to_string());

        let cleaned = untag_stale_workflow(&entry, &active)
            .expect("stale tag must produce cleaned entry");

        assert!(cleaned.workflow_run_id.is_none(), "run_id must be cleared");
        assert!(cleaned.workflow_role.is_none(), "role must be cleared");
        assert!(!cleaned.hidden, "hidden must reset to false");
        // Original is untouched — Cow semantics.
        assert_eq!(entry.workflow_run_id.as_deref(), Some("wf_stale_42"));
    }

    /// F2 negative: when the `workflow_run_id` IS in the active
    /// set, the helper must return `None` so the spawn loop reuses
    /// the original entry. Confirms we don't churn the manifest
    /// for live workflow participants.
    #[test]
    fn untag_stale_workflow_noop_when_run_active() {
        let entry = entry_with_workflow(Some("wf_live_99"));
        let mut active = std::collections::HashSet::new();
        active.insert("wf_live_99".to_string());

        let cleaned = untag_stale_workflow(&entry, &active);
        assert!(cleaned.is_none(), "live run must not be untagged");
    }

    /// F2 negative: an entry that has no workflow tag must be
    /// passed through. Guards against the predicate firing on
    /// non-participant sessions.
    #[test]
    fn untag_stale_workflow_noop_when_no_tag() {
        let entry = entry_with_workflow(None);
        let active = std::collections::HashSet::new();

        let cleaned = untag_stale_workflow(&entry, &active);
        assert!(cleaned.is_none(), "untagged entry must be passed through");
    }

    /// F1 round-trip: a run flipped to `Detached` on disk no
    /// longer satisfies `is_active()`, so the tick-level
    /// reconciler will treat it as stale and run cleanup. Pins
    /// the predicate that `reconcile_stopped_workflow_runs`
    /// queries, matching the daemon-side `stop_workflow`'s effect.
    #[test]
    fn reconcile_predicate_treats_detached_as_inactive() {
        let _tmp = with_temp_home(|| {
            let run_id = "wf_f1_detached";
            seed_run(run_id, RunStatus::Detached);

            let post = workflow::run::load_one(run_id).expect("post load");
            assert!(
                !post.is_active(),
                "Detached must NOT satisfy is_active(); got {:?}",
                post.status,
            );
        });
    }

    /// F1 round-trip: `Done` runs (natural completion via
    /// `workflow_done`) are also non-active. The tick path treats
    /// them the same as Detached — both warrant local cleanup so
    /// the sidebar doesn't keep a dangling tag.
    #[test]
    fn reconcile_predicate_treats_done_as_inactive() {
        let _tmp = with_temp_home(|| {
            let run_id = "wf_f1_done";
            seed_run(run_id, RunStatus::Done);

            let post = workflow::run::load_one(run_id).expect("post load");
            assert!(
                !post.is_active(),
                "Done must NOT satisfy is_active(); got {:?}",
                post.status,
            );
        });
    }

    /// F1 control: `Running` (and `Paused`) remain active. The
    /// tick path must NOT untag a run that's still in flight.
    #[test]
    fn reconcile_predicate_treats_running_as_active() {
        let _tmp = with_temp_home(|| {
            let run_id = "wf_f1_running";
            seed_run(run_id, RunStatus::Running);

            let post = workflow::run::load_one(run_id).expect("post load");
            assert!(
                post.is_active(),
                "Running must satisfy is_active(); got {:?}",
                post.status,
            );
        });
    }

    /// F1 round-trip: a missing run on disk is treated as
    /// inactive. The tick predicate uses
    /// `load_one(...).unwrap_or(false)`, so a deleted/never-saved
    /// run file does not panic — the tracked-but-missing entry is
    /// cleaned up.
    #[test]
    fn reconcile_predicate_treats_missing_as_inactive() {
        let _tmp = with_temp_home(|| {
            // No seed: run was never saved.
            let still_active = workflow::run::load_one("wf_f1_missing")
                .map(|r| r.is_active())
                .unwrap_or(false);
            assert!(
                !still_active,
                "missing run must NOT satisfy is_active()"
            );
        });
    }
}

#[cfg(test)]
mod rotation_binding_tests {
    //! Regression tests for the `/clear` and `/compact` rotation rebind
    //! path. The pre-fix version only rebound workflow roles, so a
    //! regular `A-n` Claude pane that ran `/clear` would keep resolving
    //! to the *old* transcript file forever and `read_session_output`
    //! returned stale data on what looked like a healthy session.

    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::Instant;

    fn dummy_session() -> Session {
        Session::new("/bin/true", &[], 80, 24, None, HashMap::new(), None)
            .expect("dummy session")
    }

    fn ts_with(
        sid: Option<&str>,
        workflow: Option<(&str, &str)>,
    ) -> TerminalSession {
        TerminalSession {
            color: None,
            uid: "uid".into(),
            label: "test".into(),
            session_type: "claude".into(),
            session: dummy_session(),
            status: SessionStatus::Idle,
            idle_since: None,
            last_write_at: None,
            transcript_id: sid.map(str::to_string),
            generation: 0,
            pending_jsonl_files: None,
            hidden: false,
            idle_timeout_secs: 0,
            burst_threshold: 0,
            pending_prompt: None,
            pending_clear: None,
            workflow_run_id: workflow.map(|(r, _)| r.to_string()),
            workflow_role: workflow.map(|(_, role)| role.to_string()),
            continuous_task_id: None,
            task_id: None,
            last_delivery: None,
            notify_on_idle: false,
            global_perms: false,
            pending_enter: None,
            created_at: Instant::now(),
            managed_by_uid: None,
            seeded_from_snapshot: None,
            preserved_last_exit: None,
            host_id: crate::hosts::HostId::local(),
        }
    }

    fn ws_with(sessions: Vec<TerminalSession>) -> Workspace {
        Workspace {
            color: None,
            pinned: false,
            id: "ws-1".into(),
            name: "ws".into(),
            is_closed: false,
            is_cloud: false,
            repo_url: None,
            worktree_path: Some(PathBuf::from("/tmp/ws")),
            main_repo_path: None,
            worker_vm: None,
            worker_zone: None,
            host_id: cm_daemon::host_id::HostId::local(),
            sessions,
            tombstones: vec![],
            is_pushing: false,
        }
    }

    #[test]
    fn cursor_selected_session_uid_resolves_session_cursor() {
        // Clear-on-focus depends on this mapping cursor → uid: it's the
        // uid whose pending notify_user alert gets cleared.
        let mut s0 = ts_with(None, None);
        s0.uid = "uid-0".into();
        let mut s1 = ts_with(None, None);
        s1.uid = "uid-1".into();
        let workspaces = vec![ws_with(vec![s0, s1])];

        assert_eq!(
            cursor_selected_session_uid(&Cursor::Session(0, 1), &workspaces),
            Some("uid-1"),
        );
        // A non-session cursor selects nothing — no alert should clear.
        assert_eq!(
            cursor_selected_session_uid(&Cursor::Workspace(0), &workspaces),
            None,
        );
        // Out-of-range indices resolve to None rather than panicking.
        assert_eq!(
            cursor_selected_session_uid(&Cursor::Session(0, 9), &workspaces),
            None,
        );
        assert_eq!(
            cursor_selected_session_uid(&Cursor::Session(5, 0), &workspaces),
            None,
        );
    }

    #[test]
    fn binding_includes_non_workflow_claude_session() {
        // The fix: a regular pane (no workflow_run_id/role) with a
        // bound transcript_id must still appear in the rotation
        // bindings map. Without this, `/clear` from that pane never
        // rebound and `read_session_output` stalled on the old file.
        let ws = ws_with(vec![ts_with(Some("solo-sid"), None)]);
        let bindings = collect_rotation_bindings(&[ws]);
        assert!(
            bindings.contains_key("solo-sid"),
            "non-workflow session must be tracked for rotation; got keys {:?}",
            bindings.keys().collect::<Vec<_>>(),
        );
        let b = bindings.get("solo-sid").unwrap();
        assert!(b.workflow.is_none());
    }

    #[test]
    fn binding_includes_workflow_claude_session() {
        let ws = ws_with(vec![ts_with(
            Some("worker-sid"),
            Some(("wf_1", "worker")),
        )]);
        let bindings = collect_rotation_bindings(&[ws]);
        let b = bindings
            .get("worker-sid")
            .expect("workflow session must still be tracked");
        assert_eq!(
            b.workflow.as_ref().map(|(r, role)| (r.as_str(), role.as_str())),
            Some(("wf_1", "worker"))
        );
    }

    #[test]
    fn binding_skips_session_without_transcript_id() {
        // No bound transcript yet — nothing to rotate from.
        let ws = ws_with(vec![ts_with(None, None)]);
        assert!(collect_rotation_bindings(&[ws]).is_empty());
    }

    #[test]
    fn binding_skips_codex_session() {
        // Codex doesn't go through history.jsonl rotation — its session
        // metadata is in the rollout file itself. Skip from this map.
        let mut ts = ts_with(Some("codex-sid"), None);
        ts.session_type = "codex".into();
        let ws = ws_with(vec![ts]);
        assert!(collect_rotation_bindings(&[ws]).is_empty());
    }

    #[test]
    fn binding_skips_workspace_without_worktree() {
        // Cloud workspaces clear `worktree_path` after `push_active`.
        // Without the path we can't compute the project dir, so skip.
        let mut ws = ws_with(vec![ts_with(Some("sid"), None)]);
        ws.worktree_path = None;
        assert!(collect_rotation_bindings(&[ws]).is_empty());
    }

    /// End-to-end-ish: write two sequential transcript files for the
    /// same logical session under `~/.claude/projects/<encoded>/`,
    /// confirm `find_post_rotation_sid` picks up the newer one given
    /// a rotation timestamp between the two file timestamps.
    #[test]
    fn find_post_rotation_picks_newer_transcript() {
        let _g = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        let worktree = std::path::PathBuf::from("/tmp/myrepo");
        // Encoded path matches the agent module's rule: '/' and '.' → '-'.
        let encoded = "-tmp-myrepo";
        let proj = tmp.path().join(format!(".claude/projects/{}", encoded));
        std::fs::create_dir_all(&proj).unwrap();

        // Old transcript: 2026-01-01T00:00:01Z = 1767225601000 ms.
        let old_line = r#"{"timestamp":"2026-01-01T00:00:01.000Z","type":"user","message":{"role":"user","content":"old"}}"#;
        std::fs::write(proj.join("old-sid.jsonl"), old_line).unwrap();

        // Rotation marker between the two transcripts. With the 2s
        // slack in `find_post_rotation_sid`, the old file's
        // `first_ts + 2000 < after_ms` filter requires after_ms to be
        // strictly greater than 1767225603000.
        let rotation_at = 1767225604000_u64;

        // New transcript: 2026-01-01T00:00:06Z = 1767225606000 ms.
        let new_line = r#"{"timestamp":"2026-01-01T00:00:06.000Z","type":"user","message":{"role":"user","content":"new"}}"#;
        std::fs::write(proj.join("new-sid.jsonl"), new_line).unwrap();

        let found = workflow::history::find_post_rotation_sid(&worktree, rotation_at);

        unsafe {
            if let Some(h) = old_home {
                std::env::set_var("HOME", h);
            } else {
                std::env::remove_var("HOME");
            }
        }

        assert_eq!(
            found.as_deref(),
            Some("new-sid"),
            "must pick the newer transcript (timestamp >= rotation)",
        );
    }
}

#[cfg(test)]
mod workflow_load_errors_tests {
    //! Pins down that workflow TOML failures captured by `load_all` flow into
    //! the picker surface. Without this, a typo in `workflows/feedback.toml`
    //! silently makes that workflow disappear from `A-f` with no hint.

    use super::{
        filter_real_workflow_load_errors, format_workflow_load_error, route_workflow_launch,
        workflow_picker_title, WorkflowLaunchRouting,
    };
    use crate::workflow;

    const VALID_TOML: &str = r#"
name = "good"
description = "Loads cleanly"
[roles.solo]
engine = "claude-code"
context = "persistent"
"#;

    /// `load_all` walks a workflows dir and partitions entries into
    /// (parsed, errors). A garbage TOML file lands in the errors bucket
    /// while siblings keep parsing — that's the contract App::new relies
    /// on to populate `workflow_load_errors` without losing valid loads.
    #[test]
    fn load_all_returns_errors_for_invalid_toml_alongside_valid_ones() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("good.toml"), VALID_TOML).unwrap();
        std::fs::write(tmp.path().join("bad.toml"), "not = = valid toml").unwrap();

        let (workflows, errors) = workflow::toml_schema::load_all(tmp.path());

        assert_eq!(workflows.len(), 1, "valid workflow should still parse");
        assert!(workflows.contains_key("good"));
        assert_eq!(errors.len(), 1, "invalid workflow should be reported");
        assert!(
            errors[0].0.file_name().and_then(|s| s.to_str()) == Some("bad.toml"),
            "error tuple should carry the offending file path",
        );
    }

    /// End-to-end: feed real `load_all` output through the same conversion
    /// `App::new` does, then check both surfaces (picker title + per-row
    /// summary) include identifying info. This is the surface promise users
    /// see when they open the picker after a load failure.
    #[test]
    fn captured_errors_render_in_picker_title_and_dim_rows() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("typo.toml"), "name = \n").unwrap();
        let (_workflows, errors) = workflow::toml_schema::load_all(tmp.path());
        assert_eq!(errors.len(), 1);

        // Mirror what App::new stores.
        let app_errors: Vec<(std::path::PathBuf, String)> = errors
            .into_iter()
            .map(|(p, e)| (p, e.to_string()))
            .collect();

        let title = workflow_picker_title(app_errors.len());
        assert_eq!(title, " Pick workflow (1 failed to load) ");

        let row = format_workflow_load_error(&app_errors[0].0, &app_errors[0].1);
        assert!(row.contains("typo.toml"), "row should name the file: {}", row);
        assert!(row.starts_with('⚠'), "row should be visually flagged: {}", row);
    }

    #[test]
    fn workflow_picker_title_omits_count_when_no_errors() {
        assert_eq!(workflow_picker_title(0), " Pick workflow ");
    }

    /// One valid + zero errors: the picker is short-circuited and we go
    /// straight to launch-confirm. This is the existing fast path.
    #[test]
    fn route_one_valid_no_errors_short_circuits_to_launch() {
        let r = route_workflow_launch(vec!["feedback".into()], false);
        assert_eq!(r, WorkflowLaunchRouting::LaunchOnly("feedback".into()));
    }

    /// One valid + one bad TOML: the picker MUST open so the dim error row
    /// is visible. Without this, the user would jump straight to confirm
    /// for the lone valid workflow with no hint that another file failed.
    /// This is the bug the reviewer flagged.
    #[test]
    fn route_one_valid_with_load_errors_forces_picker_open() {
        let r = route_workflow_launch(vec!["feedback".into()], true);
        match r {
            WorkflowLaunchRouting::OpenPicker(names) => {
                assert_eq!(names, vec!["feedback".to_string()]);
            }
            other => panic!("expected OpenPicker, got {:?}", other),
        }
    }

    /// Zero valid + at least one bad TOML: don't show the misleading
    /// "No workflows found in <dir>" status — open the picker so the load
    /// errors are surfaced as the dim rows on top.
    #[test]
    fn route_zero_valid_with_load_errors_opens_empty_picker() {
        let r = route_workflow_launch(Vec::new(), true);
        match r {
            WorkflowLaunchRouting::OpenPicker(names) => {
                assert!(names.is_empty());
            }
            other => panic!("expected OpenPicker, got {:?}", other),
        }
    }

    /// Zero valid + zero errors: the genuine empty-dir case keeps its
    /// original status_msg. We don't want to open an empty picker on a
    /// fresh checkout where no `workflows/` dir exists yet.
    #[test]
    fn route_zero_valid_no_errors_reports_not_found() {
        let r = route_workflow_launch(Vec::new(), false);
        assert_eq!(r, WorkflowLaunchRouting::NoWorkflowsFound);
    }

    /// Missing `workflows/` dir: `load_all` returns `(empty, [(dir, Io(NotFound))])`.
    /// That is NOT a per-file load failure — it's an absent surface — so the
    /// filter must drop it. Otherwise a fresh install gets the misleading
    /// "(1 failed to load)" picker title every time.
    #[test]
    fn filter_drops_directory_level_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let (workflows, errs) = workflow::toml_schema::load_all(&missing);

        assert!(workflows.is_empty());
        assert_eq!(errs.len(), 1, "load_all reports the absent dir");
        let filtered = filter_real_workflow_load_errors(&missing, errs);
        assert!(
            filtered.is_empty(),
            "directory-level NotFound must be filtered: {:?}",
            filtered
        );

        // And the routing then falls through to NoWorkflowsFound, matching
        // the pre-existing "No workflows found in <dir>" UX.
        let r = route_workflow_launch(Vec::new(), !filtered.is_empty());
        assert_eq!(r, WorkflowLaunchRouting::NoWorkflowsFound);
    }

    /// Real per-file failures (parse errors, validation errors) MUST still
    /// pass through the filter — that's the whole point of the surface.
    #[test]
    fn filter_keeps_per_file_parse_errors() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("bad.toml"), "not = = valid").unwrap();
        let (_workflows, errs) = workflow::toml_schema::load_all(tmp.path());
        assert_eq!(errs.len(), 1);

        let filtered = filter_real_workflow_load_errors(tmp.path(), errs);
        assert_eq!(filtered.len(), 1, "parse error must survive the filter");
        assert_eq!(
            filtered[0].0.file_name().and_then(|s| s.to_str()),
            Some("bad.toml"),
        );
    }
}

#[cfg(test)]
mod respawn_kills_daemon_session_tests {
    //! Slice 10d-memory-cap-relocation review finding.
    //!
    //! Pins the structural invariant the reviewer surfaced:
    //! `respawn_existing_with_workflow_mcp` swaps a fresh
    //! `Session` into a live `TerminalSession` slot via
    //! `ts.session = new_sess`. For local-PTY sessions, dropping
    //! the old `Session` closes its master fd and reaps the old
    //! agent. For daemon-attached sessions, Drop is detach-only
    //! by design (slice 10c-e-3b-fix2) — so without an explicit
    //! `App::kill_daemon_session_if_attached` BEFORE the
    //! assignment, the daemon's old PTY child stays alive while
    //! a freshly-resumed agent starts in the same slot.
    //! Duplicate live agents, transcript / worktree races.
    //!
    //! This is the same bug class as the round-33 finding 1
    //! (`MCP kill_session` orphan) and the slice-10c-e-3b-fix2
    //! teardown-paths sweep — a *missed call site* of the kill
    //! helper. The pinning test guards against a future
    //! refactor that re-removes the call.
    //!
    //! **Why a source-presence test rather than a behavioral
    //! one**: `respawn_existing_with_workflow_mcp` calls
    //! `crate::session::spawn_agent_session`, which spawns a
    //! real PTY child running the agent binary. Driving that
    //! end-to-end requires a real worktree, a live daemon, and
    //! the agent binary on `$PATH` — far too heavy for a unit
    //! test. The lower-cost behavioral coverage already exists
    //! (`client_session::tests` verifies that
    //! `kill_daemon_session_if_attached` actually removes the
    //! daemon-side registry entry). What's missing — and what
    //! this test adds — is the *call-site* pin: a future change
    //! that re-introduces the bug by removing or reordering the
    //! call will fail this test by name.

    const APP_SRC: &str = crate::app::APP_SRC_FOR_SCAN;

    /// Locate the start of `pub(crate) fn respawn_existing_with_workflow_mcp`
    /// in this file and return the function body — from the
    /// `{` after the signature through the matching `}`. Used
    /// to scope the structural assertions below to the body of
    /// the function under test (not the whole file).
    fn respawn_body() -> &'static str {
        let sig_marker = "pub(crate) fn respawn_existing_with_workflow_mcp";
        let sig_idx = APP_SRC
            .find(sig_marker)
            .expect("respawn_existing_with_workflow_mcp must exist in app.rs");
        let from_sig = &APP_SRC[sig_idx..];

        // Find the first `{` that opens the body (after the
        // signature + return type). `signed -> Option<String> {`
        let body_open = from_sig
            .find('{')
            .expect("function signature must be followed by an opening brace");
        let body = &from_sig[body_open..];

        // Find the matching closing brace by counting depth.
        let mut depth = 0usize;
        let mut end = body.len();
        for (i, c) in body.char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        &body[..end]
    }

    /// The named acceptance test. The function must call
    /// `kill_daemon_session_if_attached(ts)` before assigning
    /// `ts.session = new_sess`. Without that ordering, a
    /// daemon-attached session being respawned leaves a live
    /// orphan on the daemon side.
    #[test]
    fn respawn_calls_kill_daemon_before_session_swap() {
        let body = respawn_body();
        // 12e: helper signature changed to take `&HostPool`
        // first; the textual needle is the multi-arg call form.
        let kill_idx = body.find("kill_daemon_session_if_attached(host_pool, ts)").unwrap_or_else(|| {
            panic!(
                "respawn_existing_with_workflow_mcp must call \
                 App::kill_daemon_session_if_attached(host_pool, ts) before \
                 swapping `ts.session`. For daemon-attached sessions, Drop is \
                 detach-only — without the explicit kill, the daemon's old \
                 PTY child outlives the swap and races the new agent. \
                 Same bug class as round-33 finding 1 + the slice \
                 10c-e-3b-fix2 teardown-paths sweep.\n\nFunction body:\n{}",
                body
            )
        });
        let swap_idx = body.find("ts.session = new_sess").unwrap_or_else(|| {
            panic!(
                "respawn_existing_with_workflow_mcp must assign \
                 `ts.session = new_sess` to install the freshly-spawned \
                 session. If the assignment shape has changed, update \
                 this test's needle.\n\nFunction body:\n{}",
                body
            )
        });
        assert!(
            kill_idx < swap_idx,
            "kill_daemon_session_if_attached(host_pool, ts) at byte {} must \
             precede `ts.session = new_sess` at byte {} — otherwise the swap \
             drops the old daemon-attached Session (detach-only Drop) BEFORE the \
             explicit kill RPC fires, leaving an orphan daemon PTY child.\n\nFunction body:\n{}",
            kill_idx, swap_idx, body
        );
    }

    /// Regression pin for the review-workflow PTY-freeze bug: the
    /// respawn must spawn the replacement under a FRESH uid, not
    /// reuse `ts.uid`. `kill_session` defers daemon registry
    /// removal to the reaper, so reusing the just-killed uid races
    /// the daemon's `start_session` collision guard (`Conflict`),
    /// the spawn fails, the `ts.session` swap never runs, and the
    /// foreground PTY is left attached to a dead session — frozen,
    /// no input. A fresh uid removes the race. The `ts.uid` field
    /// must then be re-pointed at the new uid so descendant-scope
    /// MCP auth (`ts.uid == caller_uid`) keeps matching the
    /// respawned agent's `CM_TUI_SESSION_ID`.
    #[test]
    fn respawn_uses_fresh_uid_not_reused_to_avoid_kill_then_reuse_collision() {
        let body = respawn_body();
        let mint_idx = body.find("let new_uid = new_session_uid();").unwrap_or_else(|| {
            panic!(
                "respawn_existing_with_workflow_mcp must mint a fresh uid \
                 (`let new_uid = new_session_uid();`) for the replacement \
                 session instead of reusing `ts.uid`. Reusing the just-killed \
                 uid races the daemon's start_session collision guard and \
                 freezes the foreground PTY.\n\nFunction body:\n{}",
                body
            )
        });
        let kill_idx = body
            .find("kill_daemon_session_if_attached(host_pool, ts)")
            .expect("kill call must exist");
        assert!(
            mint_idx < kill_idx,
            "the fresh uid must be minted before the kill so kill-uid and \
             start-uid are guaranteed distinct",
        );
        assert!(
            body.contains("&new_uid,"),
            "the daemon spawn must be passed `&new_uid` (the fresh uid), not \
             `&ts.uid`, so the new daemon session registers under a uid the \
             just-killed one can't collide with.\n\nFunction body:\n{}",
            body,
        );
        assert!(
            !body.contains("&ts.uid,"),
            "respawn must NOT pass `&ts.uid` to the spawn — that reintroduces \
             the kill-then-reuse collision freeze.\n\nFunction body:\n{}",
            body,
        );
        let adopt_idx = body.find("ts.uid = new_uid;").unwrap_or_else(|| {
            panic!(
                "after swapping `ts.session`, respawn must adopt the fresh uid \
                 (`ts.uid = new_uid;`) so descendant-scope MCP auth \
                 (`ts.uid == caller_uid`) matches the respawned agent's \
                 CM_TUI_SESSION_ID.\n\nFunction body:\n{}",
                body
            )
        });
        let swap_idx = body
            .find("ts.session = new_sess")
            .expect("swap must exist");
        assert!(
            swap_idx < adopt_idx,
            "`ts.uid = new_uid` must come AFTER `ts.session = new_sess` — the \
             uid is adopted only once the swap succeeds (error paths keep the \
             old identity for the still-old session).",
        );
    }
}
