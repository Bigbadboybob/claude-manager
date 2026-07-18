//! Input layer: InputMode state machine, per-mode handle_* functions, SubmitAction dispatch, form Mut bags, yank/clipboard.

use super::*;

/// Modal input state.
pub(super) enum InputMode {
    /// Normal operation — keys go to terminal or app navigation.
    Normal,
    /// Typing a name/label for a new local session.
    NewSession {
        label_text: String,
        branch_text: String,
        idle_timeout_text: String,
        repo_url: String,
        /// `Some(snapshot_name)` when the session should be cloned from
        /// that agent-memory snapshot. Filled in via the picker invoked
        /// from field 4. See chunk 5 in DESIGN_AGENT_MEMORIES.md.
        seed_from: Option<String>,
        /// Host the new workspace is created on. Seeded from `active_host`
        /// at form-open so an operator who doesn't touch the field gets
        /// today's behavior; cycled with ←/→ on field 5. Host is fixed at
        /// creation — this picker makes the per-A-n choice explicit instead
        /// of forcing an `A-H` to pre-aim the global `active_host`.
        host_id: cm_daemon::host_id::HostId,
        /// 0 = repo (←/→ to cycle), 1 = name, 2 = branch, 3 = idle timeout,
        /// 4 = seed-from (Enter opens snapshot picker, Esc clears),
        /// 5 = host (←/→ to cycle the configured hosts)
        active_field: u8,
    },
    /// Picking a session type to add to a workspace.
    ///
    /// Target identified by stable `workspace_id` (NOT positional index).
    /// Backend events can fire `reconcile_tasks` while the form is open,
    /// which sorts `self.workspaces`. A stored index would silently point
    /// at the wrong workspace by submit time — the seeded clone could
    /// land in the wrong worktree. See chunk-3's SaveSnapshot fix for
    /// the same pattern.
    NewTerminalSession {
        workspace_id: String,
        session_type: String,
        /// Task scope inherited from the cursor when A-s was pressed. None =
        /// workspace-level session (no task subheader).
        task_id: Option<String>,
        /// `Some(snapshot_name)` when the session should be cloned from
        /// that agent-memory snapshot. Cleared whenever `session_type`
        /// changes — the picker filtered to a specific engine, and we
        /// can't carry a Codex snapshot across to a Claude session.
        seed_from: Option<String>,
        /// 0 = session type (j/k cycles), 1 = seed-from (Enter picks).
        active_field: u8,
    },
    /// Editing session settings.
    SessionSettings {
        ws_index: usize,
        session_index: usize,
        name: String,
        idle_timeout: String,
        burst_threshold: String,
        hidden: bool,
        notify_on_idle: bool,
        /// Global-permissions grant. When on, this session's MCP agent
        /// can prompt/read/control ANY session, not just descendants.
        global_perms: bool,
        /// Accent color for this session's sidebar row — a `USER_COLORS`
        /// name, cycled with Space/←/→. `None` = inherit workspace color.
        color: Option<String>,
        /// Read-only provenance: name of the agent-memory snapshot this
        /// session was cloned from, if any. Surfaced at the bottom of the
        /// dialog when `Some`. Not editable from settings.
        seeded_from_snapshot: Option<String>,
        /// 0 = name, 1 = idle timeout, 2 = burst threshold, 3 = hidden,
        /// 4 = notify on idle, 5 = global perms, 6 = color
        active_field: u8,
    },
    /// Workspace settings: display label (branch and worktree path stay
    /// the same), accent color, and the pinned flag.
    WorkspaceSettings {
        ws_index: usize,
        name: String,
        /// Accent color (`USER_COLORS` name); cascades to the workspace's
        /// sessions unless they set their own.
        color: Option<String>,
        /// Pinned workspaces sort to the top of the sidebar.
        pinned: bool,
        /// 0 = name, 1 = color, 2 = pinned
        active_field: u8,
    },
    /// Save the focused session as a named agent-memory snapshot. Opened
    /// via A-b in Sessions view; see `DESIGN_AGENT_MEMORIES.md` (chunk 3).
    ///
    /// Target identified by stable IDs (workspace id + session uid) — NOT
    /// by indices. Backend events can reorder `App.workspaces` while the
    /// modal is open, so indices would silently point at the wrong session
    /// (or fall out of bounds) by submit time.
    ///
    /// `error` is set when a submit fails so the modal can stay open and
    /// surface the reason inline (e.g. name conflict, no transcript yet).
    SaveSnapshot {
        workspace_id: String,
        session_uid: String,
        name_text: String,
        description_text: String,
        /// 0 = name, 1 = description.
        active_field: u8,
        error: Option<String>,
    },
    /// Browse / detail / rename / delete the agent-memory snapshot catalog.
    /// Opened via A-z in Sessions view. `mode` tracks the active sub-view
    /// (the catalog modal stays one InputMode variant rather than a family
    /// of variants, so the snapshots list is loaded once on open and shared
    /// across all sub-modes — list re-reads only happen after a rename or
    /// delete commits).
    ///
    /// `is_picker` toggles select-and-return semantics for chunk 5's
    /// seed-from-snapshot field: when true, Enter on a row would return
    /// the snapshot name to the caller instead of opening Detail, and
    /// rename / delete are disabled. Currently unused — chunk 4 only opens
    /// the catalog in browse mode (`is_picker = false`).
    SnapshotCatalog {
        snapshots: Vec<agent_memory::Snapshot>,
        selected: usize,
        mode: CatalogMode,
        /// `Some` when the catalog was opened in picker mode from a parent
        /// form — Enter on a row submits `SnapshotPicked` and the dispatch
        /// returns to that form with `seed_from` set. `None` for the
        /// stand-alone catalog opened via A-z.
        picker_target: Option<PickerTarget>,
        /// Transient error/status string surfaced at the bottom of the
        /// catalog (e.g. "Delete failed: …"). Cleared on the next user
        /// input so it doesn't linger across mode transitions.
        status_msg: Option<String>,
    },
    /// Task settings: rename (updates the `name` field via the planning
    /// API and the local TaskEntry so the sidebar subheader refreshes)
    /// plus the accent color (stored TUI-side in `App::task_colors`).
    TaskSettings {
        task_id: String,
        name: String,
        /// Accent color (`USER_COLORS` name) for the sidebar task header.
        color: Option<String>,
        /// 0 = name, 1 = color
        active_field: u8,
    },
    /// Picking which workflow to launch when more than one is defined.
    WorkflowPicker {
        // Stable workspace id captured at modal-open. Re-resolved to a current
        // index at use time (`resolve_workspace_by_id`) — a backend tick can
        // reorder/remove `workspaces` while the modal is open, so a frozen raw
        // index would target the wrong workspace.
        ws_id: String,
        focused_si: Option<usize>,
        names: Vec<String>,
        selected: usize,
        /// migrate-tui-local Issue I: the task scope the launch
        /// descends from, captured at A-f time from
        /// `Cursor::Task { task_id, .. }`. Threaded through to
        /// `LaunchWorkflow` so the controller's
        /// `spawn_workflow_session` records the daemon-side
        /// `DaemonSession.task_id` at spawn time. `None` for
        /// workspace-scope launches (cursor wasn't on a task);
        /// the existing-slot inheritance fallback still applies.
        cursor_task_id: Option<String>,
    },
    /// Confirming launch of a workflow on a workspace.
    WorkflowLaunchConfirm {
        // Stable workspace id (see `WorkflowPicker::ws_id`) — re-resolved to a
        // current index at draw + launch time.
        ws_id: String,
        workflow_name: String,
        /// One slot per role, in presentation order.
        slots: Vec<WorkflowSlotChoice>,
        /// Index of the slot whose option can currently be cycled. When equal
        /// to `slots.len()`, focus is on the goal text field.
        active_slot: usize,
        /// Optional run-level goal typed by the user. Persists on the run so
        /// templates' `{{ goal }}` expands to it across restarts.
        goal: String,
        /// migrate-tui-local Issue I: launching task scope, see
        /// `WorkflowPicker::cursor_task_id`.
        cursor_task_id: Option<String>,
    },
    /// Showing a workflow run's history.
    WorkflowHistory {
        run_id: String,
    },
    /// Picker over past workspaces (closed or all-tasks-done) so the user
    /// can reopen one without cluttering the sidebar. Opened via A-O from
    /// Sessions view. Carries the candidate list up-front instead of
    /// recomputing on every input event.
    PastWorkspacePicker {
        candidates: Vec<PastCandidate>,
        selected: usize,
    },
    /// Fuzzy-find palette over sessions + workspace headers (A-p,
    /// Sessions view). Candidate rows are snapshotted at open; each
    /// carries a stable id (`PaletteTarget`) resolved against the live
    /// rows at submit time, so a mid-modal reconcile can't misjump.
    SessionPalette {
        candidates: Vec<PaletteCandidate>,
        query: String,
        selected: usize,
    },
    /// Read-only info overlay for the focused row (A-i, Sessions view):
    /// bound-task detail (name/status/prompt) or the workspace fallback.
    /// `lines` are assembled once at open. `max_scroll` is written back
    /// by the draw each frame (the only place the final wrap width is
    /// known) and clamps `scroll` in the handler.
    TaskPeek {
        lines: Vec<PeekLine>,
        scroll: u16,
        max_scroll: u16,
    },
    /// Generic y/N confirmation overlay. The action runs only on `y`/`Y`/Enter;
    /// `n`/`N`/Esc cancels. Used to gate destructive keys (A-d, A-x).
    Confirm {
        prompt: String,
        action: ConfirmAction,
    },
}

/// Snapshot of a past workspace surfaced in the A-O picker. `worktree_exists`
/// is checked at modal-open time so the row can be greyed/disabled when the
/// directory has been removed since close.
#[derive(Clone, Debug)]
pub struct PastCandidate {
    pub ws_id: String,
    pub display: String,
    pub worktree_path: Option<std::path::PathBuf>,
    pub worktree_exists: bool,
    /// Latest tombstone `exited_at` if any — used to sort most-recent first.
    pub last_exited_at: f64,
}

#[derive(Clone, Debug)]
pub enum ConfirmAction {
    MarkDone,
    Delete,
    StopWorkflow { run_id: String },
    /// Y/n prompt that follows A-O reopen when the workspace had any
    /// tombstoned sessions. Y respawns one session per tombstone (claude
    /// /codex resume via `--resume`, bash starts fresh in the worktree).
    RestoreTombstones { ws_id: String },
}

/// Per-role slot in the launch modal. The user cycles through `options` with
/// left/right; `option_index` points at the currently-selected one.
#[derive(Clone, Debug)]
pub struct WorkflowSlotChoice {
    pub role: String,
    pub options: Vec<WorkflowSlotSource>,
    pub option_index: usize,
}

impl WorkflowSlotChoice {
    pub fn source(&self) -> &WorkflowSlotSource {
        &self.options[self.option_index]
    }
    pub fn cycle(&mut self, delta: i32) {
        if self.options.is_empty() {
            return;
        }
        let len = self.options.len() as i32;
        let next = ((self.option_index as i32 + delta).rem_euclid(len)) as usize;
        self.option_index = next;
    }
}

#[derive(Clone, Debug)]
pub enum WorkflowSlotSource {
    /// Use an existing session on the workspace, by index within `ws.sessions`.
    Existing(usize),
    /// Spawn a new session with the given engine.
    New(Engine),
}

/// Phase 3 (doc/existing-session-binding.md): map a launch modal's slots to the
/// `role -> daemon_session_uid` bindings forwarded to `start_workflow`. A slot
/// whose selected source is `Existing(si)` contributes `role -> uid` when
/// `session_uids[si]` is `Some` (a daemon-owned session); `New` slots — and any
/// `Existing(si)` pointing at a session that has no daemon uid or no longer
/// exists (out-of-range after a mid-modal reconcile) — contribute nothing, so
/// the daemon fresh-spawns those roles. `session_uids[i]` is
/// `ws.sessions[i].session.daemon_session_uid`: the DAEMON session uid, never
/// the local `TerminalSession` UI handle.
pub(crate) fn slots_to_role_sessions(
    slots: &[WorkflowSlotChoice],
    session_uids: &[Option<String>],
) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    for slot in slots {
        if let WorkflowSlotSource::Existing(si) = slot.source() {
            if let Some(uid) = session_uids.get(*si).and_then(|u| u.clone()) {
                map.insert(slot.role.clone(), uid);
            }
        }
    }
    map
}

/// Map a launch modal's slots to the `role -> engine` overrides forwarded to
/// `start_workflow`. A slot whose selected source is `New(engine)` contributes
/// `role -> "claude-code"|"codex"` ONLY when the chosen engine differs from the
/// role's TOML default (always `options[0]`, the Enter default) — so a launch
/// where every fresh slot keeps its default is byte-identical on the wire to the
/// pre-engine-choice call (the daemon's `role_engines` param is
/// `#[serde(default)]`). `Existing` slots contribute nothing: a bound session
/// keeps its own engine, which the daemon never overrides.
pub(crate) fn slots_to_role_engines(
    slots: &[WorkflowSlotChoice],
) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    for slot in slots {
        if let WorkflowSlotSource::New(engine) = slot.source() {
            let default_engine = match slot.options.first() {
                Some(WorkflowSlotSource::New(e)) => Some(e),
                _ => None,
            };
            if default_engine != Some(engine) {
                let wire = match engine {
                    Engine::ClaudeCode => "claude-code",
                    Engine::Codex => "codex",
                };
                map.insert(slot.role.clone(), wire.to_string());
            }
        }
    }
    map
}

/// Read-only context handlers may need to make decisions. The whole-App
/// reference is too coarse — only fields actually needed by some handler
/// land here. The dispatcher builds this fresh per call.
pub(crate) struct InputCtx<'a> {
    /// Repo URLs in the user's config, sorted by repo name. Used by
    /// `handle_new_session` to cycle the repo picker (←/→).
    pub repo_urls: &'a [String],
    /// Configured host ids (local + any `~/.cm/hosts.toml` entries), in
    /// config order. Used by `handle_new_session` to cycle the host picker
    /// (←/→) on field 5. Empty in contexts where host selection doesn't
    /// apply — cycling is then a no-op.
    pub host_ids: &'a [cm_daemon::host_id::HostId],
}

/// Post-condition signal from a per-mode handler back to the dispatcher.
/// Handlers stay pure-ish: they mutate their own mode payload in place
/// (typing characters, cycling fields) and surface app-level transitions
/// through this enum.
#[derive(Debug, Clone)]
pub(crate) enum InputOutcome {
    /// Event handled by the modal; no app-level state change.
    Consumed,
    /// Event was not for any modal — fall through to terminal/app keys.
    /// Only `InputMode::Normal` returns this.
    Ignored,
    /// Reset `input_mode` to `Normal`. Used by Esc / explicit cancels.
    Cancel,
    /// Reset `input_mode` to `Normal` AND fire a side effect.
    Submit(SubmitAction),
    /// Keep the modal OPEN but surface a status message — inline validation
    /// feedback (e.g. a required field left empty) so a rejected keystroke
    /// isn't a silent no-op.
    Status(String),
}

/// Side effects requested by a `Submit` outcome. The dispatcher matches
/// on this and invokes the relevant `App` method; the handlers
/// themselves never see `&mut App`.
#[derive(Debug, Clone)]
pub(crate) enum SubmitAction {
    /// Submit attempted but the inputs produced no work to do (e.g.
    /// empty workspace name, no workflow selected). Modal still closes.
    None,
    CreateLocalSession {
        repo_url: String,
        label: String,
        branch: Option<String>,
        idle_timeout_secs: u16,
        seed_from: Option<String>,
        /// `true` when the branch field held the `.` sentinel: launch
        /// in-place in the main repo (no worktree, no branch). `branch`
        /// is `None` in that case.
        in_place: bool,
        /// Host chosen on the A-n form (defaults to `active_host`). The
        /// create path pins the new workspace to THIS host — local →
        /// existing local create path, remote → `create_remote_session`.
        host_id: cm_daemon::host_id::HostId,
    },
    SpawnSessionOnWorkspace {
        workspace_id: String,
        session_type: String,
        task_id: Option<String>,
        seed_from: Option<String>,
    },
    /// Open the snapshot catalog in picker mode from the A-n form. Carries
    /// the form state so the catalog can re-open the form (with seed_from
    /// set on pick or unchanged on cancel) on submit / cancel.
    OpenSnapshotPickerForNewSession {
        label_text: String,
        branch_text: String,
        idle_timeout_text: String,
        repo_url: String,
        existing_seed_from: Option<String>,
        /// Carried through the picker round-trip so the chosen host
        /// survives a snapshot pick/cancel (same rationale as the typed
        /// text fields above).
        host_id: cm_daemon::host_id::HostId,
    },
    /// Open the snapshot catalog in picker mode from the A-s form.
    OpenSnapshotPickerForNewTerminalSession {
        workspace_id: String,
        session_type: String,
        task_id: Option<String>,
        existing_seed_from: Option<String>,
    },
    SaveSessionSettings {
        ws_index: usize,
        session_index: usize,
        name: String,
        idle_timeout: u16,
        burst_threshold: u16,
        hidden: bool,
        notify_on_idle: bool,
        global_perms: bool,
        color: Option<String>,
    },
    SaveWorkspaceSettings {
        ws_index: usize,
        name: String,
        color: Option<String>,
        pinned: bool,
    },
    SaveSnapshot {
        workspace_id: String,
        session_uid: String,
        name: String,
        description: String,
    },
    /// Emitted when the catalog is opened in picker mode and the user
    /// presses Enter on a row. Chunk 4 doesn't open the catalog in picker
    /// mode anywhere — chunk 5's seed-from-snapshot field will. The arm
    /// in `apply_submit_action` is a no-op today so the variant exists
    /// to be wired up later.
    SnapshotPicked {
        name: String,
    },
    SaveTaskName {
        task_id: String,
        name: String,
        color: Option<String>,
    },
    EnterWorkflowLaunchConfirm {
        ws_id: String,
        focused_si: Option<usize>,
        workflow_name: String,
        /// migrate-tui-local Issue I: launching task scope. See
        /// `InputMode::WorkflowPicker::cursor_task_id`.
        cursor_task_id: Option<String>,
    },
    LaunchWorkflow {
        ws_id: String,
        workflow_name: String,
        slots: Vec<WorkflowSlotChoice>,
        goal: Option<String>,
        /// migrate-tui-local Issue I: launching task scope —
        /// forwarded into `App::launch_workflow` so the daemon
        /// records `DaemonSession.task_id` for fresh workflow
        /// participants. `None` for workspace-scope launches.
        cursor_task_id: Option<String>,
    },
    MarkActiveDone,
    DeleteActive,
    StopWorkflow {
        run_id: String,
    },
    /// Picker chose a past workspace to reopen.
    ReopenPastWorkspace {
        ws_id: String,
    },
    /// A-p palette chose a row to jump the cursor to. Resolved against
    /// the live workspaces/sessions at apply time.
    PaletteJump {
        target: PaletteTarget,
    },
    /// Confirmed Y on the "Restore N closed sessions?" prompt.
    RestoreTombstones {
        ws_id: String,
    },
}

pub(crate) struct NewSessionMut<'a> {
    pub label_text: &'a mut String,
    pub branch_text: &'a mut String,
    pub idle_timeout_text: &'a mut String,
    pub repo_url: &'a mut String,
    pub seed_from: &'a mut Option<String>,
    pub host_id: &'a mut cm_daemon::host_id::HostId,
    pub active_field: &'a mut u8,
}

pub(crate) struct NewTerminalSessionMut<'a> {
    pub workspace_id: &'a str,
    pub session_type: &'a mut String,
    pub task_id: &'a Option<String>,
    pub seed_from: &'a mut Option<String>,
    pub active_field: &'a mut u8,
}

pub(crate) struct SessionSettingsMut<'a> {
    pub ws_index: usize,
    pub session_index: usize,
    pub name: &'a mut String,
    pub idle_timeout: &'a mut String,
    pub burst_threshold: &'a mut String,
    pub hidden: &'a mut bool,
    pub notify_on_idle: &'a mut bool,
    pub global_perms: &'a mut bool,
    pub color: &'a mut Option<String>,
    pub active_field: &'a mut u8,
}

pub(crate) struct WorkspaceSettingsMut<'a> {
    pub ws_index: usize,
    pub name: &'a mut String,
    pub color: &'a mut Option<String>,
    pub pinned: &'a mut bool,
    pub active_field: &'a mut u8,
}

pub(crate) struct SaveSnapshotMut<'a> {
    pub workspace_id: &'a str,
    pub session_uid: &'a str,
    pub name_text: &'a mut String,
    pub description_text: &'a mut String,
    pub active_field: &'a mut u8,
    /// Cleared on the next user input so the previous error doesn't
    /// linger after the user starts correcting the form.
    pub error: &'a mut Option<String>,
}

/// What to return to after the catalog is opened in picker mode. Carries
/// the form state captured at picker-open time so that Esc (cancel) and
/// Enter (select) can both re-open the parent form with the user's typed
/// input intact. See chunk 5 in DESIGN_AGENT_MEMORIES.md.
///
/// `existing_seed_from` is the form's `seed_from` value at the moment the
/// picker was opened. Picker-cancel restores it (don't silently wipe a
/// previously-picked snapshot just because the user opened the picker to
/// look); picker-select overwrites it with the chosen name.
#[derive(Debug, Clone)]
pub enum PickerTarget {
    NewSession {
        label_text: String,
        branch_text: String,
        idle_timeout_text: String,
        repo_url: String,
        existing_seed_from: Option<String>,
        /// The host chosen on the form when the picker was opened —
        /// preserved verbatim so a pick/cancel round-trip doesn't reset
        /// the operator's host choice back to the default.
        host_id: cm_daemon::host_id::HostId,
    },
    NewTerminalSession {
        workspace_id: String,
        session_type: String,
        task_id: Option<String>,
        existing_seed_from: Option<String>,
    },
}

/// Compute the next `InputMode` and optional status toast for an
/// `open_snapshot_catalog` call given the result of
/// `agent_memory::list()`. Pure function so the failure path —
/// especially the recovery that re-opens the parent form when called
/// from a picker — is unit-testable without standing up an `App`.
///
/// - `Ok(snapshots)`: filter by the picker target's engine (if any),
///   build the `SnapshotCatalog` input mode. No status message.
/// - `Err(e)` with `picker_target = Some(t)`: re-open the parent form
///   via `rebuild_form_from_picker` so the user's typed input survives
///   the list failure. Status message describes the error.
/// - `Err(e)` with `picker_target = None`: drop back to `Normal` and
///   surface the error.
fn catalog_open_outcome(
    list_result: Result<Vec<agent_memory::Snapshot>, agent_memory::SnapshotError>,
    picker_target: Option<PickerTarget>,
) -> (InputMode, Option<String>) {
    match list_result {
        Ok(snapshots) => {
            let filtered = match &picker_target {
                Some(t) => {
                    let want = picker_target_engine(t);
                    snapshots
                        .into_iter()
                        .filter(|s| Some(&s.manifest.engine) == want.as_ref())
                        .collect()
                }
                None => snapshots,
            };
            (
                InputMode::SnapshotCatalog {
                    snapshots: filtered,
                    selected: 0,
                    mode: CatalogMode::Browse,
                    picker_target,
                    status_msg: None,
                },
                None,
            )
        }
        Err(e) => {
            let msg = format!("Could not list snapshots: {e}");
            let mode = match picker_target {
                Some(t) => rebuild_form_from_picker(t, None),
                None => InputMode::Normal,
            };
            (mode, Some(msg))
        }
    }
}

/// Rebuild the parent input form after a snapshot picker round-trip.
///
/// - `name = Some(picked)`: overwrite `seed_from` with the picked
///   snapshot (used by `SubmitAction::SnapshotPicked`).
/// - `name = None`: preserve `existing_seed_from` from the target so
///   picker-cancel doesn't silently wipe a previously-picked snapshot
///   (the user opening the picker to look at options should be a no-op
///   if they bail).
///
/// Extracted as a free function so the round-trip is unit-testable
/// without constructing an `App`.
fn rebuild_form_from_picker(target: PickerTarget, name: Option<String>) -> InputMode {
    match target {
        PickerTarget::NewSession {
            label_text,
            branch_text,
            idle_timeout_text,
            repo_url,
            existing_seed_from,
            host_id,
        } => InputMode::NewSession {
            label_text,
            branch_text,
            idle_timeout_text,
            repo_url,
            seed_from: name.or(existing_seed_from),
            host_id,
            // Keep the picker field selected so the user sees the
            // result of their pick (or non-pick) land in context.
            active_field: 4,
        },
        PickerTarget::NewTerminalSession {
            workspace_id,
            session_type,
            task_id,
            existing_seed_from,
        } => InputMode::NewTerminalSession {
            workspace_id,
            session_type,
            task_id,
            seed_from: name.or(existing_seed_from),
            active_field: 1,
        },
    }
}

/// Engine constraint the catalog enforces when opened in picker mode.
/// `NewSession` always spawns a Claude Code session, so the filter is
/// always `ClaudeCode`. `NewTerminalSession` filters to whichever engine
/// the user selected on the form (no filter for bash — that path
/// doesn't reach the picker).
fn picker_target_engine(t: &PickerTarget) -> Option<Engine> {
    match t {
        PickerTarget::NewSession { .. } => Some(Engine::ClaudeCode),
        PickerTarget::NewTerminalSession { session_type, .. } => {
            match session_type.as_str() {
                "claude" => Some(Engine::ClaudeCode),
                "codex" => Some(Engine::Codex),
                _ => None,
            }
        }
    }
}

/// Cheap pre-flight check that the named snapshot loads cleanly. Used
/// by `create_local_session` to fail-fast BEFORE creating a git worktree
/// — otherwise a seeded A-n with a bad snapshot name would leave an
/// orphan worktree + branch on disk and block retries on the same label.
///
/// The later `clone_snapshot_for_spawn` re-loads the snapshot to do the
/// actual materialization; this just rejects names that would never
/// succeed. Free function so tests can drive it against an isolated
/// HOME without standing up an `App`.
pub(super) fn validate_seed_loadable(name: &str) -> std::result::Result<(), String> {
    agent_memory::load(name).map(|_| ()).map_err(|e| format!("Snapshot load failed: {e}"))
}

/// Resolve a stable `workspace_id` to its current position in
/// `App.workspaces`. Returns `None` if the workspace has been removed
/// since the id was captured (e.g. user deleted it, or reconcile_tasks
/// dropped a cloud workspace). Free function so it's unit-testable
/// against a hand-rolled `&[Workspace]`.
pub(super) fn resolve_workspace_by_id(workspaces: &[Workspace], workspace_id: &str) -> Option<usize> {
    workspaces.iter().position(|w| w.id == workspace_id)
}

/// Snapshot-catalog sub-mode. Tracks what the user is currently doing
/// inside the catalog modal — all sub-modes share the same list of
/// snapshots and selection cursor.
#[derive(Debug, Clone)]
pub enum CatalogMode {
    /// Default list view. j/k navigate, Enter→Detail, r→Rename, d→ConfirmDelete.
    Browse,
    /// Read-only manifest + transcript head/tail preview. Esc/Enter→Browse.
    /// `head` and `tail` are loaded once on transition and cached so
    /// rendering doesn't hit disk every frame.
    Detail {
        head: Vec<String>,
        tail: Vec<String>,
    },
    /// In-line rename of the selected snapshot. Enter commits via
    /// `agent_memory::rename`; Esc returns to Browse without changes.
    Rename {
        text: String,
        error: Option<String>,
    },
    /// "Delete snapshot `<name>`? (y/n)" confirm prompt. y/Enter commits
    /// via `agent_memory::delete`; n/Esc returns to Browse.
    ConfirmDelete,
}

pub(crate) struct SnapshotCatalogMut<'a> {
    pub snapshots: &'a mut Vec<agent_memory::Snapshot>,
    pub selected: &'a mut usize,
    pub mode: &'a mut CatalogMode,
    pub picker_target: Option<&'a PickerTarget>,
    pub status_msg: &'a mut Option<String>,
}

impl SnapshotCatalogMut<'_> {
    fn is_picker(&self) -> bool {
        self.picker_target.is_some()
    }
}

pub(crate) struct TaskSettingsMut<'a> {
    pub task_id: &'a str,
    pub name: &'a mut String,
    pub color: &'a mut Option<String>,
    pub active_field: &'a mut u8,
}

pub(crate) struct WorkflowLaunchConfirmMut<'a> {
    pub ws_id: &'a str,
    pub workflow_name: &'a str,
    pub slots: &'a mut Vec<WorkflowSlotChoice>,
    pub active_slot: &'a mut usize,
    pub goal: &'a mut String,
    /// migrate-tui-local Issue I: launching task scope captured
    /// at `open_workflow_launch` time. Forwarded into the
    /// `LaunchWorkflow` submit action so `App::launch_workflow`
    /// can thread it through to the daemon.
    pub cursor_task_id: Option<&'a str>,
}

pub(crate) struct WorkflowPickerMut<'a> {
    pub ws_id: &'a str,
    pub focused_si: Option<usize>,
    pub names: &'a [String],
    pub selected: &'a mut usize,
    /// migrate-tui-local Issue I: launching task scope. See
    /// `WorkflowLaunchConfirmMut::cursor_task_id`.
    pub cursor_task_id: Option<&'a str>,
}

pub(crate) fn handle_new_session(
    state: NewSessionMut<'_>,
    ctx: InputCtx<'_>,
    event: &CrosstermEvent,
) -> InputOutcome {
    let CrosstermEvent::Key(key) = event else {
        return InputOutcome::Consumed;
    };
    const FIELD_COUNT: u8 = 6; // repo, label, branch, idle, seed-from, host
    match key.code {
        KeyCode::Esc => {
            // Esc on the seed-from field with a value clears the
            // selection instead of cancelling the whole form — matches
            // the design: "Esc clears the selection back to [none]".
            if *state.active_field == 4 && state.seed_from.is_some() {
                *state.seed_from = None;
                InputOutcome::Consumed
            } else {
                InputOutcome::Cancel
            }
        }
        KeyCode::Tab => {
            *state.active_field = (*state.active_field + 1) % FIELD_COUNT;
            InputOutcome::Consumed
        }
        KeyCode::BackTab => {
            *state.active_field = if *state.active_field == 0 {
                FIELD_COUNT - 1
            } else {
                *state.active_field - 1
            };
            InputOutcome::Consumed
        }
        KeyCode::Left | KeyCode::Right if *state.active_field == 0 => {
            if let Some(cur) = ctx.repo_urls.iter().position(|u| u == state.repo_url) {
                let n = ctx.repo_urls.len();
                let next = if matches!(key.code, KeyCode::Right) {
                    (cur + 1) % n
                } else {
                    (cur + n - 1) % n
                };
                *state.repo_url = ctx.repo_urls[next].clone();
            }
            InputOutcome::Consumed
        }
        KeyCode::Left | KeyCode::Right if *state.active_field == 5 => {
            // Host picker: cycle through the configured hosts (local +
            // any hosts.toml entries). Mirrors the repo picker at field 0.
            // Empty list (no ctx) or an unknown current host falls back to
            // index 0 so a cycle still lands somewhere sensible.
            if !ctx.host_ids.is_empty() {
                let cur = ctx
                    .host_ids
                    .iter()
                    .position(|h| h == state.host_id)
                    .unwrap_or(0);
                let n = ctx.host_ids.len();
                let next = if matches!(key.code, KeyCode::Right) {
                    (cur + 1) % n
                } else {
                    (cur + n - 1) % n
                };
                *state.host_id = ctx.host_ids[next].clone();
            }
            InputOutcome::Consumed
        }
        KeyCode::Enter if *state.active_field == 4 => {
            // Open the snapshot catalog in picker mode. The dispatcher
            // stashes the form state on the submit action so it can
            // re-open this exact form after the user picks (or cancels).
            // existing_seed_from is captured so picker-cancel doesn't
            // wipe a previously-picked snapshot.
            InputOutcome::Submit(SubmitAction::OpenSnapshotPickerForNewSession {
                label_text: state.label_text.clone(),
                branch_text: state.branch_text.clone(),
                idle_timeout_text: state.idle_timeout_text.clone(),
                repo_url: state.repo_url.clone(),
                existing_seed_from: state.seed_from.clone(),
                host_id: state.host_id.clone(),
            })
        }
        KeyCode::Enter => {
            if !state.label_text.trim().is_empty() {
                // Branch field semantics:
                //   "."   → in-place: run in the main repo, no worktree/branch.
                //   ""    → new worktree, branch `cm/<slug>` from HEAD.
                //   other → new worktree from that base branch.
                let trimmed = state.branch_text.trim();
                let in_place = trimmed == ".";
                let branch = if trimmed.is_empty() || in_place {
                    None
                } else {
                    Some(state.branch_text.clone())
                };
                let timeout = state
                    .idle_timeout_text
                    .parse::<u16>()
                    .unwrap_or(DEFAULT_IDLE_TIMEOUT_SECS);
                InputOutcome::Submit(SubmitAction::CreateLocalSession {
                    repo_url: state.repo_url.clone(),
                    label: state.label_text.clone(),
                    branch,
                    idle_timeout_secs: timeout,
                    seed_from: state.seed_from.clone(),
                    in_place,
                    host_id: state.host_id.clone(),
                })
            } else {
                // Empty Name field: pre-fix this was a silent no-op (Enter did
                // "nothing"). Surface why instead.
                InputOutcome::Status(
                    "Name is required — type a session name (Tab to the Name field), then Enter".into(),
                )
            }
        }
        KeyCode::Backspace => {
            match *state.active_field {
                1 => {
                    state.label_text.pop();
                }
                2 => {
                    state.branch_text.pop();
                }
                3 => {
                    state.idle_timeout_text.pop();
                }
                _ => {}
            }
            InputOutcome::Consumed
        }
        KeyCode::Char(c) => {
            match *state.active_field {
                1 => state.label_text.push(c),
                2 => state.branch_text.push(c),
                3 => {
                    if c.is_ascii_digit() {
                        state.idle_timeout_text.push(c);
                    }
                }
                _ => {}
            }
            InputOutcome::Consumed
        }
        _ => InputOutcome::Consumed,
    }
}

pub(crate) fn handle_new_terminal_session(
    state: NewTerminalSessionMut<'_>,
    _ctx: InputCtx<'_>,
    event: &CrosstermEvent,
) -> InputOutcome {
    let CrosstermEvent::Key(key) = event else {
        return InputOutcome::Consumed;
    };
    // Two fields: 0 = session type, 1 = seed-from. Tab/BackTab cycle
    // between them; j/k cycle the session type when field 0 is active
    // (within-field). j/k on the seed-from field are no-ops (would
    // conflict with the picker selection later).
    match (key.code, *state.active_field) {
        (KeyCode::Esc, 1) if state.seed_from.is_some() => {
            *state.seed_from = None;
            InputOutcome::Consumed
        }
        (KeyCode::Esc, _) => InputOutcome::Cancel,
        (KeyCode::Tab, _) | (KeyCode::BackTab, _) => {
            *state.active_field = (*state.active_field + 1) % 2;
            InputOutcome::Consumed
        }
        (KeyCode::Char('j') | KeyCode::Down, 0) => {
            *state.session_type = match state.session_type.as_str() {
                "claude" => "codex".to_string(),
                "codex" => "bash".to_string(),
                _ => "claude".to_string(),
            };
            // Engine changed — any previously-picked snapshot was
            // engine-filtered for the OLD value and no longer applies.
            *state.seed_from = None;
            InputOutcome::Consumed
        }
        (KeyCode::Char('k') | KeyCode::Up, 0) => {
            *state.session_type = match state.session_type.as_str() {
                "claude" => "bash".to_string(),
                "bash" => "codex".to_string(),
                _ => "claude".to_string(),
            };
            *state.seed_from = None;
            InputOutcome::Consumed
        }
        (KeyCode::Enter, 1) => {
            // Bash doesn't have transcripts and so isn't pickable; bounce
            // the user back to field 0 with a no-op rather than opening
            // an empty picker.
            if state.session_type == "bash" {
                return InputOutcome::Consumed;
            }
            InputOutcome::Submit(
                SubmitAction::OpenSnapshotPickerForNewTerminalSession {
                    workspace_id: state.workspace_id.to_string(),
                    session_type: state.session_type.clone(),
                    task_id: state.task_id.clone(),
                    existing_seed_from: state.seed_from.clone(),
                },
            )
        }
        (KeyCode::Enter, _) => {
            InputOutcome::Submit(SubmitAction::SpawnSessionOnWorkspace {
                workspace_id: state.workspace_id.to_string(),
                session_type: state.session_type.clone(),
                task_id: state.task_id.clone(),
                seed_from: state.seed_from.clone(),
            })
        }
        _ => InputOutcome::Consumed,
    }
}

pub(crate) fn handle_session_settings(
    state: SessionSettingsMut<'_>,
    _ctx: InputCtx<'_>,
    event: &CrosstermEvent,
) -> InputOutcome {
    let CrosstermEvent::Key(key) = event else {
        return InputOutcome::Consumed;
    };
    match key.code {
        KeyCode::Esc => InputOutcome::Cancel,
        KeyCode::Tab | KeyCode::BackTab => {
            *state.active_field = (*state.active_field + 1) % 7;
            InputOutcome::Consumed
        }
        KeyCode::Char(' ') if *state.active_field == 3 => {
            *state.hidden = !*state.hidden;
            InputOutcome::Consumed
        }
        KeyCode::Char(' ') if *state.active_field == 4 => {
            *state.notify_on_idle = !*state.notify_on_idle;
            InputOutcome::Consumed
        }
        KeyCode::Char(' ') if *state.active_field == 5 => {
            *state.global_perms = !*state.global_perms;
            InputOutcome::Consumed
        }
        KeyCode::Char(' ') | KeyCode::Right if *state.active_field == 6 => {
            *state.color = theme::cycle_user_color(state.color.as_deref(), true);
            InputOutcome::Consumed
        }
        KeyCode::Left if *state.active_field == 6 => {
            *state.color = theme::cycle_user_color(state.color.as_deref(), false);
            InputOutcome::Consumed
        }
        KeyCode::Enter => {
            let new_timeout = state
                .idle_timeout
                .parse::<u16>()
                .unwrap_or(DEFAULT_IDLE_TIMEOUT_SECS);
            let new_burst = state
                .burst_threshold
                .parse::<u16>()
                .unwrap_or(WAKEUP_BURST_THRESHOLD as u16)
                .max(1);
            InputOutcome::Submit(SubmitAction::SaveSessionSettings {
                ws_index: state.ws_index,
                session_index: state.session_index,
                name: state.name.clone(),
                idle_timeout: new_timeout,
                burst_threshold: new_burst,
                hidden: *state.hidden,
                notify_on_idle: *state.notify_on_idle,
                global_perms: *state.global_perms,
                color: state.color.clone(),
            })
        }
        KeyCode::Backspace => {
            match *state.active_field {
                0 => {
                    state.name.pop();
                }
                1 => {
                    state.idle_timeout.pop();
                }
                2 => {
                    state.burst_threshold.pop();
                }
                _ => {}
            }
            InputOutcome::Consumed
        }
        KeyCode::Char(c) => {
            match *state.active_field {
                0 => state.name.push(c),
                1 => {
                    if c.is_ascii_digit() {
                        state.idle_timeout.push(c);
                    }
                }
                2 => {
                    if c.is_ascii_digit() {
                        state.burst_threshold.push(c);
                    }
                }
                _ => {}
            }
            InputOutcome::Consumed
        }
        _ => InputOutcome::Consumed,
    }
}

pub(crate) fn handle_workspace_settings(
    state: WorkspaceSettingsMut<'_>,
    _ctx: InputCtx<'_>,
    event: &CrosstermEvent,
) -> InputOutcome {
    let CrosstermEvent::Key(key) = event else {
        return InputOutcome::Consumed;
    };
    match key.code {
        KeyCode::Esc => InputOutcome::Cancel,
        KeyCode::Tab | KeyCode::BackTab => {
            *state.active_field = (*state.active_field + 1) % 3;
            InputOutcome::Consumed
        }
        KeyCode::Char(' ') | KeyCode::Right if *state.active_field == 1 => {
            *state.color = theme::cycle_user_color(state.color.as_deref(), true);
            InputOutcome::Consumed
        }
        KeyCode::Left if *state.active_field == 1 => {
            *state.color = theme::cycle_user_color(state.color.as_deref(), false);
            InputOutcome::Consumed
        }
        KeyCode::Char(' ') if *state.active_field == 2 => {
            *state.pinned = !*state.pinned;
            InputOutcome::Consumed
        }
        KeyCode::Enter => InputOutcome::Submit(SubmitAction::SaveWorkspaceSettings {
            ws_index: state.ws_index,
            name: state.name.trim().to_string(),
            color: state.color.clone(),
            pinned: *state.pinned,
        }),
        KeyCode::Backspace if *state.active_field == 0 => {
            state.name.pop();
            InputOutcome::Consumed
        }
        KeyCode::Char(c) if *state.active_field == 0 => {
            state.name.push(c);
            InputOutcome::Consumed
        }
        _ => InputOutcome::Consumed,
    }
}

pub(crate) fn handle_snapshot_catalog(
    state: SnapshotCatalogMut<'_>,
    _ctx: InputCtx<'_>,
    event: &CrosstermEvent,
) -> InputOutcome {
    let CrosstermEvent::Key(key) = event else {
        return InputOutcome::Consumed;
    };

    // Alt+z anywhere closes the catalog (matches the open binding so it
    // toggles). Esc behaves contextually inside each sub-mode below.
    if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('z') {
        return InputOutcome::Cancel;
    }

    // Clear any transient status message ("Delete failed: …" etc.) on the
    // next keystroke so it doesn't linger across unrelated interactions.
    // Sub-handlers can re-set it for the current event if needed.
    *state.status_msg = None;

    match state.mode.clone() {
        CatalogMode::Browse => handle_catalog_browse(state, key),
        CatalogMode::Detail { .. } => handle_catalog_detail(state, key),
        CatalogMode::Rename { text, error } => {
            handle_catalog_rename(state, key, text, error)
        }
        CatalogMode::ConfirmDelete => handle_catalog_delete(state, key),
    }
}

fn handle_catalog_browse(
    state: SnapshotCatalogMut<'_>,
    key: &crossterm::event::KeyEvent,
) -> InputOutcome {
    match key.code {
        KeyCode::Esc => InputOutcome::Cancel,
        KeyCode::Down | KeyCode::Char('j') => {
            if !state.snapshots.is_empty() {
                *state.selected = (*state.selected + 1) % state.snapshots.len();
            }
            InputOutcome::Consumed
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if !state.snapshots.is_empty() {
                *state.selected = if *state.selected == 0 {
                    state.snapshots.len() - 1
                } else {
                    *state.selected - 1
                };
            }
            InputOutcome::Consumed
        }
        KeyCode::Enter => {
            let Some(snap) = state.snapshots.get(*state.selected) else {
                return InputOutcome::Consumed;
            };
            if state.is_picker() {
                return InputOutcome::Submit(SubmitAction::SnapshotPicked {
                    name: snap.name.clone(),
                });
            }
            let (head, tail) = read_transcript_head_tail(&snap.dir, 5);
            *state.mode = CatalogMode::Detail { head, tail };
            InputOutcome::Consumed
        }
        KeyCode::Char('r') if !state.is_picker() => {
            let Some(snap) = state.snapshots.get(*state.selected) else {
                return InputOutcome::Consumed;
            };
            *state.mode = CatalogMode::Rename {
                text: snap.name.clone(),
                error: None,
            };
            InputOutcome::Consumed
        }
        KeyCode::Char('d') if !state.is_picker() => {
            if state.snapshots.get(*state.selected).is_some() {
                *state.mode = CatalogMode::ConfirmDelete;
            }
            InputOutcome::Consumed
        }
        _ => InputOutcome::Consumed,
    }
}

fn handle_catalog_detail(
    state: SnapshotCatalogMut<'_>,
    key: &crossterm::event::KeyEvent,
) -> InputOutcome {
    match key.code {
        KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
            *state.mode = CatalogMode::Browse;
            InputOutcome::Consumed
        }
        _ => InputOutcome::Consumed,
    }
}

fn handle_catalog_rename(
    state: SnapshotCatalogMut<'_>,
    key: &crossterm::event::KeyEvent,
    mut text: String,
    error: Option<String>,
) -> InputOutcome {
    match key.code {
        KeyCode::Esc => {
            *state.mode = CatalogMode::Browse;
            InputOutcome::Consumed
        }
        KeyCode::Enter => {
            let Some(snap) = state.snapshots.get(*state.selected) else {
                *state.mode = CatalogMode::Browse;
                return InputOutcome::Consumed;
            };
            let old = snap.name.clone();
            let new = text.trim().to_string();
            if new == old {
                *state.mode = CatalogMode::Browse;
                return InputOutcome::Consumed;
            }
            match agent_memory::rename(&old, &new) {
                Ok(()) => match agent_memory::list() {
                    Ok(fresh) => {
                        // Move selection to the renamed entry so the
                        // cursor doesn't appear to jump arbitrarily.
                        let new_idx = fresh
                            .iter()
                            .position(|s| s.name == new)
                            .unwrap_or(0);
                        *state.snapshots = fresh;
                        *state.selected = new_idx;
                        *state.mode = CatalogMode::Browse;
                        InputOutcome::Consumed
                    }
                    Err(e) => {
                        *state.mode = CatalogMode::Rename {
                            text: new,
                            error: Some(format!("rename succeeded but reload failed: {e}")),
                        };
                        InputOutcome::Consumed
                    }
                },
                Err(e) => {
                    *state.mode = CatalogMode::Rename {
                        text: new,
                        error: Some(e.to_string()),
                    };
                    InputOutcome::Consumed
                }
            }
        }
        KeyCode::Backspace => {
            text.pop();
            *state.mode = CatalogMode::Rename { text, error: None };
            InputOutcome::Consumed
        }
        KeyCode::Char(c) => {
            // Drop the prior error so the user sees their typing land
            // before any new validation runs at submit.
            let _ = error;
            text.push(c);
            *state.mode = CatalogMode::Rename { text, error: None };
            InputOutcome::Consumed
        }
        _ => InputOutcome::Consumed,
    }
}

fn handle_catalog_delete(
    state: SnapshotCatalogMut<'_>,
    key: &crossterm::event::KeyEvent,
) -> InputOutcome {
    match key.code {
        KeyCode::Char('y') | KeyCode::Enter => {
            let Some(snap) = state.snapshots.get(*state.selected) else {
                *state.mode = CatalogMode::Browse;
                return InputOutcome::Consumed;
            };
            let name = snap.name.clone();
            match agent_memory::delete(&name) {
                Err(e) => {
                    // Keep list + selection as-is and surface the error.
                    // The previous code discarded the result and then
                    // potentially blanked the list via list().unwrap_or_default(),
                    // which silently dropped state on failures like a
                    // permission-denied rmdir.
                    *state.status_msg = Some(format!("Delete failed: {e}"));
                    *state.mode = CatalogMode::Browse;
                }
                Ok(()) => match agent_memory::list() {
                    Ok(fresh) => {
                        let len = fresh.len();
                        *state.snapshots = fresh;
                        *state.selected = if len == 0 {
                            0
                        } else {
                            (*state.selected).min(len - 1)
                        };
                        *state.mode = CatalogMode::Browse;
                    }
                    Err(e) => {
                        // Disk delete succeeded but we can't refresh the
                        // list. Keep the in-memory list intact (slightly
                        // stale) — better than blanking it. The user can
                        // close + reopen the catalog to retry the list.
                        *state.status_msg = Some(format!(
                            "Deleted, but reload failed: {e}"
                        ));
                        *state.mode = CatalogMode::Browse;
                    }
                },
            }
            InputOutcome::Consumed
        }
        KeyCode::Char('n') | KeyCode::Esc => {
            *state.mode = CatalogMode::Browse;
            InputOutcome::Consumed
        }
        _ => InputOutcome::Consumed,
    }
}

/// Read up to `n` lines from the start and the end of `transcript.jsonl`
/// inside a snapshot dir. Cached in `CatalogMode::Detail` so the read
/// happens once per transition, not every frame.
///
/// Memory is O(n) regardless of file size: head is a `Vec<String>` capped at
/// `n`, tail is a `VecDeque<String>` ring buffer of capacity `n` that the
/// rest of the iterator drains into. Earlier implementation slurped the
/// whole transcript into memory just to take 5 lines from each end —
/// for multi-MB transcripts that stalled the UI on Detail open.
fn read_transcript_head_tail(
    snapshot_dir: &Path,
    n: usize,
) -> (Vec<String>, Vec<String>) {
    use std::collections::VecDeque;
    use std::io::{BufRead, BufReader};

    let path = snapshot_dir.join("transcript.jsonl");
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return (Vec::new(), Vec::new()),
    };
    let reader = BufReader::new(file);

    let mut head: Vec<String> = Vec::with_capacity(n);
    let mut tail: VecDeque<String> = VecDeque::with_capacity(n.saturating_add(1));

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            // Skip unreadable lines (e.g. invalid UTF-8 in the middle) but
            // keep the loop going so we still get a usable head/tail.
            Err(_) => continue,
        };
        if head.len() < n {
            head.push(line);
            continue;
        }
        if n == 0 {
            break;
        }
        if tail.len() == n {
            tail.pop_front();
        }
        tail.push_back(line);
    }

    (head, tail.into_iter().collect())
}

/// Build the body lines of the catalog Rename overlay. Pure function so
/// unit tests can drive it without spinning up a Terminal/TestBackend.
/// Both `text` and `error` (validation messages can quote control bytes)
/// are sanitized before being placed into spans.
pub(super) fn rename_overlay_lines(text: &str, error: Option<&str>) -> Vec<Line<'static>> {
    let dim = Style::default().fg(theme::DIM);
    let white = Style::default().fg(theme::TEXT);
    let red = Style::default().fg(theme::ERROR);

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("New name: ", dim),
            // Defensive: paste into the buffer could carry control
            // bytes — sanitize on render so the rename modal can't be
            // weaponized via clipboard injection.
            Span::styled(sanitize_for_display(text), white),
            Span::styled("\u{2588}", white),
        ]),
        Line::from(""),
    ];
    if let Some(msg) = error {
        // Validation errors (`validate_name`) quote the offending
        // character — including potentially an ESC. Sanitize on render
        // so the error message can't drive the terminal.
        lines.push(Line::from(Span::styled(sanitize_for_display(msg), red)));
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled(
        "Enter rename \u{00b7} Esc cancel",
        dim,
    )));
    lines
}

/// Compute the visible row range for a scrollable list given the current
/// selection. Keeps `selected` on-screen by centering it within the window
/// when possible and clamping to the top/bottom edges otherwise. Returns
/// `(start, end)` such that `start <= selected < end` and `end - start <=
/// visible` and `end <= total`. Both bounds are 0 when there's nothing to
/// show.
pub(super) fn visible_range(selected: usize, total: usize, visible: usize) -> (usize, usize) {
    if total == 0 || visible == 0 {
        return (0, 0);
    }
    let visible = visible.min(total);
    let half = visible / 2;
    // Center the selection, clamping at the top.
    let mut start = selected.saturating_sub(half);
    // Pull start in so end never overshoots `total`.
    if start + visible > total {
        start = total - visible;
    }
    let end = start + visible;
    (start, end)
}

/// Strip bytes that would let an untrusted string drive the user's terminal
/// — ESC (0x1B), DEL (0x7F), and other C0 control characters except `\t`.
/// Snapshot manifests and transcript lines are user-authored or
/// agent-authored content; if one contains a stray `\x1b[2J` it would clear
/// the screen the moment the catalog opens. Apply this to every string
/// that came from a snapshot before handing it to ratatui.
///
/// `\n` is also stripped here because the catalog/detail renderers handle
/// line breaks themselves via separate `Line`s — letting a raw newline
/// through would visually merge a field with the following one.
pub(super) fn sanitize_for_display(s: &str) -> String {
    s.chars()
        .filter(|c| {
            if *c == '\t' {
                return true;
            }
            !c.is_control()
        })
        .collect()
}

/// Truncate `s` to at most `max` characters (counting chars, not bytes).
/// Used for one-line previews of transcript text in the detail pane.
pub(super) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('\u{2026}');
        out
    }
}

/// Format a unix-seconds timestamp relative to `now_secs` as "just now",
/// "5m ago", "2h ago", "3d ago", etc. Saturates at "1y+ ago" so very old
/// timestamps don't render an awkwardly large number.
pub(super) fn format_relative_time(then_secs: u64, now_secs: u64) -> String {
    let delta = now_secs.saturating_sub(then_secs);
    if delta < 60 {
        return "just now".to_string();
    }
    let minutes = delta / 60;
    if minutes < 60 {
        return format!("{minutes}m ago");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    if days < 365 {
        return format!("{days}d ago");
    }
    "1y+ ago".to_string()
}

pub(crate) fn handle_save_snapshot(
    state: SaveSnapshotMut<'_>,
    _ctx: InputCtx<'_>,
    event: &CrosstermEvent,
) -> InputOutcome {
    let CrosstermEvent::Key(key) = event else {
        return InputOutcome::Consumed;
    };
    match key.code {
        KeyCode::Esc => InputOutcome::Cancel,
        KeyCode::Tab | KeyCode::BackTab => {
            *state.active_field = (*state.active_field + 1) % 2;
            *state.error = None;
            InputOutcome::Consumed
        }
        KeyCode::Enter => InputOutcome::Submit(SubmitAction::SaveSnapshot {
            workspace_id: state.workspace_id.to_string(),
            session_uid: state.session_uid.to_string(),
            name: state.name_text.trim().to_string(),
            description: state.description_text.trim().to_string(),
        }),
        KeyCode::Backspace => {
            let buf = if *state.active_field == 0 {
                &mut *state.name_text
            } else {
                &mut *state.description_text
            };
            buf.pop();
            *state.error = None;
            InputOutcome::Consumed
        }
        KeyCode::Char(c) => {
            let buf = if *state.active_field == 0 {
                &mut *state.name_text
            } else {
                &mut *state.description_text
            };
            buf.push(c);
            *state.error = None;
            InputOutcome::Consumed
        }
        _ => InputOutcome::Consumed,
    }
}

pub(crate) fn handle_task_settings(
    state: TaskSettingsMut<'_>,
    _ctx: InputCtx<'_>,
    event: &CrosstermEvent,
) -> InputOutcome {
    let CrosstermEvent::Key(key) = event else {
        return InputOutcome::Consumed;
    };
    match key.code {
        KeyCode::Esc => InputOutcome::Cancel,
        KeyCode::Tab | KeyCode::BackTab => {
            *state.active_field = (*state.active_field + 1) % 2;
            InputOutcome::Consumed
        }
        KeyCode::Char(' ') | KeyCode::Right if *state.active_field == 1 => {
            *state.color = theme::cycle_user_color(state.color.as_deref(), true);
            InputOutcome::Consumed
        }
        KeyCode::Left if *state.active_field == 1 => {
            *state.color = theme::cycle_user_color(state.color.as_deref(), false);
            InputOutcome::Consumed
        }
        KeyCode::Enter => InputOutcome::Submit(SubmitAction::SaveTaskName {
            task_id: state.task_id.to_string(),
            name: state.name.trim().to_string(),
            color: state.color.clone(),
        }),
        KeyCode::Backspace if *state.active_field == 0 => {
            state.name.pop();
            InputOutcome::Consumed
        }
        KeyCode::Char(c) if *state.active_field == 0 => {
            state.name.push(c);
            InputOutcome::Consumed
        }
        _ => InputOutcome::Consumed,
    }
}

pub(crate) fn handle_workflow_launch_confirm(
    state: WorkflowLaunchConfirmMut<'_>,
    _ctx: InputCtx<'_>,
    event: &CrosstermEvent,
) -> InputOutcome {
    let CrosstermEvent::Key(key) = event else {
        return InputOutcome::Consumed;
    };
    // `active_slot == slots.len()` means the goal text field is focused;
    // typing goes there instead of cycling slots.
    let goal_focused = *state.active_slot == state.slots.len();
    let positions = state.slots.len() + 1;
    match key.code {
        KeyCode::Esc => InputOutcome::Cancel,
        KeyCode::Enter => {
            let goal_owned = state.goal.trim().to_string();
            let goal_opt = if goal_owned.is_empty() {
                None
            } else {
                Some(goal_owned)
            };
            InputOutcome::Submit(SubmitAction::LaunchWorkflow {
                ws_id: state.ws_id.to_string(),
                workflow_name: state.workflow_name.to_string(),
                slots: state.slots.clone(),
                goal: goal_opt,
                cursor_task_id: state.cursor_task_id.map(str::to_string),
            })
        }
        KeyCode::Down | KeyCode::Tab => {
            *state.active_slot = (*state.active_slot + 1) % positions;
            InputOutcome::Consumed
        }
        KeyCode::Up | KeyCode::BackTab => {
            *state.active_slot = if *state.active_slot == 0 {
                positions - 1
            } else {
                *state.active_slot - 1
            };
            InputOutcome::Consumed
        }
        KeyCode::Right => {
            if !goal_focused {
                if let Some(slot) = state.slots.get_mut(*state.active_slot) {
                    slot.cycle(1);
                }
            }
            InputOutcome::Consumed
        }
        KeyCode::Left => {
            if !goal_focused {
                if let Some(slot) = state.slots.get_mut(*state.active_slot) {
                    slot.cycle(-1);
                }
            }
            InputOutcome::Consumed
        }
        KeyCode::Backspace => {
            if goal_focused {
                state.goal.pop();
            }
            InputOutcome::Consumed
        }
        KeyCode::Char(c) => {
            if goal_focused {
                state.goal.push(c);
            } else {
                // Slot navigation shorthands (only when the goal field
                // isn't focused, so characters in the goal don't get
                // consumed as commands).
                match c {
                    'j' => *state.active_slot = (*state.active_slot + 1) % positions,
                    'k' => {
                        *state.active_slot = if *state.active_slot == 0 {
                            positions - 1
                        } else {
                            *state.active_slot - 1
                        };
                    }
                    'l' | ' ' => {
                        if let Some(slot) = state.slots.get_mut(*state.active_slot) {
                            slot.cycle(1);
                        }
                    }
                    'h' => {
                        if let Some(slot) = state.slots.get_mut(*state.active_slot) {
                            slot.cycle(-1);
                        }
                    }
                    _ => {}
                }
            }
            InputOutcome::Consumed
        }
        _ => InputOutcome::Consumed,
    }
}

pub(crate) fn handle_workflow_picker(
    state: WorkflowPickerMut<'_>,
    _ctx: InputCtx<'_>,
    event: &CrosstermEvent,
) -> InputOutcome {
    let CrosstermEvent::Key(key) = event else {
        return InputOutcome::Consumed;
    };
    match key.code {
        KeyCode::Esc => InputOutcome::Cancel,
        KeyCode::Enter => match state.names.get(*state.selected).cloned() {
            Some(wf_name) => {
                InputOutcome::Submit(SubmitAction::EnterWorkflowLaunchConfirm {
                    ws_id: state.ws_id.to_string(),
                    focused_si: state.focused_si,
                    workflow_name: wf_name,
                    cursor_task_id: state.cursor_task_id.map(str::to_string),
                })
            }
            None => InputOutcome::Submit(SubmitAction::None),
        },
        KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
            if !state.names.is_empty() {
                *state.selected = (*state.selected + 1) % state.names.len();
            }
            InputOutcome::Consumed
        }
        KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => {
            if !state.names.is_empty() {
                *state.selected = if *state.selected == 0 {
                    state.names.len() - 1
                } else {
                    *state.selected - 1
                };
            }
            InputOutcome::Consumed
        }
        _ => InputOutcome::Consumed,
    }
}

pub(crate) fn handle_past_workspace_picker(
    candidates: &[PastCandidate],
    selected: &mut usize,
    _ctx: InputCtx<'_>,
    event: &CrosstermEvent,
) -> InputOutcome {
    let CrosstermEvent::Key(key) = event else {
        return InputOutcome::Consumed;
    };
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => InputOutcome::Cancel,
        KeyCode::Enter => match candidates.get(*selected) {
            Some(c) => InputOutcome::Submit(SubmitAction::ReopenPastWorkspace {
                ws_id: c.ws_id.clone(),
            }),
            None => InputOutcome::Cancel,
        },
        KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
            if !candidates.is_empty() {
                *selected = (*selected + 1) % candidates.len();
            }
            InputOutcome::Consumed
        }
        KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => {
            if !candidates.is_empty() {
                *selected = if *selected == 0 {
                    candidates.len() - 1
                } else {
                    *selected - 1
                };
            }
            InputOutcome::Consumed
        }
        _ => InputOutcome::Consumed,
    }
}

/// A-p fuzzy-find palette. Plain typed chars edit the query (this is a
/// text filter — j/k must NOT move the selection), so selection movement
/// rides Up/Down, Tab/BackTab, and Ctrl-j/Ctrl-k. Enter submits the
/// selected row of the CURRENT filtered view; Esc / A-p close.
pub(crate) fn handle_session_palette(
    candidates: &[PaletteCandidate],
    query: &mut String,
    selected: &mut usize,
    _ctx: InputCtx<'_>,
    event: &CrosstermEvent,
) -> InputOutcome {
    let CrosstermEvent::Key(key) = event else {
        return InputOutcome::Consumed;
    };
    // A-p toggles the palette closed (matches the open binding).
    if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('p') {
        return InputOutcome::Cancel;
    }
    // Same filtered view the draw shows — selection indexes into it.
    let displays: Vec<&str> = candidates.iter().map(|c| c.display.as_str()).collect();
    let mut filtered = palette_match_indices(query, &displays);
    filtered.truncate(PALETTE_MAX_RESULTS);
    let flen = filtered.len();
    match key.code {
        KeyCode::Esc => InputOutcome::Cancel,
        KeyCode::Enter => match filtered.get(*selected).or(filtered.first()) {
            Some(&ci) => InputOutcome::Submit(SubmitAction::PaletteJump {
                target: candidates[ci].target.clone(),
            }),
            None => InputOutcome::Cancel,
        },
        KeyCode::Down | KeyCode::Tab => {
            if flen > 0 {
                *selected = (*selected + 1) % flen;
            }
            InputOutcome::Consumed
        }
        KeyCode::Up | KeyCode::BackTab => {
            if flen > 0 {
                *selected = if *selected == 0 { flen - 1 } else { *selected - 1 };
            }
            InputOutcome::Consumed
        }
        // Ctrl-j / Ctrl-k mirror Down / Up. These arms MUST precede the
        // generic Char(c) arm — plain j/k fall through to typing.
        KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if flen > 0 {
                *selected = (*selected + 1) % flen;
            }
            InputOutcome::Consumed
        }
        KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if flen > 0 {
                *selected = if *selected == 0 { flen - 1 } else { *selected - 1 };
            }
            InputOutcome::Consumed
        }
        KeyCode::Backspace => {
            query.pop();
            *selected = 0;
            InputOutcome::Consumed
        }
        KeyCode::Char(c)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT) =>
        {
            query.push(c);
            *selected = 0;
            InputOutcome::Consumed
        }
        _ => InputOutcome::Consumed,
    }
}

/// A-i read-only peek: j/k, Up/Down, PgUp/PgDn scroll (no text input in
/// this modal); Esc, q, or A-i again close. `max_scroll` comes from the
/// modal state (written back by the draw) so scrolling clamps to the
/// wrapped content height.
pub(crate) fn handle_task_peek(
    scroll: &mut u16,
    max_scroll: u16,
    event: &CrosstermEvent,
) -> InputOutcome {
    let CrosstermEvent::Key(key) = event else {
        return InputOutcome::Consumed;
    };
    // A-i toggles the peek closed (matches the open binding).
    if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('i') {
        return InputOutcome::Cancel;
    }
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => InputOutcome::Cancel,
        KeyCode::Down | KeyCode::Char('j') => {
            *scroll = scroll.saturating_add(1).min(max_scroll);
            InputOutcome::Consumed
        }
        KeyCode::Up | KeyCode::Char('k') => {
            *scroll = scroll.saturating_sub(1);
            InputOutcome::Consumed
        }
        KeyCode::PageDown => {
            *scroll = scroll.saturating_add(10).min(max_scroll);
            InputOutcome::Consumed
        }
        KeyCode::PageUp => {
            *scroll = scroll.saturating_sub(10);
            InputOutcome::Consumed
        }
        _ => InputOutcome::Consumed,
    }
}

pub(crate) fn handle_workflow_history(
    _ctx: InputCtx<'_>,
    event: &CrosstermEvent,
) -> InputOutcome {
    let CrosstermEvent::Key(key) = event else {
        return InputOutcome::Consumed;
    };
    match key.code {
        KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => InputOutcome::Cancel,
        _ => InputOutcome::Consumed,
    }
}

pub(crate) fn handle_confirm(
    action: &ConfirmAction,
    _ctx: InputCtx<'_>,
    event: &CrosstermEvent,
) -> InputOutcome {
    let CrosstermEvent::Key(key) = event else {
        return InputOutcome::Consumed;
    };
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
            let submit = match action.clone() {
                ConfirmAction::MarkDone => SubmitAction::MarkActiveDone,
                ConfirmAction::Delete => SubmitAction::DeleteActive,
                ConfirmAction::StopWorkflow { run_id } => SubmitAction::StopWorkflow { run_id },
                ConfirmAction::RestoreTombstones { ws_id } => {
                    SubmitAction::RestoreTombstones { ws_id }
                }
            };
            InputOutcome::Submit(submit)
        }
        KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => InputOutcome::Cancel,
        _ => InputOutcome::Consumed,
    }
}

/// Max UTF-8 payload bytes accepted into an OSC 52 write by the A-' yank
/// path (~100KB of base64 after the 4/3 expansion). Terminal emulators cap
/// OSC payload sizes around this order of magnitude, and an unbounded
/// multi-hundred-KB yank would stall the synchronous stdout write.
const OSC52_MAX_TEXT_BYTES: usize = 75 * 1024;

/// Build the OSC 52 clipboard-set escape sequence (ST-terminated) for
/// `text`. Pure — the stdout write lives in [`copy_to_clipboard`].
fn osc52_sequence(text: &str) -> String {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    format!("\x1b]52;c;{}\x1b\\", encoded)
}

/// Truncate `text` to at most `max_bytes` UTF-8 bytes, backing off to the
/// nearest char boundary so the result is always valid UTF-8. Returns the
/// (possibly shortened) slice plus whether truncation happened.
fn truncate_utf8(text: &str, max_bytes: usize) -> (&str, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

/// Copy text to the system clipboard via the OSC 52 escape sequence.
/// Supported by most modern terminal emulators (kitty, wezterm, iTerm2, alacritty,
/// xterm, and tmux with `set -g set-clipboard on`).
fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    let seq = osc52_sequence(text);
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

/// A-' yank core: read the session's transcript through the engine-specific
/// [`crate::agent::Agent::read_messages`] reader (the same normalized-message
/// path that powers MCP `read_session_output` and workflow templating's
/// `assistant[-1]`) and return the LAST assistant message's text. Errors are
/// user-facing status strings. A fresh session with no bound transcript
/// yields an empty message list, which lands in the "no assistant messages"
/// arm rather than an error.
fn last_assistant_message_text(
    ts: &TerminalSession,
    worktree_path: &Path,
) -> Result<String, String> {
    let agent = crate::agent::agent_for(&ts.session_type);
    let ctx = crate::agent::AgentCtx { ts, worktree_path };
    let (messages, _cursor) = agent
        .read_messages(ctx, None, usize::MAX)
        .map_err(|e| format!("Yank: transcript read failed: {}", e))?;
    messages
        .into_iter()
        .rev()
        .find(|m| m.role == crate::agent::Role::Assistant)
        .map(|m| m.content)
        .ok_or_else(|| "Yank: no assistant messages in transcript yet".to_string())
}

impl App {
    pub fn is_input_mode(&self) -> bool {
        !matches!(self.input_mode, InputMode::Normal)
    }

    /// Body of `SubmitAction::SaveSnapshot`. Resolves the focused session's
    /// transcript path via the agent strategy, builds a `SaveSpec`, and
    /// either toasts on success or re-opens the modal with the error
    /// surfaced inline.
    fn handle_save_snapshot_submit(
        &mut self,
        workspace_id: String,
        session_uid: String,
        name: String,
        description: String,
    ) {
        // Reopen the modal carrying `name` / `description` so the user
        // doesn't lose what they typed. Called via the early-return closure
        // pattern below — used for any failure that's recoverable in-form
        // (validation, name conflict, missing transcript). Stable IDs are
        // re-stored so a subsequent reorder still resolves correctly.
        let reopen = |this: &mut Self, err: String| {
            this.input_mode = InputMode::SaveSnapshot {
                workspace_id: workspace_id.clone(),
                session_uid: session_uid.clone(),
                name_text: name.clone(),
                description_text: description.clone(),
                active_field: 0,
                error: Some(err),
            };
        };

        // Resolve stable IDs → current indices. Backend events can reorder
        // workspaces while the modal is open, so the IDs are the source of
        // truth, not the indices that were captured at open time.
        let Some((_wi, _si)) = resolve_session_by_ids(
            &self.workspaces,
            &workspace_id,
            &session_uid,
        ) else {
            self.set_status_msg(
                "Snapshot cancelled — the target session is no longer available",
            );
            return;
        };

        // Pull immutable refs anew now that we have current indices.
        let ws = &self.workspaces[_wi];
        let ts = &ws.sessions[_si];

        let engine = match ts.session_type.as_str() {
            "claude" => Engine::ClaudeCode,
            "codex" => Engine::Codex,
            _ => {
                reopen(
                    self,
                    "Snapshots only supported for Claude Code / Codex sessions"
                        .into(),
                );
                return;
            }
        };

        let Some(source_cwd) = ws.worktree_path.clone() else {
            reopen(self, "Session has no worktree path".into());
            return;
        };

        let source_session_uid = ts.uid.clone();
        let Some(source_transcript_id) = ts.transcript_id.clone() else {
            reopen(
                self,
                "No transcript yet — let the session produce at least one message first"
                    .into(),
            );
            return;
        };

        // Resolve the transcript on disk via the agent strategy (walks
        // ~/.codex/sessions for codex; encoded-cwd join for claude).
        let agent = agent::agent_for(&ts.session_type);
        let ctx = agent::AgentCtx {
            ts,
            worktree_path: &source_cwd,
        };
        let Some(source_transcript_path) = agent.transcript_path(ctx) else {
            reopen(
                self,
                "Could not resolve transcript path for this session".into(),
            );
            return;
        };

        // Claude has a per-cwd memory dir; codex does not.
        let memory_dir = match engine {
            Engine::ClaudeCode => agent_memory::claude_memory_dir(&source_cwd),
            Engine::Codex => None,
        };

        let spec = agent_memory::SaveSpec {
            name: name.as_str(),
            description: description.as_str(),
            engine: engine.clone(),
            source_session_uid: source_session_uid.as_str(),
            source_transcript_id: source_transcript_id.as_str(),
            source_cwd: &source_cwd,
            source_transcript_path: &source_transcript_path,
            source_memory_dir: memory_dir.as_deref(),
        };

        match agent_memory::save(spec) {
            Ok(snap) => {
                self.set_status_msg(&format!("Snapshot saved: {}", snap.name));
            }
            Err(e) => {
                reopen(self, e.to_string());
            }
        }
    }

    /// Open the save-snapshot modal for the focused session. Surface the
    /// design's two upfront error toasts (engine not supported, no
    /// transcript yet) so the modal only opens when a save can plausibly
    /// succeed. See `DESIGN_AGENT_MEMORIES.md` save flow.
    /// Load all saved snapshots and open the catalog modal in browse
    /// mode. Toasts if the on-disk list can't be read; otherwise opens
    /// even when there are zero snapshots (the empty-state render tells
    /// the user how to create one).
    ///
    /// `picker_target` is `None` for the stand-alone A-z catalog. Pass
    /// `Some(target)` when invoking from a parent form so the catalog
    /// can re-open it (with seed_from set or unchanged) on submit /
    /// cancel — including when `list()` fails before the catalog can
    /// even render. Without that restoration, a list() error would
    /// silently drop the user's typed form state.
    fn open_snapshot_catalog(&mut self, picker_target: Option<PickerTarget>) {
        let (mode, status) =
            catalog_open_outcome(agent_memory::list(), picker_target);
        self.input_mode = mode;
        if let Some(msg) = status {
            self.set_status_msg(&msg);
        }
    }

    fn open_save_snapshot(&mut self) {
        let (wi, si) = match self.cursor {
            Cursor::Session(wi, si) => (wi, si),
            _ => {
                self.set_status_msg("Snapshot: focus a session first");
                return;
            }
        };
        let Some(ws) = self.workspaces.get(wi) else {
            return;
        };
        let Some(ts) = ws.sessions.get(si) else {
            return;
        };
        if !matches!(ts.session_type.as_str(), "claude" | "codex") {
            self.set_status_msg(
                "Snapshots only supported for Claude Code / Codex sessions",
            );
            return;
        }
        if ts.transcript_id.is_none() {
            self.set_status_msg(
                "No transcript yet — let the session produce at least one message first",
            );
            return;
        }
        // Capture stable IDs so a backend-driven workspace reorder while
        // the modal is open doesn't make the indices stale.
        self.input_mode = InputMode::SaveSnapshot {
            workspace_id: ws.id.clone(),
            session_uid: ts.uid.clone(),
            name_text: String::new(),
            description_text: String::new(),
            active_field: 0,
            error: None,
        };
    }

    /// Open settings for whatever the cursor is focused on — a workspace
    /// (rename) when on a header, a session (label / idle / hidden) when on
    /// a specific session.
    fn open_session_settings(&mut self) {
        match self.cursor.clone() {
            Cursor::Session(wi, si) => {
                if let Some(ws) = self.workspaces.get(wi) {
                    if let Some(ts) = ws.sessions.get(si) {
                        let timeout = ts.idle_timeout_secs;
                        let burst = ts.burst_threshold;
                        self.input_mode = InputMode::SessionSettings {
                            ws_index: wi,
                            session_index: si,
                            name: ts.label.clone(),
                            idle_timeout: if timeout == 0 {
                                DEFAULT_IDLE_TIMEOUT_SECS.to_string()
                            } else {
                                timeout.to_string()
                            },
                            burst_threshold: if burst == 0 {
                                WAKEUP_BURST_THRESHOLD.to_string()
                            } else {
                                burst.to_string()
                            },
                            hidden: ts.hidden,
                            notify_on_idle: ts.notify_on_idle,
                            global_perms: ts.global_perms,
                            color: ts.color.clone(),
                            seeded_from_snapshot: ts.seeded_from_snapshot.clone(),
                            active_field: 0,
                        };
                    }
                }
            }
            Cursor::Workspace(wi) => {
                if let Some(ws) = self.workspaces.get(wi) {
                    self.input_mode = InputMode::WorkspaceSettings {
                        ws_index: wi,
                        name: ws.name.clone(),
                        color: ws.color.clone(),
                        pinned: ws.pinned,
                        active_field: 0,
                    };
                }
            }
            Cursor::Task { task_id, .. } => {
                let current_name = self
                    .tasks
                    .iter()
                    .find(|t| t.task_id.as_deref() == Some(task_id.as_str()))
                    .map(|t| t.name.clone())
                    .unwrap_or_default();
                let current_color = self.task_colors.get(&task_id).cloned();
                self.input_mode = InputMode::TaskSettings {
                    task_id,
                    name: current_name,
                    color: current_color,
                    active_field: 0,
                };
            }
        }
    }

    /// Build the candidate list for the A-O picker and open it. Shows a
    /// status message when no past workspaces exist instead of an empty
    /// modal. Most-recent (latest tombstone) first.
    fn open_past_workspace_picker(&mut self) {
        let mut candidates: Vec<PastCandidate> = self
            .workspaces
            .iter()
            .enumerate()
            .filter(|(wi, _)| self.is_past_workspace(*wi))
            .map(|(_, ws)| {
                let last_exited_at = ws
                    .tombstones
                    .iter()
                    .map(|t| t.exited_at)
                    .fold(0.0f64, f64::max);
                let worktree_exists = ws
                    .worktree_path
                    .as_ref()
                    .map_or(false, |p| p.exists());
                PastCandidate {
                    ws_id: ws.id.clone(),
                    display: ws.name.clone(),
                    worktree_path: ws.worktree_path.clone(),
                    worktree_exists,
                    last_exited_at,
                }
            })
            .collect();
        if candidates.is_empty() {
            self.set_status_msg("No past workspaces");
            return;
        }
        candidates.sort_by(|a, b| {
            b.last_exited_at
                .partial_cmp(&a.last_exited_at)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        self.input_mode = InputMode::PastWorkspacePicker {
            candidates,
            selected: 0,
        };
    }

    /// Toggle terminal mouse capture. When off, mouse events go to the
    /// terminal emulator instead of the TUI, so the user can use native
    /// selection (including block-select chords like Ctrl+Shift+drag).
    fn toggle_mouse_capture(&mut self) {
        use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
        use crossterm::execute;
        let mut stdout = std::io::stdout();
        if self.mouse_capture_enabled {
            let _ = execute!(stdout, DisableMouseCapture);
            self.mouse_capture_enabled = false;
            self.set_status_msg("Mouse capture OFF — use terminal's native selection (Alt+m to re-enable)");
        } else {
            let _ = execute!(stdout, EnableMouseCapture);
            self.mouse_capture_enabled = true;
            self.set_status_msg("Mouse capture ON");
        }
    }

    /// A-': copy the focused session's LAST assistant message to the system
    /// clipboard via OSC 52 (`copy_to_clipboard`), so it works over SSH too.
    /// Focus resolution reuses `active_session` — a workspace/task row maps
    /// to its single session when unambiguous, otherwise we ask the user to
    /// focus one. Every failure path is a status message, never a panic.
    fn yank_last_assistant_message(&mut self) {
        let outcome: Result<String, String> = match self.active_session() {
            None => Err("Yank: focus a session first".to_string()),
            Some((_, ts)) if ts.session_type == "bash" => {
                Err("Yank: bash sessions have no transcript".to_string())
            }
            Some((ws, ts)) => match ws.worktree_path.as_deref() {
                None => Err(
                    "Yank: workspace has no worktree — transcript path unknown"
                        .to_string(),
                ),
                Some(wt) => last_assistant_message_text(ts, wt),
            },
        };
        match outcome {
            Ok(text) => {
                let total_chars = text.chars().count();
                let (payload, truncated) =
                    truncate_utf8(&text, OSC52_MAX_TEXT_BYTES);
                copy_to_clipboard(payload);
                if truncated {
                    self.set_status_msg(&format!(
                        "Yanked last assistant message ({} chars — truncated to {}KB for clipboard)",
                        total_chars,
                        payload.len() / 1024,
                    ));
                } else {
                    self.set_status_msg(&format!(
                        "Yanked last assistant message ({} chars)",
                        total_chars,
                    ));
                }
            }
            Err(msg) => self.set_status_msg(&msg),
        }
    }

    /// Handle a crossterm event. Returns true if consumed.
    pub fn handle_event(&mut self, event: &CrosstermEvent) -> bool {
        // Drop key release events — we only care about presses/repeats.
        if let CrosstermEvent::Key(key) = event {
            if key.kind == crossterm::event::KeyEventKind::Release {
                return false;
            }
        }

        self.needs_redraw = true;

        // A-; MRU walk boundary: any OTHER key press ends the walk, so
        // the next A-; starts fresh from the live deque. This is the
        // closest a TUI gets to classic alt-tab's "modifier released".
        if let CrosstermEvent::Key(key) = event {
            let is_mru_chord = key.modifiers.contains(KeyModifiers::ALT)
                && key.code == KeyCode::Char(';');
            if !is_mru_chord && self.mru_walk.is_some() {
                self.mru_walk = None;
            }
        }

        // Alt+t toggles between Sessions and Planning view.
        if let CrosstermEvent::Key(key) = event {
            if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('t') {
                self.view_mode = match self.view_mode {
                    ViewMode::Sessions => {
                        // Refresh planning tasks when switching to planning view.
                        self.backend.refresh_plan_tasks();
                        ViewMode::Planning
                    }
                    ViewMode::Planning => ViewMode::Sessions,
                };
                return true;
            }
        }

        // Alt+m toggles mouse capture so the user can use their terminal's
        // native selection (including block-select chords).
        if let CrosstermEvent::Key(key) = event {
            if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('m') {
                self.toggle_mouse_capture();
                return true;
            }
            // Phase 6: Alt+, toggles the activity feed strip.
            if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char(',') {
                self.activity_visible = !self.activity_visible;
                self.needs_redraw = true;
                return true;
            }
            // Continuous panel: Alt+c toggles the dedicated continuous COLUMN
            // (orchestrators + their nested subtasks, split off the right of the
            // sidebar). It's the SINGLE continuous control — column ON shows the
            // continuous tree in its own column; column OFF hides it entirely
            // (continuous tasks never appear in the main sidebar either way).
            // Persisted in the manifest. (The old Alt+Shift+C column toggle and
            // the separate Alt+c master-hide were merged into this one key.)
            if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('c') {
                self.continuous_column_on = !self.continuous_column_on;
                // Turning the column off strands a continuous-column cursor —
                // pull it back to the main sidebar (restoring the saved spot).
                // Stash the continuous spot first so toggling back on + stepping
                // in lands where you left off.
                if !self.continuous_column_on
                    && self.cursor_column == SidebarColumn::Continuous
                {
                    self.saved_continuous_uid = self.cursor_session_uid();
                    self.cursor = self
                        .saved_main_cursor
                        .take()
                        .unwrap_or(Cursor::Workspace(0));
                    self.cursor_column = SidebarColumn::Main;
                }
                self.needs_redraw = true;
                return true;
            }
        }

        // Delegate to planning view when in Planning mode.
        if self.view_mode == ViewMode::Planning {
            // Keep planning's workspace picker in sync with the current
            // set of open workspaces before it sees the event.
            let candidates = self.collect_workspace_candidates();
            self.planning.set_workspace_candidates(candidates);
            let action = self.planning.handle_event(event);
            match action {
                PlanAction::Consumed => return true,
                PlanAction::Ignored => return false,
                PlanAction::LaunchTask {
                    project,
                    slug,
                    prompt,
                    branch,
                    autostart,
                    task_id,
                    parent_task_id,
                    in_place,
                } => {
                    self.launch_from_plan(
                        &project,
                        &slug,
                        &prompt,
                        branch.as_deref(),
                        autostart,
                        &task_id,
                        parent_task_id.as_deref(),
                        in_place,
                    );
                    return true;
                }
                PlanAction::LaunchTaskIntoWorkspace {
                    workspace_id,
                    task_id,
                    task_title,
                    task_repo_url,
                    project,
                    prompt,
                    parent_task_id,
                } => {
                    self.launch_into_workspace(
                        &workspace_id,
                        &task_id,
                        &task_title,
                        &task_repo_url,
                        &project,
                        &prompt,
                        parent_task_id.as_deref(),
                    );
                    return true;
                }
                PlanAction::UnbindTask { task_id } => {
                    self.unbind_task_from_workspace(&task_id);
                    return true;
                }
                PlanAction::UnlaunchTask { task_id } => {
                    self.unlaunch_task(&task_id);
                    return true;
                }
                PlanAction::ReopenTask { task_id } => {
                    self.reopen_task_from_planning(&task_id);
                    return true;
                }
                PlanAction::SwitchToSessions => {
                    self.view_mode = ViewMode::Sessions;
                    return true;
                }
                PlanAction::Quit => {
                    self.save_session_manifest();
                    self.clear_tui_sessions_on_daemon();
                    self.should_quit = true;
                    return true;
                }
                PlanAction::CreateTask {
                    project,
                    repo_url,
                    name,
                    description,
                    status,
                    parent_task_id,
                    worktree_mode,
                } => {
                    self.backend.create_plan_task(
                        project,
                        repo_url,
                        name,
                        description,
                        status,
                        parent_task_id,
                        worktree_mode,
                    );
                    return true;
                }
                PlanAction::UpdateTask { id, fields, status_msg } => {
                    self.backend.update_plan_task(id, fields);
                    if let Some(msg) = status_msg {
                        self.set_status_msg(&msg);
                    }
                    return true;
                }
                PlanAction::BulkUpdateTasks { ids, fields } => {
                    for id in ids {
                        self.backend.update_plan_task(id, fields.clone());
                    }
                    return true;
                }
                PlanAction::DeleteTask { id } => {
                    self.backend.delete_plan_task(id);
                    return true;
                }
                PlanAction::WatchBacktest {
                    task_id,
                    kind,
                    worker_vm,
                    vm_project,
                    vm_zone,
                    title,
                } => {
                    self.watch_backtest(
                        &task_id, &kind, worker_vm, vm_project, vm_zone, &title,
                    );
                    return true;
                }
                PlanAction::RefreshTasks => {
                    self.backend.refresh_plan_tasks();
                    return true;
                }
            }
        }

        // If in input mode, handle input events.
        if self.is_input_mode() {
            return self.handle_input_event(event);
        }

        if let CrosstermEvent::Key(key) = event {
            if key.modifiers.contains(KeyModifiers::ALT) {
                match key.code {
                    KeyCode::Char('q') => {
                        self.save_session_manifest();
                        self.clear_tui_sessions_on_daemon();
                        self.should_quit = true;
                        return true;
                    }
                    KeyCode::Char('j') => {
                        let prev = self.cursor_session_uid();
                        self.navigate(1);
                        self.note_session_focus_change(prev);
                        return true;
                    }
                    KeyCode::Char('k') => {
                        let prev = self.cursor_session_uid();
                        self.navigate(-1);
                        self.note_session_focus_change(prev);
                        return true;
                    }
                    // A-g: cycle through sessions needing a human — pending
                    // notify_user alerts first, then idle sessions.
                    KeyCode::Char('g') => {
                        let prev = self.cursor_session_uid();
                        self.jump_to_next_attention();
                        self.note_session_focus_change(prev);
                        return true;
                    }
                    // A-;: MRU quick-switch — jump to the most recent other
                    // session; repeated presses walk deeper into the ring.
                    KeyCode::Char(';') => {
                        self.mru_jump();
                        return true;
                    }
                    // A-p: fuzzy-find palette over sessions + workspaces.
                    // (Sessions view only — Planning's A-p project picker is
                    // dispatched in planning.rs before this match is reached.)
                    KeyCode::Char('p') => {
                        self.open_session_palette();
                        return true;
                    }
                    // A-i: read-only info peek for the focused row.
                    KeyCode::Char('i') => {
                        self.open_task_peek();
                        return true;
                    }
                    // A-': yank the focused session's last assistant message
                    // to the system clipboard (OSC 52, so it works over SSH).
                    KeyCode::Char('\'') => {
                        self.yank_last_assistant_message();
                        return true;
                    }
                    KeyCode::Char('v') => {
                        self.sidebar_view = match self.sidebar_view {
                            SidebarView::Status => SidebarView::Task,
                            SidebarView::Task => SidebarView::Status,
                        };
                        self.save_session_manifest();
                        return true;
                    }
                    KeyCode::Char('s') => {
                        self.start_new_terminal_session();
                        return true;
                    }
                    // A-W (close workspace) vs A-w (close session). Terminals
                    // differ on whether Shift is baked into the char case or
                    // reported as a modifier — accept both forms.
                    KeyCode::Char('W') => {
                        self.close_active_workspace();
                        return true;
                    }
                    KeyCode::Char('w')
                        if key.modifiers.contains(KeyModifiers::SHIFT) =>
                    {
                        self.close_active_workspace();
                        return true;
                    }
                    KeyCode::Char('w') => {
                        self.close_active_session();
                        return true;
                    }
                    KeyCode::Char('a') => {
                        self.attach_active();
                        return true;
                    }
                    KeyCode::Char('d') => {
                        self.input_mode = InputMode::Confirm {
                            prompt: "Mark task done? Sessions for this task will close.".to_string(),
                            action: ConfirmAction::MarkDone,
                        };
                        return true;
                    }
                    KeyCode::Char('x') => {
                        let prompt = if self.cursor_task_id().is_some() {
                            "Delete this task and close its sessions?".to_string()
                        } else {
                            "Delete this workspace? Worktree, branch, and any bound tasks will be removed.".to_string()
                        };
                        self.input_mode = InputMode::Confirm {
                            prompt,
                            action: ConfirmAction::Delete,
                        };
                        return true;
                    }
                    KeyCode::Char('r') => {
                        self.backend.refresh();
                        // A-r doubles as "reconnect now": accelerate in-flight
                        // remote reconnects and revive any that gave up (the
                        // daemon may have restored them since). Addresses the
                        // "A-r doesn't clear a frozen remote pane" gap.
                        let nudged = self.nudge_remote_reconnects();
                        // Also force a full physical repaint — A-r doubles
                        // as the "fix my screen" recovery if ratatui's diff
                        // model ever desyncs from the terminal.
                        self.force_clear = true;
                        if nudged > 0 {
                            self.set_status_msg(&format!(
                                "Refreshing + reconnecting {} remote session(s)...",
                                nudged,
                            ));
                        } else {
                            self.set_status_msg("Refreshing + redrawing...");
                        }
                        return true;
                    }
                    KeyCode::Char('e') => {
                        self.open_session_settings();
                        return true;
                    }
                    // A-H toggles the focused session's hidden status. It took
                    // over from the retired global-host switcher (`A-H` /
                    // `cycle_active_host`, both removed — host is now a
                    // per-workspace attribute, new sessions default to `local`).
                    // Some terminals deliver Alt+Shift+h as Char('h')+SHIFT,
                    // others as Char('H'); match both. **Bare A-h is now FREE** —
                    // it becomes continuous-panel column-nav LEFT (wired in S4).
                    KeyCode::Char('H') => {
                        self.toggle_session_hidden();
                        return true;
                    }
                    KeyCode::Char('h')
                        if key.modifiers.contains(KeyModifiers::SHIFT) =>
                    {
                        self.toggle_session_hidden();
                        return true;
                    }
                    // Continuous-panel column nav (S4): bare A-h / A-l move the
                    // cursor LEFT / RIGHT between the main sidebar and the
                    // continuous column. No-op when the column isn't shown.
                    KeyCode::Char('h') => {
                        let prev = self.cursor_session_uid();
                        self.step_column(-1);
                        self.note_session_focus_change(prev);
                        return true;
                    }
                    KeyCode::Char('l') => {
                        let prev = self.cursor_session_uid();
                        self.step_column(1);
                        self.note_session_focus_change(prev);
                        return true;
                    }
                    KeyCode::Char('b') => {
                        self.open_save_snapshot();
                        return true;
                    }
                    KeyCode::Char('z') => {
                        self.open_snapshot_catalog(None);
                        return true;
                    }
                    KeyCode::Char('n') => {
                        self.start_new_session();
                        return true;
                    }
                    // Cloud push/pull moved off A-p / A-l (freed for the
                    // continuous-panel column nav, S4) to A-9 / A-0 — digits
                    // deliver cleanly (unlike Alt+[, which can collide with the
                    // CSI escape introducer). Rarely-used cloud ops.
                    KeyCode::Char('9') => {
                        self.push_active();
                        return true;
                    }
                    KeyCode::Char('0') => {
                        self.pull_active();
                        return true;
                    }
                    KeyCode::Char('f') => {
                        self.open_workflow_launch();
                        return true;
                    }
                    KeyCode::Char('u') => {
                        self.resume_workflow_for_cursor();
                        return true;
                    }
                    // A-O (Alt+Shift+O): open the past-workspaces picker.
                    // Past workspaces never appear in the sidebar — this is
                    // the only path to find and reopen them. Terminals
                    // differ on whether Shift is folded into the case of
                    // the char or reported as a modifier — accept both.
                    KeyCode::Char('O') => {
                        self.open_past_workspace_picker();
                        return true;
                    }
                    KeyCode::Char('o')
                        if key.modifiers.contains(KeyModifiers::SHIFT) =>
                    {
                        self.open_past_workspace_picker();
                        return true;
                    }
                    KeyCode::Char('o') => {
                        if let Some(run_id) = self.focused_session_run_id() {
                            self.input_mode = InputMode::Confirm {
                                prompt: "Stop workflow? Sessions stay open; run can't resume.".to_string(),
                                action: ConfirmAction::StopWorkflow { run_id },
                            };
                        } else {
                            self.set_status_msg("Focused session is not in a workflow");
                        }
                        return true;
                    }
                    KeyCode::Char('y') => {
                        self.open_workflow_history();
                        return true;
                    }
                    // (Retired: A-H used to cycle the global active host. A-H now
                    // toggles session-hidden — handled at the top of this match
                    // via Char('H') / Char('h')+SHIFT. The global active_host and
                    // `cycle_active_host` are gone — host is a per-workspace
                    // attribute now, DESIGN_REMOVE_GLOBAL_HOST.md.)
                    _ => {}
                }
            }
        }

        // Handle scroll in terminal.
        if let CrosstermEvent::Key(key) = event {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                match key.code {
                    KeyCode::PageUp => {
                        if let Some((_, ts)) = self.active_session() {
                            use alacritty_terminal::grid::Scroll;
                            ts.session.term.lock().scroll_display(Scroll::PageUp);
                        }
                        return true;
                    }
                    KeyCode::PageDown => {
                        if let Some((_, ts)) = self.active_session() {
                            use alacritty_terminal::grid::Scroll;
                            ts.session.term.lock().scroll_display(Scroll::PageDown);
                        }
                        return true;
                    }
                    _ => {}
                }
            }
            // Plain PageUp/PageDown also scroll our scrollback (Shift not
            // required) — but only in the primary screen. In the alternate
            // screen there's no scrollback to move, and fullscreen apps (Claude
            // Code's renderer, vim, less) expect these keys themselves, so we let
            // them fall through to the PTY instead of swallowing them into a
            // no-op. Shift+PageUp/PageDown above stay the dedicated scrollback
            // binding regardless, matching the usual terminal convention.
            let in_alt_screen = self
                .active_session()
                .is_some_and(|(_, ts)| {
                    ts.session.term.lock().mode().contains(TermMode::ALT_SCREEN)
                });
            if !in_alt_screen {
                match key.code {
                    KeyCode::PageUp if key.modifiers.is_empty() => {
                        if let Some((_, ts)) = self.active_session() {
                            use alacritty_terminal::grid::Scroll;
                            ts.session.term.lock().scroll_display(Scroll::PageUp);
                        }
                        return true;
                    }
                    KeyCode::PageDown if key.modifiers.is_empty() => {
                        if let Some((_, ts)) = self.active_session() {
                            use alacritty_terminal::grid::Scroll;
                            ts.session.term.lock().scroll_display(Scroll::PageDown);
                        }
                        return true;
                    }
                    _ => {}
                }
            }
        }

        // Handle mouse events over the terminal pane: scroll wheel + click-drag selection.
        // Always consume — un-consumed mouse events would fall through to the terminal
        // forwarder below, which both snaps scroll to bottom and writes ANSI bytes to the PTY.
        if let CrosstermEvent::Mouse(me) = event {
            self.handle_terminal_mouse(me);
            return true;
        }

        // Remote auto-reconnect: while the active remote session's attach
        // stream is dead (awaiting reattach), don't forward input to its dead
        // EventLoop — the write would just fail and spam an error toast per
        // keystroke. The session rebinds on its own; queued workflow prompts
        // are preserved by the pending-write gate in `drain_terminal_events`.
        let active_reconnecting = match self.active_session() {
            Some((_, ts)) => self.reconnecting_sessions.contains(&ts.uid),
            None => false,
        };

        // Handle bracketed paste — send entire text at once, wrapped in
        // bracket escapes if the inner program has enabled bracketed paste mode.
        if let CrosstermEvent::Paste(text) = event {
            let mut paste_err: Option<(String, std::io::Error)> = None;
            let mut handled = false;
            if let Some(ts) = self.active_session_mut() {
                if !ts.session.exited && !active_reconnecting {
                    use alacritty_terminal::grid::Scroll;
                    ts.session.term.lock().scroll_display(Scroll::Bottom);

                    let term_mode = *ts.session.term.lock().mode();
                    let data = if term_mode.contains(TermMode::BRACKETED_PASTE) {
                        format!("\x1b[200~{}\x1b[201~", text).into_bytes()
                    } else {
                        text.as_bytes().to_vec()
                    };
                    if let Err(e) = ts.session.write(&data) {
                        paste_err = Some((ts.label.clone(), e));
                    }
                    ts.last_write_at = Some(Instant::now());
                    handled = true;
                }
            }
            if let Some((label, e)) = paste_err {
                self.set_status_msg(&format!("paste to {}: {}", label, e));
            }
            if handled {
                return true;
            }
        }

        // If the focused session is part of a running workflow and the user
        // hit Ctrl-C, pause the run. We do not swallow the keystroke — it's
        // still forwarded below so the agent sees the interrupt as usual.
        if let CrosstermEvent::Key(key) = event {
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && key.code == KeyCode::Char('c')
            {
                self.pause_focused_workflow();
            }
        }

        // Forward to active terminal.
        let mut input_err: Option<(String, std::io::Error)> = None;
        let mut handled = false;
        if let Some(ts) = self.active_session_mut() {
            if !ts.session.exited && !active_reconnecting {
                // Auto-scroll to bottom on any input so the cursor stays visible.
                {
                    use alacritty_terminal::grid::Scroll;
                    ts.session.term.lock().scroll_display(Scroll::Bottom);
                }
                let term_mode = *ts.session.term.lock().mode();
                if let Some(bytes) = crate::input::event_to_bytes(event, &term_mode) {
                    if let Err(e) = ts.session.write(&bytes) {
                        input_err = Some((ts.label.clone(), e));
                    }
                    ts.last_write_at = Some(Instant::now());
                }
                handled = true;
            }
        }
        if let Some((label, e)) = input_err {
            self.set_status_msg(&format!("input to {}: {}", label, e));
        }
        if handled {
            return true;
        }

        false
    }

    /// Handle a mouse event over the terminal pane.
    /// Returns true if the event was consumed.
    fn handle_terminal_mouse(&mut self, me: &crossterm::event::MouseEvent) -> bool {
        if !matches!(self.view_mode, ViewMode::Sessions) {
            return false;
        }
        // Terminal inner rect (after border) sits at (1,1) with last_term_size dims.
        let (term_cols, term_rows) = self.last_term_size;
        if me.column < 1 || me.row < 1
            || me.column > term_cols
            || me.row > term_rows
        {
            return false;
        }
        let grid_col = (me.column - 1) as usize;
        let viewport_row = (me.row - 1) as usize;

        let Some(ts) = self.active_session_mut() else { return false; };

        // If the inner app has enabled mouse tracking (e.g. Claude Code's
        // fullscreen renderer, or vim/less in the alternate screen), the mouse
        // belongs to the app: consume the event and forward it to the PTY instead
        // of driving our own scrollback/selection. The app manages its own scroll
        // region; in the alternate screen there is no scrollback for
        // `scroll_display` to move anyway, so handling the wheel locally just
        // makes it appear dead. Exited sessions always fall through to local
        // scrollback so leftover transcripts stay scrollable.
        let term_mode = *ts.session.term.lock().mode();
        if !ts.session.exited && term_mode.intersects(TermMode::MOUSE_MODE) {
            if let Some(bytes) =
                encode_mouse_for_pty(me, term_mode, grid_col, viewport_row)
            {
                let _ = ts.session.write(&bytes);
                ts.last_write_at = Some(Instant::now());
            }
            return true;
        }

        use alacritty_terminal::grid::Scroll;
        use alacritty_terminal::index::{Column, Point as GridPoint, Side};
        use alacritty_terminal::selection::{Selection, SelectionType};
        use alacritty_terminal::term::viewport_to_point;

        match me.kind {
            MouseEventKind::ScrollUp => {
                ts.session.term.lock().scroll_display(Scroll::Delta(3));
                true
            }
            MouseEventKind::ScrollDown => {
                ts.session.term.lock().scroll_display(Scroll::Delta(-3));
                true
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let mut term = ts.session.term.lock();
                let display_offset = term.grid().display_offset();
                let point = viewport_to_point(
                    display_offset,
                    GridPoint::new(viewport_row, Column(grid_col)),
                );
                let ty = if me.modifiers.contains(KeyModifiers::ALT) {
                    SelectionType::Block
                } else {
                    SelectionType::Simple
                };
                term.selection = Some(Selection::new(ty, point, Side::Left));
                true
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let mut term = ts.session.term.lock();
                let display_offset = term.grid().display_offset();
                let point = viewport_to_point(
                    display_offset,
                    GridPoint::new(viewport_row, Column(grid_col)),
                );
                if let Some(sel) = term.selection.as_mut() {
                    sel.update(point, Side::Right);
                }
                true
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let text = ts.session.term.lock().selection_to_string();
                if let Some(text) = text {
                    if !text.is_empty() {
                        copy_to_clipboard(&text);
                        self.set_status_msg(&format!("Copied {} chars", text.len()));
                    }
                }
                true
            }
            _ => false,
        }
    }

    /// Handle events while in input mode.
    pub(super) fn handle_input_event(&mut self, event: &CrosstermEvent) -> bool {
        // Non-Key events (resize, focus, etc.) match pre-extraction
        // behavior: while in any non-Normal input mode, the event is
        // absorbed by the modal.
        if !matches!(event, CrosstermEvent::Key(_)) {
            return true;
        }
        let urls = sorted_repo_urls(&self.config.repos);
        // Configured hosts (local + any hosts.toml entries) in config order,
        // for the A-n host picker. Cloned per keystroke; the list is tiny
        // (1-3 entries) so the cost is negligible.
        let host_ids: Vec<cm_daemon::host_id::HostId> =
            self.hosts.hosts.iter().map(|h| h.id.clone()).collect();
        let outcome = match &mut self.input_mode {
            InputMode::Normal => InputOutcome::Ignored,
            InputMode::NewSession {
                label_text,
                branch_text,
                idle_timeout_text,
                repo_url,
                seed_from,
                host_id,
                active_field,
            } => handle_new_session(
                NewSessionMut {
                    label_text,
                    branch_text,
                    idle_timeout_text,
                    repo_url,
                    seed_from,
                    host_id,
                    active_field,
                },
                InputCtx { repo_urls: &urls, host_ids: &host_ids },
                event,
            ),
            InputMode::NewTerminalSession {
                workspace_id,
                session_type,
                task_id,
                seed_from,
                active_field,
            } => handle_new_terminal_session(
                NewTerminalSessionMut {
                    workspace_id: workspace_id.as_str(),
                    session_type,
                    task_id,
                    seed_from,
                    active_field,
                },
                InputCtx { repo_urls: &urls, host_ids: &host_ids },
                event,
            ),
            InputMode::SessionSettings {
                ws_index,
                session_index,
                name,
                idle_timeout,
                burst_threshold,
                hidden,
                notify_on_idle,
                global_perms,
                color,
                seeded_from_snapshot: _,
                active_field,
            } => handle_session_settings(
                SessionSettingsMut {
                    ws_index: *ws_index,
                    session_index: *session_index,
                    name,
                    idle_timeout,
                    burst_threshold,
                    hidden,
                    notify_on_idle,
                    global_perms,
                    color,
                    active_field,
                },
                InputCtx { repo_urls: &urls, host_ids: &host_ids },
                event,
            ),
            InputMode::WorkspaceSettings { ws_index, name, color, pinned, active_field } => {
                handle_workspace_settings(
                    WorkspaceSettingsMut {
                        ws_index: *ws_index,
                        name,
                        color,
                        pinned,
                        active_field,
                    },
                    InputCtx { repo_urls: &urls, host_ids: &host_ids },
                    event,
                )
            }
            InputMode::SaveSnapshot {
                workspace_id,
                session_uid,
                name_text,
                description_text,
                active_field,
                error,
            } => handle_save_snapshot(
                SaveSnapshotMut {
                    workspace_id: workspace_id.as_str(),
                    session_uid: session_uid.as_str(),
                    name_text,
                    description_text,
                    active_field,
                    error,
                },
                InputCtx { repo_urls: &urls, host_ids: &host_ids },
                event,
            ),
            InputMode::SnapshotCatalog {
                snapshots,
                selected,
                mode,
                picker_target,
                status_msg,
            } => handle_snapshot_catalog(
                SnapshotCatalogMut {
                    snapshots,
                    selected,
                    mode,
                    picker_target: picker_target.as_ref(),
                    status_msg,
                },
                InputCtx { repo_urls: &urls, host_ids: &host_ids },
                event,
            ),
            InputMode::TaskSettings { task_id, name, color, active_field } => {
                handle_task_settings(
                    TaskSettingsMut {
                        task_id: task_id.as_str(),
                        name,
                        color,
                        active_field,
                    },
                    InputCtx { repo_urls: &urls, host_ids: &host_ids },
                    event,
                )
            }
            InputMode::WorkflowLaunchConfirm {
                ws_id,
                workflow_name,
                slots,
                active_slot,
                goal,
                cursor_task_id,
            } => handle_workflow_launch_confirm(
                WorkflowLaunchConfirmMut {
                    ws_id: ws_id.as_str(),
                    workflow_name: workflow_name.as_str(),
                    slots,
                    active_slot,
                    goal,
                    cursor_task_id: cursor_task_id.as_deref(),
                },
                InputCtx { repo_urls: &urls, host_ids: &host_ids },
                event,
            ),
            InputMode::WorkflowPicker {
                ws_id,
                focused_si,
                names,
                selected,
                cursor_task_id,
            } => handle_workflow_picker(
                WorkflowPickerMut {
                    ws_id: ws_id.as_str(),
                    focused_si: *focused_si,
                    names,
                    selected,
                    cursor_task_id: cursor_task_id.as_deref(),
                },
                InputCtx { repo_urls: &urls, host_ids: &host_ids },
                event,
            ),
            InputMode::WorkflowHistory { run_id: _ } => {
                handle_workflow_history(InputCtx { repo_urls: &urls, host_ids: &host_ids }, event)
            }
            InputMode::PastWorkspacePicker { candidates, selected } => {
                handle_past_workspace_picker(
                    candidates,
                    selected,
                    InputCtx { repo_urls: &urls, host_ids: &host_ids },
                    event,
                )
            }
            InputMode::SessionPalette { candidates, query, selected } => {
                handle_session_palette(
                    candidates,
                    query,
                    selected,
                    InputCtx { repo_urls: &urls, host_ids: &host_ids },
                    event,
                )
            }
            InputMode::TaskPeek { scroll, max_scroll, .. } => {
                handle_task_peek(scroll, *max_scroll, event)
            }
            InputMode::Confirm { action, .. } => {
                handle_confirm(action, InputCtx { repo_urls: &urls, host_ids: &host_ids }, event)
            }
        };
        self.apply_input_outcome(outcome)
    }

    /// Apply a handler's outcome to App state. Returns whether the event
    /// was consumed by an input modal (matches the legacy `bool` return
    /// of `handle_input_event` — `false` means "fall through to terminal/
    /// app keybindings", which only happens in `InputMode::Normal`).
    fn apply_input_outcome(&mut self, outcome: InputOutcome) -> bool {
        match outcome {
            InputOutcome::Ignored => false,
            InputOutcome::Consumed => true,
            InputOutcome::Cancel => {
                // If the cancelled modal was a snapshot picker invoked
                // from a parent form, re-open the form with seed_from
                // unchanged (None on first open). Otherwise the user's
                // form input would be silently lost on a picker Esc.
                let old = std::mem::replace(&mut self.input_mode, InputMode::Normal);
                if let InputMode::SnapshotCatalog {
                    picker_target: Some(target),
                    ..
                } = old
                {
                    self.reopen_form_from_picker(target, None);
                }
                true
            }
            InputOutcome::Submit(action) => {
                // Special case: picker-mode SnapshotPicked submission
                // returns to the parent form with seed_from set to the
                // chosen name. Other submits (and a no-target catalog)
                // go through the normal path.
                let old = std::mem::replace(&mut self.input_mode, InputMode::Normal);
                if let InputMode::SnapshotCatalog {
                    picker_target: Some(target),
                    ..
                } = old
                {
                    if let SubmitAction::SnapshotPicked { name } = action {
                        self.reopen_form_from_picker(target, Some(name));
                        return true;
                    }
                    // Some other submit emerged from picker mode
                    // (shouldn't happen given the catalog handler, but
                    // safe-fall back to reopening the form unchanged).
                    self.reopen_form_from_picker(target, None);
                    return true;
                }
                self.apply_submit_action(action);
                true
            }
            InputOutcome::Status(msg) => {
                // Inline validation: keep the form open, just surface the
                // message in the status bar.
                self.set_status_msg(&msg);
                true
            }
        }
    }

    /// Restore the parent form that opened the snapshot picker.
    fn reopen_form_from_picker(
        &mut self,
        target: PickerTarget,
        name: Option<String>,
    ) {
        self.input_mode = rebuild_form_from_picker(target, name);
    }

    pub(super) fn apply_submit_action(&mut self, action: SubmitAction) {
        match action {
            SubmitAction::None => {}
            SubmitAction::CreateLocalSession {
                repo_url,
                label,
                branch,
                idle_timeout_secs,
                seed_from,
                in_place,
                host_id,
            } => {
                self.create_local_session(
                    &host_id,
                    &repo_url,
                    &label,
                    branch.as_deref(),
                    idle_timeout_secs,
                    seed_from.as_deref(),
                    in_place,
                );
            }
            SubmitAction::SpawnSessionOnWorkspace {
                workspace_id,
                session_type,
                task_id,
                seed_from,
            } => {
                self.spawn_session_on_workspace(
                    &workspace_id,
                    &session_type,
                    task_id,
                    seed_from.as_deref(),
                );
            }
            SubmitAction::OpenSnapshotPickerForNewSession {
                label_text,
                branch_text,
                idle_timeout_text,
                repo_url,
                existing_seed_from,
                host_id,
            } => {
                self.open_snapshot_catalog(Some(PickerTarget::NewSession {
                    label_text,
                    branch_text,
                    idle_timeout_text,
                    repo_url,
                    existing_seed_from,
                    host_id,
                }));
            }
            SubmitAction::OpenSnapshotPickerForNewTerminalSession {
                workspace_id,
                session_type,
                task_id,
                existing_seed_from,
            } => {
                self.open_snapshot_catalog(Some(
                    PickerTarget::NewTerminalSession {
                        workspace_id,
                        session_type,
                        task_id,
                        existing_seed_from,
                    },
                ));
            }
            SubmitAction::SaveSessionSettings {
                ws_index,
                session_index,
                name,
                idle_timeout,
                burst_threshold,
                hidden,
                notify_on_idle,
                global_perms,
                color,
            } => {
                // Apply the plain fields in place, and capture the uid +
                // whether the global-perms grant changed — the grant has
                // to round-trip to the daemon (its Session-caller auth
                // keys off the DaemonSession flag), so it can't be set by
                // a local field assignment alone.
                let mut perms_change: Option<(String, bool)> = None;
                if let Some(ws) = self.workspaces.get_mut(ws_index) {
                    if let Some(ts) = ws.sessions.get_mut(session_index) {
                        if !name.trim().is_empty() {
                            ts.label = name;
                        }
                        ts.idle_timeout_secs = idle_timeout;
                        ts.burst_threshold = burst_threshold;
                        ts.hidden = hidden;
                        ts.notify_on_idle = notify_on_idle;
                        ts.color = color;
                        if ts.global_perms != global_perms {
                            perms_change = Some((ts.uid.clone(), global_perms));
                        }
                    }
                }
                self.save_session_manifest();
                match perms_change {
                    Some((uid, value)) => {
                        match self.set_session_global_perms(&uid, value) {
                            Ok(true) => self.set_status_msg(
                                "Settings saved — global permissions GRANTED",
                            ),
                            Ok(false) => self.set_status_msg(
                                "Settings saved — global permissions revoked",
                            ),
                            Err(e) => self.set_status_msg(&format!(
                                "Settings saved, but global-perms change failed: {}",
                                e,
                            )),
                        }
                    }
                    None => self.set_status_msg("Settings saved"),
                }
            }
            SubmitAction::SaveWorkspaceSettings { ws_index, name, color, pinned } => {
                let mut pinned_changed = false;
                if let Some(ws) = self.workspaces.get_mut(ws_index) {
                    // An emptied name keeps the old one (matches the old
                    // rename-only behavior); color/pinned always apply.
                    if !name.is_empty() {
                        ws.name = name;
                    }
                    ws.color = color;
                    if ws.pinned != pinned {
                        ws.pinned = pinned;
                        pinned_changed = true;
                    }
                }
                if pinned_changed {
                    self.resort_workspaces_for_pin();
                }
                self.save_session_manifest();
                self.set_status_msg("Workspace settings saved");
            }
            SubmitAction::SaveSnapshot {
                workspace_id,
                session_uid,
                name,
                description,
            } => {
                self.handle_save_snapshot_submit(
                    workspace_id,
                    session_uid,
                    name,
                    description,
                );
            }
            SubmitAction::SnapshotPicked { name } => {
                // Picker-mode result. Chunk 4 never opens the catalog in
                // picker mode; chunk 5 will wire this to the seed-from
                // field on the new-session form. For now we just toast so
                // the variant has somewhere to land if a misbehaving caller
                // emits it.
                let _ = name;
            }
            SubmitAction::SaveTaskName { task_id, name, color } => {
                // Color rides the local manifest sidecar, not the API row —
                // apply it regardless of whether the rename half is valid.
                match color {
                    Some(c) => {
                        self.task_colors.insert(task_id.clone(), c);
                    }
                    None => {
                        self.task_colors.remove(&task_id);
                    }
                }
                self.save_session_manifest();
                if !name.is_empty() {
                    if let Some(task) = self
                        .tasks
                        .iter_mut()
                        .find(|t| t.task_id.as_deref() == Some(task_id.as_str()))
                    {
                        task.name = name.clone();
                    }
                    let mut fields = HashMap::new();
                    fields.insert("name".to_string(), serde_json::Value::String(name));
                    self.backend.update_plan_task(task_id, fields);
                }
                self.set_status_msg("Task settings saved");
            }
            SubmitAction::EnterWorkflowLaunchConfirm {
                ws_id,
                focused_si,
                workflow_name,
                cursor_task_id,
            } => {
                self.enter_workflow_launch_confirm(
                    ws_id,
                    focused_si,
                    workflow_name,
                    cursor_task_id,
                );
            }
            SubmitAction::LaunchWorkflow {
                ws_id,
                workflow_name,
                slots,
                goal,
                cursor_task_id,
            } => {
                // migrate-tui-local Issue G: UI A-f launches now
                // carry the cursor's task scope (captured at
                // observation time in `open_workflow_launch` per
                // Issue I). When the cursor was on a tasked
                // session or `Cursor::Task`, the daemon records
                // the participant's task_id at spawn time —
                // matching the MCP-launched path. When the
                // cursor was workspace-scope (no task), this is
                // None and the controller's existing-slot
                // inheritance fallback still applies (Issue I
                // completes the Issue G threading for the UI
                // path).
                // Phase 4 §D/§E: A-f launches through the daemon's
                // `start_workflow` RPC (same path as the MCP tool). The daemon
                // spawns the participants, writes state.json, and drives the run
                // headlessly; the TUI observes it via `workflow_watch` /
                // `manifest.watch`. The TUI no longer spawns or drives locally.
                self.launch_workflow_via_daemon(
                    &ws_id,
                    &workflow_name,
                    &slots,
                    goal,
                    cursor_task_id,
                );
            }
            SubmitAction::MarkActiveDone => self.mark_active_done(),
            SubmitAction::DeleteActive => self.delete_active(),
            SubmitAction::StopWorkflow { run_id } => self.stop_workflow_run(&run_id),
            SubmitAction::ReopenPastWorkspace { ws_id } => {
                self.reopen_workspace_by_id(&ws_id);
            }
            SubmitAction::RestoreTombstones { ws_id } => {
                self.restore_tombstones_for_workspace(&ws_id);
            }
            SubmitAction::PaletteJump { target } => {
                self.apply_palette_jump(target);
            }
        }
    }
}

#[cfg(test)]
mod yank_clipboard_tests {
    //! A-' yank: the OSC 52 encoder, the base64-safe truncation, and the
    //! last-assistant-message extraction against a fixture transcript.

    use super::*;
    use std::collections::HashMap;

    // ── osc52_sequence: input → exact escape sequence ──

    #[test]
    fn osc52_sequence_wraps_base64_payload() {
        // "hi" → base64 "aGk=", OSC 52 clipboard-set, ST-terminated.
        assert_eq!(osc52_sequence("hi"), "\x1b]52;c;aGk=\x1b\\");
    }

    #[test]
    fn osc52_sequence_empty_payload() {
        assert_eq!(osc52_sequence(""), "\x1b]52;c;\x1b\\");
    }

    // ── truncate_utf8: boundary behavior ──

    #[test]
    fn truncate_utf8_exact_fit_is_untouched() {
        assert_eq!(truncate_utf8("abcd", 4), ("abcd", false));
    }

    #[test]
    fn truncate_utf8_over_limit_cuts_at_limit() {
        assert_eq!(truncate_utf8("abcdef", 4), ("abcd", true));
    }

    #[test]
    fn truncate_utf8_backs_off_to_char_boundary() {
        // "é" is 2 bytes: a budget landing mid-char must back off so the
        // clipboard payload stays valid UTF-8 (never panics on slicing).
        let s = "a\u{e9}\u{e9}"; // 5 bytes: a=1, é=2, é=2
        assert_eq!(truncate_utf8(s, 3), ("a\u{e9}", true));
        assert_eq!(truncate_utf8(s, 2), ("a", true));
        assert_eq!(truncate_utf8(s, 0), ("", true));
    }

    // ── last_assistant_message_text against a fixture transcript ──

    /// Minimal claude-type session bound to `sid`. Mirrors the fixture
    /// pattern of `transcript_rebind_tests::make_test_session` — /bin/true
    /// exits immediately and the PTY is never read.
    fn yank_test_session(sid: Option<&str>) -> TerminalSession {
        let session = crate::session::Session::new(
            "/bin/true",
            &[],
            80,
            24,
            None,
            HashMap::new(),
            None,
        )
        .expect("session for test");
        TerminalSession {
            color: None,
            uid: "yank-uid".into(),
            label: "yank-test".into(),
            session_type: "claude".into(),
            session,
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
            workflow_run_id: None,
            workflow_role: None,
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

    /// Run `f` with HOME pointed at a fresh tempdir (serialized via
    /// `home_lock`, restored before returning so a failing assertion can't
    /// poison other tests' HOME).
    fn with_temp_home<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
        let _g = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }
        let out = f(tmp.path());
        unsafe {
            if let Some(h) = old_home {
                std::env::set_var("HOME", h);
            } else {
                std::env::remove_var("HOME");
            }
        }
        out
    }

    #[test]
    fn last_assistant_picks_final_assistant_entry() {
        let got = with_temp_home(|home| {
            // Encoded path matches the agent module's rule: '/' and '.' → '-'.
            let proj = home.join(".claude/projects/-tmp-yankrepo");
            std::fs::create_dir_all(&proj).unwrap();
            let lines = concat!(
                r#"{"type":"user","message":{"role":"user","content":"do the thing"}}"#,
                "\n",
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"first answer"}]}}"#,
                "\n",
                r#"{"type":"user","message":{"role":"user","content":"again"}}"#,
                "\n",
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"final answer"}]}}"#,
                "\n",
            );
            std::fs::write(proj.join("yank-sid.jsonl"), lines).unwrap();

            let ts = yank_test_session(Some("yank-sid"));
            last_assistant_message_text(&ts, Path::new("/tmp/yankrepo"))
        });
        assert_eq!(
            got,
            Ok("final answer".to_string()),
            "must yank the LAST assistant message, not the first",
        );
    }

    #[test]
    fn last_assistant_errs_when_no_assistant_turns() {
        let got = with_temp_home(|home| {
            let proj = home.join(".claude/projects/-tmp-yankrepo");
            std::fs::create_dir_all(&proj).unwrap();
            let lines = concat!(
                r#"{"type":"user","message":{"role":"user","content":"hello?"}}"#,
                "\n",
            );
            std::fs::write(proj.join("user-only.jsonl"), lines).unwrap();

            let ts = yank_test_session(Some("user-only"));
            last_assistant_message_text(&ts, Path::new("/tmp/yankrepo"))
        });
        assert!(
            got.as_deref()
                .err()
                .is_some_and(|e| e.contains("no assistant messages")),
            "user-only transcript must produce the no-assistant error, got {:?}",
            got,
        );
    }

    #[test]
    fn last_assistant_errs_when_transcript_unbound() {
        // Fresh session: no transcript_id yet → the reader returns an empty
        // message list → same "no assistant messages" status, no panic.
        let got = with_temp_home(|_home| {
            let ts = yank_test_session(None);
            last_assistant_message_text(&ts, Path::new("/tmp/yankrepo"))
        });
        assert!(got.is_err());
    }
}

#[cfg(test)]
mod input_handler_tests {
    //! Per-mode handler tests. Each input mode is exercised through its
    //! free `handle_<mode>` function with synthesized `CrosstermEvent::Key`
    //! events — no `App`, no PTY, no terminal. Behaviors pinned here:
    //!   - ESC → InputOutcome::Cancel (closes the modal),
    //!   - ENTER → InputOutcome::Submit(...) (close + side effect),
    //!   - BACKSPACE → mode-payload buffer mutated in place.
    //! These guard the behavior contract that pre-extraction was only
    //! enforced by the call site of `handle_input_event`.
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> CrosstermEvent {
        CrosstermEvent::Key(KeyEvent::new(code, KeyModifiers::empty()))
    }

    fn ctx_no_repos<'a>() -> InputCtx<'a> {
        InputCtx { repo_urls: &[], host_ids: &[] }
    }

    fn assert_consumed(o: &InputOutcome) {
        assert!(
            matches!(o, InputOutcome::Consumed),
            "expected Consumed, got {:?}",
            o
        );
    }

    fn assert_cancel(o: &InputOutcome) {
        assert!(
            matches!(o, InputOutcome::Cancel),
            "expected Cancel, got {:?}",
            o
        );
    }

    // ── NewSession ────────────────────────────────────────────────

    fn new_session_state(
        label: &str,
        branch: &str,
        timeout: &str,
        repo: &str,
        active: u8,
    ) -> (String, String, String, String, cm_daemon::host_id::HostId, u8) {
        (
            label.to_string(),
            branch.to_string(),
            timeout.to_string(),
            repo.to_string(),
            cm_daemon::host_id::HostId::local(),
            active,
        )
    }

    #[test]
    fn new_session_esc_cancels() {
        let (mut label, mut branch, mut timeout, mut repo, mut host, mut active) =
            new_session_state("hello", "", "2", "", 1);
        let outcome = handle_new_session(
            NewSessionMut {
                label_text: &mut label,
                branch_text: &mut branch,
                idle_timeout_text: &mut timeout,
                repo_url: &mut repo,
                seed_from: &mut None,
                host_id: &mut host,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Esc),
        );
        assert_cancel(&outcome);
        // Buffers preserved — Cancel doesn't clear input.
        assert_eq!(label, "hello");
    }

    #[test]
    fn new_session_backspace_pops_label_buffer() {
        let (mut label, mut branch, mut timeout, mut repo, mut host, mut active) =
            new_session_state("foo", "", "2", "", 1);
        let outcome = handle_new_session(
            NewSessionMut {
                label_text: &mut label,
                branch_text: &mut branch,
                idle_timeout_text: &mut timeout,
                repo_url: &mut repo,
                seed_from: &mut None,
                host_id: &mut host,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Backspace),
        );
        assert_consumed(&outcome);
        assert_eq!(label, "fo");
    }

    #[test]
    fn new_session_enter_with_label_submits_create_local_session() {
        let (mut label, mut branch, mut timeout, mut repo, mut host, mut active) =
            new_session_state("my-task", "feat/x", "10", "https://github.com/a/b", 1);
        let outcome = handle_new_session(
            NewSessionMut {
                label_text: &mut label,
                branch_text: &mut branch,
                idle_timeout_text: &mut timeout,
                repo_url: &mut repo,
                seed_from: &mut None,
                host_id: &mut host,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        match outcome {
            InputOutcome::Submit(SubmitAction::CreateLocalSession {
                repo_url,
                label,
                branch,
                idle_timeout_secs,
                seed_from,
                in_place,
                host_id,
            }) => {
                assert_eq!(repo_url, "https://github.com/a/b");
                assert_eq!(label, "my-task");
                assert_eq!(branch.as_deref(), Some("feat/x"));
                assert_eq!(idle_timeout_secs, 10);
                assert!(seed_from.is_none());
                assert!(!in_place, "a real branch must not be in-place");
                // No host field touched → defaults to local (active_host).
                assert_eq!(host_id, cm_daemon::host_id::HostId::local());
            }
            other => panic!("expected Submit(CreateLocalSession), got {:?}", other),
        }
    }

    /// The `.` sentinel in the branch field flips to in-place: no worktree,
    /// no branch. A leading-dot path like `./foo` must NOT be treated as the
    /// sentinel (it's a real, if unusual, branch name).
    #[test]
    fn new_session_dot_branch_sets_in_place() {
        for raw in ["."] {
            let (mut label, mut branch, mut timeout, mut repo, mut host, mut active) =
                new_session_state("my-task", raw, "10", "https://github.com/a/b", 1);
            let outcome = handle_new_session(
                NewSessionMut {
                    label_text: &mut label,
                    branch_text: &mut branch,
                    idle_timeout_text: &mut timeout,
                    repo_url: &mut repo,
                    seed_from: &mut None,
                    host_id: &mut host,
                    active_field: &mut active,
                },
                ctx_no_repos(),
                &key(KeyCode::Enter),
            );
            match outcome {
                InputOutcome::Submit(SubmitAction::CreateLocalSession {
                    branch, in_place, ..
                }) => {
                    assert!(in_place, "branch {raw:?} should be in-place");
                    assert!(branch.is_none(), "in-place must carry no branch");
                }
                other => panic!("expected Submit(CreateLocalSession), got {:?}", other),
            }
        }

        // Negative: `./foo` is a real branch name, never in-place.
        let (mut label, mut branch, mut timeout, mut repo, mut host, mut active) =
            new_session_state("my-task", "./foo", "10", "https://github.com/a/b", 1);
        let outcome = handle_new_session(
            NewSessionMut {
                label_text: &mut label,
                branch_text: &mut branch,
                idle_timeout_text: &mut timeout,
                repo_url: &mut repo,
                seed_from: &mut None,
                host_id: &mut host,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        match outcome {
            InputOutcome::Submit(SubmitAction::CreateLocalSession {
                branch, in_place, ..
            }) => {
                assert!(!in_place);
                assert_eq!(branch.as_deref(), Some("./foo"));
            }
            other => panic!("expected Submit(CreateLocalSession), got {:?}", other),
        }
    }

    #[test]
    fn new_session_enter_with_blank_label_surfaces_required_message() {
        // When the label is empty, Enter keeps the modal OPEN and surfaces a
        // "Name is required" status — NOT a silent no-op (the reported "pressed
        // Enter and nothing happened"). The form stays open (Status, not Submit).
        let (mut label, mut branch, mut timeout, mut repo, mut host, mut active) =
            new_session_state("   ", "", "2", "", 1);
        let outcome = handle_new_session(
            NewSessionMut {
                label_text: &mut label,
                branch_text: &mut branch,
                idle_timeout_text: &mut timeout,
                repo_url: &mut repo,
                seed_from: &mut None,
                host_id: &mut host,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        match outcome {
            InputOutcome::Status(msg) => assert!(
                msg.to_lowercase().contains("required"),
                "expected a 'Name is required' message, got {:?}",
                msg,
            ),
            other => panic!("expected Status(required message), got {:?}", other),
        }
    }

    #[test]
    fn new_session_char_appends_only_to_active_field() {
        let (mut label, mut branch, mut timeout, mut repo, mut host, mut active) =
            new_session_state("", "", "2", "", 2);
        let outcome = handle_new_session(
            NewSessionMut {
                label_text: &mut label,
                branch_text: &mut branch,
                idle_timeout_text: &mut timeout,
                repo_url: &mut repo,
                seed_from: &mut None,
                host_id: &mut host,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Char('x')),
        );
        assert_consumed(&outcome);
        assert_eq!(label, "");
        assert_eq!(branch, "x");
    }

    #[test]
    fn new_session_right_cycles_repo_when_field_zero() {
        let urls = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let (mut label, mut branch, mut timeout, mut repo, mut host, mut active) =
            new_session_state("", "", "2", "b", 0);
        let outcome = handle_new_session(
            NewSessionMut {
                label_text: &mut label,
                branch_text: &mut branch,
                idle_timeout_text: &mut timeout,
                repo_url: &mut repo,
                seed_from: &mut None,
                host_id: &mut host,
                active_field: &mut active,
            },
            InputCtx { repo_urls: &urls, host_ids: &[] },
            &key(KeyCode::Right),
        );
        assert_consumed(&outcome);
        assert_eq!(repo, "c");
    }

    // ── NewSession seed-from (chunk 5) ────────────────────────────

    #[test]
    fn new_session_tab_cycles_through_six_fields() {
        // 0 → 1 → 2 → 3 → 4 → 5 → 0 (host picker added as field 5)
        let (mut label, mut branch, mut timeout, mut repo, mut host, mut active) =
            new_session_state("", "", "", "", 0);
        let mut seed: Option<String> = None;
        for expected in [1, 2, 3, 4, 5, 0] {
            handle_new_session(
                NewSessionMut {
                    label_text: &mut label,
                    branch_text: &mut branch,
                    idle_timeout_text: &mut timeout,
                    repo_url: &mut repo,
                    seed_from: &mut seed,
                    host_id: &mut host,
                    active_field: &mut active,
                },
                ctx_no_repos(),
                &key(KeyCode::Tab),
            );
            assert_eq!(active, expected);
        }
    }

    #[test]
    fn new_session_enter_on_seed_field_opens_picker_with_form_state() {
        let (mut label, mut branch, mut timeout, mut repo, mut host, mut active) =
            new_session_state("my-task", "feat/x", "12", "https://github.com/o/r", 4);
        let mut seed: Option<String> = None;
        let outcome = handle_new_session(
            NewSessionMut {
                label_text: &mut label,
                branch_text: &mut branch,
                idle_timeout_text: &mut timeout,
                repo_url: &mut repo,
                seed_from: &mut seed,
                host_id: &mut host,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        match outcome {
            InputOutcome::Submit(
                SubmitAction::OpenSnapshotPickerForNewSession {
                    label_text,
                    branch_text,
                    idle_timeout_text,
                    repo_url,
                    existing_seed_from,
                    host_id,
                },
            ) => {
                assert_eq!(label_text, "my-task");
                assert_eq!(branch_text, "feat/x");
                assert_eq!(idle_timeout_text, "12");
                assert_eq!(repo_url, "https://github.com/o/r");
                assert!(existing_seed_from.is_none());
                // Defaulted to local in `new_session_state`; carried through.
                assert_eq!(host_id, cm_daemon::host_id::HostId::local());
            }
            other => panic!(
                "expected OpenSnapshotPickerForNewSession, got {other:?}"
            ),
        }
    }

    #[test]
    fn new_session_esc_on_seed_field_with_value_clears_seed_only() {
        let (mut label, mut branch, mut timeout, mut repo, mut host, mut active) =
            new_session_state("x", "", "2", "", 4);
        let mut seed: Option<String> = Some("reviewer".into());
        let outcome = handle_new_session(
            NewSessionMut {
                label_text: &mut label,
                branch_text: &mut branch,
                idle_timeout_text: &mut timeout,
                repo_url: &mut repo,
                seed_from: &mut seed,
                host_id: &mut host,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Esc),
        );
        assert_consumed(&outcome);
        assert!(seed.is_none(), "seed_from should have been cleared");
        assert_eq!(label, "x", "other form fields untouched");
    }

    #[test]
    fn new_session_esc_on_other_fields_still_cancels() {
        let (mut label, mut branch, mut timeout, mut repo, mut host, mut active) =
            new_session_state("x", "", "2", "", 1);
        let mut seed: Option<String> = None;
        let outcome = handle_new_session(
            NewSessionMut {
                label_text: &mut label,
                branch_text: &mut branch,
                idle_timeout_text: &mut timeout,
                repo_url: &mut repo,
                seed_from: &mut seed,
                host_id: &mut host,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Esc),
        );
        assert_cancel(&outcome);
    }

    #[test]
    fn new_session_submit_carries_seed_from_when_set() {
        let (mut label, mut branch, mut timeout, mut repo, mut host, mut active) =
            new_session_state("task", "", "2", "https://github.com/a/b", 1);
        let mut seed: Option<String> = Some("reviewer-strict".into());
        let outcome = handle_new_session(
            NewSessionMut {
                label_text: &mut label,
                branch_text: &mut branch,
                idle_timeout_text: &mut timeout,
                repo_url: &mut repo,
                seed_from: &mut seed,
                host_id: &mut host,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        match outcome {
            InputOutcome::Submit(SubmitAction::CreateLocalSession {
                seed_from, ..
            }) => assert_eq!(seed_from.as_deref(), Some("reviewer-strict")),
            other => panic!("expected CreateLocalSession, got {other:?}"),
        }
    }

    // ── NewSession host picker ────────────────────────────────────

    /// The host field (5) cycles through the OFFERED host list — sourced
    /// from `ctx.host_ids` (the configured hosts) — with ←/→, wrapping at
    /// both ends, exactly like the repo picker at field 0.
    #[test]
    fn new_session_host_field_cycles_offered_hosts() {
        let hosts = vec![
            cm_daemon::host_id::HostId::local(),
            cm_daemon::host_id::HostId::new("manager"),
            cm_daemon::host_id::HostId::new("worker"),
        ];
        let (mut label, mut branch, mut timeout, mut repo, mut host, mut active) =
            new_session_state("task", "", "2", "https://github.com/a/b", 5);
        // Defaults to local (seeded by `new_session_state`).
        assert_eq!(host, cm_daemon::host_id::HostId::local());
        let mut press = |code: KeyCode, host: &mut cm_daemon::host_id::HostId| {
            handle_new_session(
                NewSessionMut {
                    label_text: &mut label,
                    branch_text: &mut branch,
                    idle_timeout_text: &mut timeout,
                    repo_url: &mut repo,
                    seed_from: &mut None,
                    host_id: host,
                    active_field: &mut active,
                },
                InputCtx { repo_urls: &[], host_ids: &hosts },
                &key(code),
            );
        };
        press(KeyCode::Right, &mut host);
        assert_eq!(host, cm_daemon::host_id::HostId::new("manager"));
        press(KeyCode::Right, &mut host);
        assert_eq!(host, cm_daemon::host_id::HostId::new("worker"));
        // Wraps forward to the first.
        press(KeyCode::Right, &mut host);
        assert_eq!(host, cm_daemon::host_id::HostId::local());
        // Wraps backward to the last.
        press(KeyCode::Left, &mut host);
        assert_eq!(host, cm_daemon::host_id::HostId::new("worker"));
    }

    /// Enter on the form submits `CreateLocalSession` carrying the CHOSEN
    /// host — so the create path pins the new workspace to it instead of
    /// the global active_host.
    #[test]
    fn new_session_submit_carries_chosen_host() {
        let hosts = vec![
            cm_daemon::host_id::HostId::local(),
            cm_daemon::host_id::HostId::new("manager"),
        ];
        let (mut label, mut branch, mut timeout, mut repo, mut host, mut active) =
            new_session_state("task", "", "2", "https://github.com/a/b", 5);
        // Cycle to the remote host, then submit from the host field.
        handle_new_session(
            NewSessionMut {
                label_text: &mut label,
                branch_text: &mut branch,
                idle_timeout_text: &mut timeout,
                repo_url: &mut repo,
                seed_from: &mut None,
                host_id: &mut host,
                active_field: &mut active,
            },
            InputCtx { repo_urls: &[], host_ids: &hosts },
            &key(KeyCode::Right),
        );
        let outcome = handle_new_session(
            NewSessionMut {
                label_text: &mut label,
                branch_text: &mut branch,
                idle_timeout_text: &mut timeout,
                repo_url: &mut repo,
                seed_from: &mut None,
                host_id: &mut host,
                active_field: &mut active,
            },
            InputCtx { repo_urls: &[], host_ids: &hosts },
            &key(KeyCode::Enter),
        );
        match outcome {
            InputOutcome::Submit(SubmitAction::CreateLocalSession {
                host_id, ..
            }) => assert_eq!(host_id, cm_daemon::host_id::HostId::new("manager")),
            other => panic!("expected CreateLocalSession, got {other:?}"),
        }
    }

    /// An empty offered list (no hosts in ctx) makes host cycling a no-op
    /// rather than panicking — the chosen host stays at its default.
    #[test]
    fn new_session_host_cycle_no_op_when_list_empty() {
        let (mut label, mut branch, mut timeout, mut repo, mut host, mut active) =
            new_session_state("task", "", "2", "https://github.com/a/b", 5);
        handle_new_session(
            NewSessionMut {
                label_text: &mut label,
                branch_text: &mut branch,
                idle_timeout_text: &mut timeout,
                repo_url: &mut repo,
                seed_from: &mut None,
                host_id: &mut host,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Right),
        );
        assert_eq!(host, cm_daemon::host_id::HostId::local());
    }

    // ── NewTerminalSession ────────────────────────────────────────

    #[test]
    fn new_terminal_session_j_cycles_type_forward() {
        let mut session_type = "claude".to_string();
        let task_id = None;
        let mut seed = None;
        let mut active = 0u8;
        let outcome = handle_new_terminal_session(
            NewTerminalSessionMut {
                workspace_id: "ws-7",
                session_type: &mut session_type,
                task_id: &task_id,
                seed_from: &mut seed,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Char('j')),
        );
        assert_consumed(&outcome);
        assert_eq!(session_type, "codex");
    }

    #[test]
    fn new_terminal_session_enter_submits_with_payload() {
        let mut session_type = "bash".to_string();
        let task_id = Some("t-123".to_string());
        let mut seed = None;
        let mut active = 0u8;
        let outcome = handle_new_terminal_session(
            NewTerminalSessionMut {
                workspace_id: "ws-4",
                session_type: &mut session_type,
                task_id: &task_id,
                seed_from: &mut seed,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        match outcome {
            InputOutcome::Submit(SubmitAction::SpawnSessionOnWorkspace {
                workspace_id,
                session_type,
                task_id,
                seed_from,
            }) => {
                assert_eq!(workspace_id, "ws-4");
                assert_eq!(session_type, "bash");
                assert_eq!(task_id, Some("t-123".to_string()));
                assert!(seed_from.is_none());
            }
            other => panic!("expected SpawnSessionOnWorkspace, got {:?}", other),
        }
    }

    #[test]
    fn new_terminal_session_esc_cancels() {
        let mut session_type = "claude".to_string();
        let task_id = None;
        let mut seed = None;
        let mut active = 0u8;
        let outcome = handle_new_terminal_session(
            NewTerminalSessionMut {
                workspace_id: "ws-0",
                session_type: &mut session_type,
                task_id: &task_id,
                seed_from: &mut seed,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Esc),
        );
        assert_cancel(&outcome);
    }

    // ── NewTerminalSession seed-from (chunk 5) ────────────────────

    #[test]
    fn new_terminal_session_j_on_field_0_clears_seed_from() {
        // Engine change invalidates a previously picked snapshot (picker
        // filters by engine). Must clear seed_from to avoid carrying a
        // claude-code snapshot across to a codex session.
        let mut session_type = "claude".to_string();
        let task_id = None;
        let mut seed: Option<String> = Some("reviewer".into());
        let mut active = 0u8;
        handle_new_terminal_session(
            NewTerminalSessionMut {
                workspace_id: "ws-0",
                session_type: &mut session_type,
                task_id: &task_id,
                seed_from: &mut seed,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Char('j')),
        );
        assert_eq!(session_type, "codex");
        assert!(seed.is_none(), "engine change should clear seed_from");
    }

    #[test]
    fn new_terminal_session_tab_cycles_field_0_and_1() {
        let mut session_type = "claude".to_string();
        let task_id = None;
        let mut seed: Option<String> = None;
        let mut active = 0u8;
        for expected in [1u8, 0] {
            handle_new_terminal_session(
                NewTerminalSessionMut {
                    workspace_id: "ws-0",
                    session_type: &mut session_type,
                    task_id: &task_id,
                    seed_from: &mut seed,
                    active_field: &mut active,
                },
                ctx_no_repos(),
                &key(KeyCode::Tab),
            );
            assert_eq!(active, expected);
        }
    }

    #[test]
    fn new_terminal_session_enter_on_seed_field_with_bash_is_noop() {
        let mut session_type = "bash".to_string();
        let task_id = None;
        let mut seed: Option<String> = None;
        let mut active = 1u8;
        let outcome = handle_new_terminal_session(
            NewTerminalSessionMut {
                workspace_id: "ws-0",
                session_type: &mut session_type,
                task_id: &task_id,
                seed_from: &mut seed,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        assert_consumed(&outcome);
    }

    #[test]
    fn new_terminal_session_enter_on_seed_field_with_claude_opens_picker() {
        let mut session_type = "claude".to_string();
        let task_id = Some("t-42".to_string());
        let mut seed: Option<String> = None;
        let mut active = 1u8;
        let outcome = handle_new_terminal_session(
            NewTerminalSessionMut {
                workspace_id: "ws-9",
                session_type: &mut session_type,
                task_id: &task_id,
                seed_from: &mut seed,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        match outcome {
            InputOutcome::Submit(
                SubmitAction::OpenSnapshotPickerForNewTerminalSession {
                    workspace_id,
                    session_type,
                    task_id,
                    existing_seed_from,
                },
            ) => {
                assert_eq!(workspace_id, "ws-9");
                assert_eq!(session_type, "claude");
                assert_eq!(task_id.as_deref(), Some("t-42"));
                assert!(existing_seed_from.is_none());
            }
            other => panic!(
                "expected OpenSnapshotPickerForNewTerminalSession, got {other:?}"
            ),
        }
    }

    #[test]
    fn new_terminal_session_esc_on_seed_field_with_value_clears_seed_only() {
        let mut session_type = "claude".to_string();
        let task_id = None;
        let mut seed: Option<String> = Some("reviewer".into());
        let mut active = 1u8;
        let outcome = handle_new_terminal_session(
            NewTerminalSessionMut {
                workspace_id: "ws-0",
                session_type: &mut session_type,
                task_id: &task_id,
                seed_from: &mut seed,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Esc),
        );
        assert_consumed(&outcome);
        assert!(seed.is_none());
    }

    #[test]
    fn new_terminal_session_submit_carries_seed_from() {
        let mut session_type = "claude".to_string();
        let task_id = None;
        let mut seed: Option<String> = Some("reviewer".into());
        let mut active = 0u8;
        let outcome = handle_new_terminal_session(
            NewTerminalSessionMut {
                workspace_id: "ws-1",
                session_type: &mut session_type,
                task_id: &task_id,
                seed_from: &mut seed,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        match outcome {
            InputOutcome::Submit(SubmitAction::SpawnSessionOnWorkspace {
                seed_from, ..
            }) => assert_eq!(seed_from.as_deref(), Some("reviewer")),
            other => panic!("expected SpawnSessionOnWorkspace, got {other:?}"),
        }
    }

    #[test]
    fn picker_cancel_preserves_existing_seed_from() {
        // Regression for "Picker cancel clears an existing seed_from".
        // The form had seed_from = Some("A"); the user opens the picker
        // to maybe change it, then Escs out. rebuild_form_from_picker
        // with name=None must restore the captured existing value.
        let target = PickerTarget::NewSession {
            label_text: "task".into(),
            branch_text: "br".into(),
            idle_timeout_text: "30".into(),
            repo_url: "u".into(),
            existing_seed_from: Some("snap-A".into()),
            host_id: cm_daemon::host_id::HostId::local(),
        };
        let mode = super::rebuild_form_from_picker(target, None);
        match mode {
            InputMode::NewSession { seed_from, .. } => {
                assert_eq!(seed_from.as_deref(), Some("snap-A"));
            }
            _ => panic!("expected NewSession"),
        }
    }

    #[test]
    fn picker_select_overwrites_existing_seed_from() {
        let target = PickerTarget::NewTerminalSession {
            workspace_id: "ws-3".into(),
            session_type: "claude".into(),
            task_id: None,
            existing_seed_from: Some("snap-A".into()),
        };
        let mode = super::rebuild_form_from_picker(
            target,
            Some("snap-B".into()),
        );
        match mode {
            InputMode::NewTerminalSession {
                seed_from,
                workspace_id,
                ..
            } => {
                assert_eq!(seed_from.as_deref(), Some("snap-B"));
                assert_eq!(workspace_id, "ws-3");
            }
            _ => panic!("expected NewTerminalSession"),
        }
    }

    #[test]
    fn picker_cancel_with_no_prior_seed_remains_none() {
        // No prior pick → cancel leaves seed_from None (i.e. the new
        // existing-seed_from preservation logic doesn't accidentally
        // inject something).
        let target = PickerTarget::NewSession {
            label_text: String::new(),
            branch_text: String::new(),
            idle_timeout_text: String::new(),
            repo_url: String::new(),
            existing_seed_from: None,
            host_id: cm_daemon::host_id::HostId::local(),
        };
        let mode = super::rebuild_form_from_picker(target, None);
        match mode {
            InputMode::NewSession { seed_from, .. } => {
                assert!(seed_from.is_none())
            }
            _ => panic!("expected NewSession"),
        }
    }

    #[test]
    fn cleanup_failed_clone_removes_transcript_file() {
        // Regression for "Seeded A-s leaves clone artifacts when spawn
        // fails". If a later step (build_args or spawn) errors after
        // clone_into_session has written the seed transcript, the file
        // must be removed so the next retry isn't blocked by
        // `AlreadyExists` from clone_into_session.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");
        std::fs::write(&path, b"seed\n").unwrap();
        let cloned = agent_memory::ClonedSession {
            transcript_id: "tid".into(),
            transcript_path: path.clone(),
            rollback: agent_memory::ClonedRollback::default(),
        };
        assert!(path.exists());
        App::cleanup_failed_clone(&cloned);
        assert!(!path.exists(), "transcript should have been removed");
    }

    #[test]
    fn cleanup_failed_clone_is_idempotent_on_missing_file() {
        // Called twice or against a never-written path: must not panic
        // or surface an error to the user. We're a best-effort cleanup.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("never-existed.jsonl");
        let cloned = agent_memory::ClonedSession {
            transcript_id: "tid".into(),
            transcript_path: path,
            rollback: agent_memory::ClonedRollback::default(),
        };
        App::cleanup_failed_clone(&cloned); // no panic
    }

    #[test]
    fn cleanup_failed_clone_restores_overwritten_memory_files() {
        // End-to-end rollback for the Claude memory-merge case. Build a
        // fake worktree with a user's MEMORY.md, clone a snapshot whose
        // memory dir contains a same-named file (the merge overwrites),
        // then call cleanup_failed_clone and assert the user's original
        // bytes are restored.
        use crate::test_support::home_lock;
        let dir = tempfile::tempdir().unwrap();
        let fake_home = dir.path();
        let worktree = fake_home.join("workspace");
        std::fs::create_dir_all(&worktree).unwrap();

        let projects_dir = fake_home.join(".claude/projects").join(
            worktree
                .to_string_lossy()
                .replace('/', "-")
                .replace('.', "-"),
        );
        let memory_dir = projects_dir.join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();
        let user_memory = memory_dir.join("MEMORY.md");
        std::fs::write(&user_memory, b"USER ORIGINAL CONTENT").unwrap();

        // Build a snapshot on disk with a competing MEMORY.md.
        let snap_root = fake_home.join(".cm/agent-memories");
        let snap_dir = snap_root.join("reviewer");
        std::fs::create_dir_all(snap_dir.join("memory")).unwrap();
        std::fs::write(
            snap_dir.join("memory/MEMORY.md"),
            b"SNAPSHOT MEMORY",
        )
        .unwrap();
        std::fs::write(snap_dir.join("transcript.jsonl"), b"line\n").unwrap();
        let manifest = serde_json::json!({
            "version": agent_memory::MANIFEST_VERSION,
            "description": "",
            "engine": "claude-code",
            "source_session_uid": "ts-x",
            "source_transcript_id": "tid-1",
            "source_cwd": "/tmp",
            "created_at_unix": 0,
            "transcript_bytes": 5,
            "memory_files": 1,
        });
        std::fs::write(
            snap_dir.join("manifest.json"),
            manifest.to_string(),
        )
        .unwrap();

        let _guard = home_lock();
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", fake_home) };

        let snap = agent_memory::load("reviewer").unwrap();
        let cloned =
            agent_memory::clone_into_session(&snap, &worktree).unwrap();

        // Sanity: after clone the user's file was overwritten and the
        // transcript exists.
        assert_eq!(
            std::fs::read(&user_memory).unwrap(),
            b"SNAPSHOT MEMORY",
            "clone should have merged snapshot memory over user's file"
        );
        assert!(cloned.transcript_path.exists());

        // Simulate the post-clone failure path.
        App::cleanup_failed_clone(&cloned);

        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        assert!(!cloned.transcript_path.exists(), "transcript removed");
        assert_eq!(
            std::fs::read(&user_memory).unwrap(),
            b"USER ORIGINAL CONTENT",
            "user's MEMORY.md must be byte-restored after cleanup",
        );
    }

    #[test]
    fn cleanup_failed_clone_removes_newly_created_memory_artifacts() {
        // When the worktree had no MEMORY.md before the clone, the file
        // is "newly created" and rollback should remove it (and the
        // memory dir if we created that too). A subsequent unseeded
        // session would then see no snapshot memory at all.
        use crate::test_support::home_lock;
        let dir = tempfile::tempdir().unwrap();
        let fake_home = dir.path();
        let worktree = fake_home.join("workspace");
        std::fs::create_dir_all(&worktree).unwrap();

        let projects_dir = fake_home.join(".claude/projects").join(
            worktree
                .to_string_lossy()
                .replace('/', "-")
                .replace('.', "-"),
        );
        // No memory dir/files yet — fresh worktree.

        let snap_root = fake_home.join(".cm/agent-memories");
        let snap_dir = snap_root.join("primed");
        std::fs::create_dir_all(snap_dir.join("memory")).unwrap();
        std::fs::write(
            snap_dir.join("memory/MEMORY.md"),
            b"SNAPSHOT MEMORY",
        )
        .unwrap();
        std::fs::write(snap_dir.join("transcript.jsonl"), b"line\n").unwrap();
        let manifest = serde_json::json!({
            "version": agent_memory::MANIFEST_VERSION,
            "description": "",
            "engine": "claude-code",
            "source_session_uid": "ts-y",
            "source_transcript_id": "tid-2",
            "source_cwd": "/tmp",
            "created_at_unix": 0,
            "transcript_bytes": 5,
            "memory_files": 1,
        });
        std::fs::write(
            snap_dir.join("manifest.json"),
            manifest.to_string(),
        )
        .unwrap();

        let _guard = home_lock();
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", fake_home) };

        let snap = agent_memory::load("primed").unwrap();
        let cloned =
            agent_memory::clone_into_session(&snap, &worktree).unwrap();
        let dst_memory = projects_dir.join("memory/MEMORY.md");
        assert_eq!(std::fs::read(&dst_memory).unwrap(), b"SNAPSHOT MEMORY");

        App::cleanup_failed_clone(&cloned);

        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        assert!(
            !dst_memory.exists(),
            "newly-created memory file should be removed"
        );
        assert!(
            !projects_dir.join("memory").exists(),
            "newly-created memory dir should be removed after cleanup"
        );
    }

    #[test]
    fn catalog_open_list_failure_with_picker_restores_form() {
        // Regression: list() failure inside open_snapshot_catalog used
        // to set a toast and return — silently dropping the captured
        // PickerTarget form state. The fix routes through
        // rebuild_form_from_picker, preserving every typed field plus
        // any existing seed_from.
        let target = PickerTarget::NewSession {
            label_text: "task".into(),
            branch_text: "feat/x".into(),
            idle_timeout_text: "30".into(),
            repo_url: "https://github.com/o/r".into(),
            existing_seed_from: Some("prior-snap".into()),
            host_id: cm_daemon::host_id::HostId::local(),
        };
        let err = agent_memory::SnapshotError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "denied",
        ));
        let (mode, status) =
            super::catalog_open_outcome(Err(err), Some(target));
        match mode {
            InputMode::NewSession {
                label_text,
                branch_text,
                idle_timeout_text,
                repo_url,
                seed_from,
                ..
            } => {
                assert_eq!(label_text, "task");
                assert_eq!(branch_text, "feat/x");
                assert_eq!(idle_timeout_text, "30");
                assert_eq!(repo_url, "https://github.com/o/r");
                assert_eq!(seed_from.as_deref(), Some("prior-snap"));
            }
            _ => panic!("expected restored NewSession form"),
        }
        let msg = status.expect("error should surface as status_msg");
        assert!(
            msg.contains("Could not list snapshots"),
            "unexpected status: {msg:?}"
        );
    }

    #[test]
    fn catalog_open_list_failure_without_picker_drops_to_normal() {
        let err = agent_memory::SnapshotError::NotFound;
        let (mode, status) = super::catalog_open_outcome(Err(err), None);
        assert!(matches!(mode, InputMode::Normal));
        assert!(status.is_some());
    }

    #[test]
    fn catalog_open_success_filters_by_picker_engine() {
        // Picker for a codex form must only show codex snapshots.
        let snaps = vec![
            agent_memory::Snapshot {
                name: "claude-one".into(),
                dir: std::path::PathBuf::from("/dev/null"),
                manifest: agent_memory::Manifest {
                    version: agent_memory::MANIFEST_VERSION,
                    description: String::new(),
                    engine: Engine::ClaudeCode,
                    source_session_uid: "u".into(),
                    source_transcript_id: "tid".into(),
                    source_cwd: std::path::PathBuf::from("/tmp"),
                    created_at_unix: 0,
                    transcript_bytes: 0,
                    memory_files: 0,
                },
            },
            agent_memory::Snapshot {
                name: "codex-one".into(),
                dir: std::path::PathBuf::from("/dev/null"),
                manifest: agent_memory::Manifest {
                    version: agent_memory::MANIFEST_VERSION,
                    description: String::new(),
                    engine: Engine::Codex,
                    source_session_uid: "u".into(),
                    source_transcript_id: "tid".into(),
                    source_cwd: std::path::PathBuf::from("/tmp"),
                    created_at_unix: 0,
                    transcript_bytes: 0,
                    memory_files: 0,
                },
            },
        ];
        let target = PickerTarget::NewTerminalSession {
            workspace_id: "ws-0".into(),
            session_type: "codex".into(),
            task_id: None,
            existing_seed_from: None,
        };
        let (mode, _) = super::catalog_open_outcome(Ok(snaps), Some(target));
        match mode {
            InputMode::SnapshotCatalog { snapshots, .. } => {
                assert_eq!(snapshots.len(), 1);
                assert_eq!(snapshots[0].name, "codex-one");
            }
            _ => panic!("expected SnapshotCatalog"),
        }
    }

    #[test]
    fn validate_seed_loadable_rejects_nonexistent_snapshot() {
        // Pointed at a fresh HOME with no snapshots; `load` returns
        // NotFound, the helper surfaces it as a user-facing error
        // string. `create_local_session` consults this BEFORE
        // `worktree::create_worktree`, so a bad seed name no longer
        // leaves an orphan worktree + branch on disk.
        use crate::test_support::home_lock;
        let dir = tempfile::tempdir().unwrap();
        let _guard = home_lock();
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", dir.path()) };

        let result = super::validate_seed_loadable("ghost-snapshot");

        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        let msg = result.expect_err("missing snapshot should fail validation");
        assert!(
            msg.starts_with("Snapshot load failed: "),
            "unexpected error message: {msg:?}"
        );
    }

    #[test]
    fn validate_seed_loadable_accepts_real_snapshot() {
        // Counterpart: a real on-disk snapshot returns Ok so the
        // worktree creation proceeds.
        use crate::test_support::home_lock;
        let dir = tempfile::tempdir().unwrap();
        let fake_home = dir.path();
        let snap_root = fake_home.join(".cm/agent-memories");
        let snap_dir = snap_root.join("real");
        std::fs::create_dir_all(&snap_dir).unwrap();
        let manifest = serde_json::json!({
            "version": agent_memory::MANIFEST_VERSION,
            "description": "",
            "engine": "claude-code",
            "source_session_uid": "ts-x",
            "source_transcript_id": "tid-1",
            "source_cwd": "/tmp",
            "created_at_unix": 0,
            "transcript_bytes": 0,
            "memory_files": 0,
        });
        std::fs::write(snap_dir.join("manifest.json"), manifest.to_string())
            .unwrap();
        std::fs::write(snap_dir.join("transcript.jsonl"), b"line\n").unwrap();

        let _guard = home_lock();
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", fake_home) };

        let result = super::validate_seed_loadable("real");

        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        assert!(result.is_ok(), "real snapshot should pass validation: {result:?}");
    }

    #[test]
    fn picker_round_trip_targets_workspace_by_stable_id() {
        // Regression: PickerTarget::NewTerminalSession used to store a
        // positional ws_index. While the picker was open, backend events
        // (reconcile_tasks) could reorder workspaces and the form would
        // reopen pointing at the wrong slot. After the fix the target
        // carries workspace_id, so the form always reopens on the
        // workspace the user actually invoked the picker from.
        let target = PickerTarget::NewTerminalSession {
            workspace_id: "ws-B".into(),
            session_type: "claude".into(),
            task_id: Some("t-1".into()),
            existing_seed_from: None,
        };

        // Simulate: workspaces were [A, B, C] when picker opened; while
        // the picker was open a reconcile reordered them to [C, B, A].
        // The form's workspace_id must still be ws-B (not whatever sits
        // at index 1 now, which happens to be B in this case but would
        // diverge in other orderings).
        let workspaces = vec![
            ws_fixture("ws-C", &[]),
            ws_fixture("ws-B", &[]),
            ws_fixture("ws-A", &[]),
        ];
        let mode = super::rebuild_form_from_picker(target, Some("snap-X".into()));
        let workspace_id = match mode {
            InputMode::NewTerminalSession {
                workspace_id,
                seed_from,
                ..
            } => {
                assert_eq!(seed_from.as_deref(), Some("snap-X"));
                workspace_id
            }
            _ => panic!("expected NewTerminalSession"),
        };
        // The reopened form still references workspace B by id.
        assert_eq!(workspace_id, "ws-B");
        // And resolving against the reordered vec lands on B's current
        // position — index 1 now (because of the swap), but the lookup
        // would still work even if it had moved further.
        let resolved =
            super::resolve_workspace_by_id(&workspaces, &workspace_id);
        assert_eq!(resolved, Some(1));
        assert_eq!(workspaces[resolved.unwrap()].id, "ws-B");
    }

    #[test]
    fn resolve_workspace_by_id_returns_none_when_workspace_removed() {
        // The "workspace vanished between open and submit" path —
        // spawn_session_on_workspace bails on None rather than spawning
        // into whatever (unrelated) workspace happens to share the
        // old index.
        let workspaces = vec![
            ws_fixture("ws-A", &[]),
            ws_fixture("ws-C", &[]),
        ];
        assert!(
            super::resolve_workspace_by_id(&workspaces, "ws-B").is_none(),
            "lookup must return None for a removed workspace"
        );
    }

    #[test]
    fn picker_target_engine_resolves_per_target() {
        assert_eq!(
            super::picker_target_engine(&PickerTarget::NewSession {
                label_text: String::new(),
                branch_text: String::new(),
                idle_timeout_text: String::new(),
                repo_url: String::new(),
                existing_seed_from: None,
                host_id: cm_daemon::host_id::HostId::local(),
            }),
            Some(Engine::ClaudeCode)
        );
        assert_eq!(
            super::picker_target_engine(&PickerTarget::NewTerminalSession {
                workspace_id: "ws-0".into(),
                session_type: "claude".into(),
                task_id: None,
                existing_seed_from: None,
            }),
            Some(Engine::ClaudeCode)
        );
        assert_eq!(
            super::picker_target_engine(&PickerTarget::NewTerminalSession {
                workspace_id: "ws-0".into(),
                session_type: "codex".into(),
                task_id: None,
                existing_seed_from: None,
            }),
            Some(Engine::Codex)
        );
        assert_eq!(
            super::picker_target_engine(&PickerTarget::NewTerminalSession {
                workspace_id: "ws-0".into(),
                session_type: "bash".into(),
                task_id: None,
                existing_seed_from: None,
            }),
            None
        );
    }

    // ── SessionSettings ───────────────────────────────────────────

    fn session_settings_state(
        active: u8,
    ) -> (String, String, String, bool, bool, u8) {
        (
            "label".to_string(),
            "30".to_string(),
            "5".to_string(),
            false,
            false,
            active,
        )
    }

    #[test]
    fn session_settings_tab_cycles_active_field() {
        let (mut name, mut idle, mut burst, mut hidden, mut notify, mut active) =
            session_settings_state(0);
            let mut gp = false;
            let mut color: Option<String> = None;
        let outcome = handle_session_settings(
            SessionSettingsMut {
                ws_index: 1,
                session_index: 2,
                name: &mut name,
                idle_timeout: &mut idle,
                burst_threshold: &mut burst,
                hidden: &mut hidden,
                notify_on_idle: &mut notify,
                global_perms: &mut gp,
                color: &mut color,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Tab),
        );
        assert_consumed(&outcome);
        assert_eq!(active, 1);
    }

    #[test]
    fn session_settings_backspace_pops_name_when_field_zero() {
        let (mut name, mut idle, mut burst, mut hidden, mut notify, mut active) =
            session_settings_state(0);
            let mut gp = false;
            let mut color: Option<String> = None;
        let outcome = handle_session_settings(
            SessionSettingsMut {
                ws_index: 0,
                session_index: 0,
                name: &mut name,
                idle_timeout: &mut idle,
                burst_threshold: &mut burst,
                hidden: &mut hidden,
                notify_on_idle: &mut notify,
                global_perms: &mut gp,
                color: &mut color,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Backspace),
        );
        assert_consumed(&outcome);
        assert_eq!(name, "labe");
    }

    #[test]
    fn session_settings_space_toggles_hidden_when_field_three() {
        let (mut name, mut idle, mut burst, mut hidden, mut notify, mut active) =
            session_settings_state(3);
            let mut gp = false;
            let mut color: Option<String> = None;
        hidden = false;
        let outcome = handle_session_settings(
            SessionSettingsMut {
                ws_index: 0,
                session_index: 0,
                name: &mut name,
                idle_timeout: &mut idle,
                burst_threshold: &mut burst,
                hidden: &mut hidden,
                notify_on_idle: &mut notify,
                global_perms: &mut gp,
                color: &mut color,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Char(' ')),
        );
        assert_consumed(&outcome);
        assert!(hidden);
    }

    #[test]
    fn session_settings_enter_submits_save() {
        let (mut name, mut idle, mut burst, mut hidden, mut notify, mut active) =
            session_settings_state(0);
            let mut gp = false;
            let mut color: Option<String> = None;
        let outcome = handle_session_settings(
            SessionSettingsMut {
                ws_index: 4,
                session_index: 9,
                name: &mut name,
                idle_timeout: &mut idle,
                burst_threshold: &mut burst,
                hidden: &mut hidden,
                notify_on_idle: &mut notify,
                global_perms: &mut gp,
                color: &mut color,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        match outcome {
            InputOutcome::Submit(SubmitAction::SaveSessionSettings {
                ws_index,
                session_index,
                name,
                idle_timeout,
                burst_threshold,
                hidden,
                notify_on_idle,
                global_perms: _,
                color: _,
            }) => {
                assert_eq!(ws_index, 4);
                assert_eq!(session_index, 9);
                assert_eq!(name, "label");
                assert_eq!(idle_timeout, 30);
                assert_eq!(burst_threshold, 5);
                assert!(!hidden);
                assert!(!notify_on_idle);
            }
            other => panic!("expected SaveSessionSettings, got {:?}", other),
        }
    }

    // ── WorkspaceSettings ─────────────────────────────────────────

    #[test]
    fn workspace_settings_char_appends() {
        let mut name = "foo".to_string();
        let (mut color, mut pinned, mut active) = (None::<String>, false, 0u8);
        let outcome = handle_workspace_settings(
            WorkspaceSettingsMut {
                ws_index: 0,
                name: &mut name,
                color: &mut color,
                pinned: &mut pinned,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Char('x')),
        );
        assert_consumed(&outcome);
        assert_eq!(name, "foox");
    }

    #[test]
    fn workspace_settings_backspace_pops() {
        let mut name = "abc".to_string();
        let (mut color, mut pinned, mut active) = (None::<String>, false, 0u8);
        let outcome = handle_workspace_settings(
            WorkspaceSettingsMut {
                ws_index: 0,
                name: &mut name,
                color: &mut color,
                pinned: &mut pinned,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Backspace),
        );
        assert_consumed(&outcome);
        assert_eq!(name, "ab");
    }

    #[test]
    fn workspace_settings_enter_submits_trimmed_name() {
        let mut name = "  hello  ".to_string();
        let (mut color, mut pinned, mut active) = (None::<String>, false, 0u8);
        let outcome = handle_workspace_settings(
            WorkspaceSettingsMut {
                ws_index: 3,
                name: &mut name,
                color: &mut color,
                pinned: &mut pinned,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        match outcome {
            InputOutcome::Submit(SubmitAction::SaveWorkspaceSettings {
                ws_index,
                name,
                color,
                pinned,
            }) => {
                assert_eq!(ws_index, 3);
                assert_eq!(name, "hello");
                assert_eq!(color, None);
                assert!(!pinned);
            }
            other => panic!("expected SaveWorkspaceSettings, got {:?}", other),
        }
    }

    #[test]
    fn workspace_settings_esc_cancels() {
        let mut name = "n".to_string();
        let (mut color, mut pinned, mut active) = (None::<String>, false, 0u8);
        let outcome = handle_workspace_settings(
            WorkspaceSettingsMut {
                ws_index: 0,
                name: &mut name,
                color: &mut color,
                pinned: &mut pinned,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Esc),
        );
        assert_cancel(&outcome);
    }

    // ── SaveSnapshot ──────────────────────────────────────────────

    fn save_snapshot_state<'a>(
        name: &'a mut String,
        desc: &'a mut String,
        active: &'a mut u8,
        error: &'a mut Option<String>,
    ) -> SaveSnapshotMut<'a> {
        SaveSnapshotMut {
            workspace_id: "ws-1",
            session_uid: "uid-1",
            name_text: name,
            description_text: desc,
            active_field: active,
            error,
        }
    }

    #[test]
    fn save_snapshot_char_routes_to_active_field() {
        let mut name = "fo".to_string();
        let mut desc = "ba".to_string();
        let mut active = 0u8;
        let mut error = None;

        let outcome = handle_save_snapshot(
            save_snapshot_state(&mut name, &mut desc, &mut active, &mut error),
            ctx_no_repos(),
            &key(KeyCode::Char('o')),
        );
        assert_consumed(&outcome);
        assert_eq!(name, "foo");
        assert_eq!(desc, "ba");

        active = 1;
        let outcome = handle_save_snapshot(
            save_snapshot_state(&mut name, &mut desc, &mut active, &mut error),
            ctx_no_repos(),
            &key(KeyCode::Char('r')),
        );
        assert_consumed(&outcome);
        assert_eq!(name, "foo");
        assert_eq!(desc, "bar");
    }

    #[test]
    fn save_snapshot_tab_cycles_field_and_clears_error() {
        let mut name = "x".to_string();
        let mut desc = String::new();
        let mut active = 0u8;
        let mut error = Some("prior".to_string());

        let outcome = handle_save_snapshot(
            save_snapshot_state(&mut name, &mut desc, &mut active, &mut error),
            ctx_no_repos(),
            &key(KeyCode::Tab),
        );
        assert_consumed(&outcome);
        assert_eq!(active, 1);
        assert!(error.is_none(), "tab should clear stale error");
    }

    #[test]
    fn save_snapshot_typing_clears_error() {
        let mut name = String::new();
        let mut desc = String::new();
        let mut active = 0u8;
        let mut error = Some("prior".to_string());

        let outcome = handle_save_snapshot(
            save_snapshot_state(&mut name, &mut desc, &mut active, &mut error),
            ctx_no_repos(),
            &key(KeyCode::Char('a')),
        );
        assert_consumed(&outcome);
        assert!(error.is_none(), "typing should clear stale error");
    }

    #[test]
    fn save_snapshot_enter_submits_trimmed() {
        let mut name = "  reviewer-strict  ".to_string();
        let mut desc = "  hello  ".to_string();
        let mut active = 0u8;
        let mut error = None;

        let outcome = handle_save_snapshot(
            SaveSnapshotMut {
                workspace_id: "ws-target",
                session_uid: "uid-target",
                name_text: &mut name,
                description_text: &mut desc,
                active_field: &mut active,
                error: &mut error,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        match outcome {
            InputOutcome::Submit(SubmitAction::SaveSnapshot {
                workspace_id,
                session_uid,
                name,
                description,
            }) => {
                assert_eq!(workspace_id, "ws-target");
                assert_eq!(session_uid, "uid-target");
                assert_eq!(name, "reviewer-strict");
                assert_eq!(description, "hello");
            }
            other => panic!("expected SaveSnapshot, got {other:?}"),
        }
    }

    #[test]
    fn resolve_session_by_ids_follows_reordered_workspaces() {
        // Regression for stale-index bug: the modal stores (workspace_id,
        // session_uid). Even after the backend reorders self.workspaces,
        // submit-time resolution must still point at the originally-
        // targeted session — not whatever happens to be at the old index.

        // Initial order: [A, B]. Target = ws-A, uid-A2 → (0, 1).
        let mut workspaces = vec![
            ws_fixture("ws-A", &[("uid-A1", "claude"), ("uid-A2", "claude")]),
            ws_fixture("ws-B", &[("uid-B1", "codex")]),
        ];
        assert_eq!(
            super::resolve_session_by_ids(&workspaces, "ws-A", "uid-A2"),
            Some((0, 1)),
        );

        // Backend swaps order in place: [B, A]. Same IDs → (1, 1).
        workspaces.swap(0, 1);
        assert_eq!(
            super::resolve_session_by_ids(&workspaces, "ws-A", "uid-A2"),
            Some((1, 1)),
        );

        // Target session removed → None (caller toasts and bails).
        let wi_a = workspaces.iter().position(|w| w.id == "ws-A").unwrap();
        workspaces[wi_a].sessions.retain(|s| s.uid != "uid-A2");
        assert!(
            super::resolve_session_by_ids(&workspaces, "ws-A", "uid-A2").is_none()
        );

        // Workspace removed → None.
        workspaces.retain(|w| w.id != "ws-A");
        assert!(
            super::resolve_session_by_ids(&workspaces, "ws-A", "uid-A1").is_none()
        );
    }

    /// Build a `Workspace` with the requested `(uid, session_type)` sessions.
    /// Bash sessions are used (no transcript, no PTY work) so the helper
    /// can synthesize them cheaply for resolver-only tests.
    fn ws_fixture(id: &str, sessions: &[(&str, &str)]) -> Workspace {
        use std::collections::HashMap;
        let mut out = Workspace {
            color: None,
            pinned: false,
            id: id.to_string(),
            name: id.to_string(),
            is_closed: false,
            is_cloud: false,
            repo_url: None,
            worktree_path: None,
            main_repo_path: None,
            worker_vm: None,
            worker_zone: None,
            host_id: cm_daemon::host_id::HostId::local(),
            sessions: Vec::new(),
            tombstones: Vec::new(),
            is_pushing: false,
        };
        for (uid, ty) in sessions {
            let session = crate::session::Session::new(
                "/bin/true",
                &[],
                80,
                24,
                None,
                HashMap::new(),
                None,
            )
            .expect("dummy session for fixture");
            out.sessions.push(TerminalSession {
                color: None,
                uid: (*uid).into(),
                label: (*uid).into(),
                session_type: (*ty).into(),
                session,
                status: SessionStatus::Idle,
                idle_since: None,
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
            });
        }
        out
    }

    #[test]
    fn save_snapshot_esc_cancels() {
        let mut name = "x".to_string();
        let mut desc = String::new();
        let mut active = 0u8;
        let mut error = None;
        let outcome = handle_save_snapshot(
            save_snapshot_state(&mut name, &mut desc, &mut active, &mut error),
            ctx_no_repos(),
            &key(KeyCode::Esc),
        );
        assert_cancel(&outcome);
    }

    // ── SnapshotCatalog ───────────────────────────────────────────

    fn fake_snapshot(name: &str) -> agent_memory::Snapshot {
        use agent_memory::{Manifest, Snapshot, MANIFEST_VERSION};
        Snapshot {
            name: name.to_string(),
            dir: std::path::PathBuf::from("/dev/null"),
            manifest: Manifest {
                version: MANIFEST_VERSION,
                description: String::new(),
                engine: Engine::ClaudeCode,
                source_session_uid: "uid".into(),
                source_transcript_id: "tid".into(),
                source_cwd: std::path::PathBuf::from("/tmp"),
                created_at_unix: 0,
                transcript_bytes: 0,
                memory_files: 0,
            },
        }
    }

    fn alt(code: KeyCode) -> CrosstermEvent {
        CrosstermEvent::Key(crossterm::event::KeyEvent::new(
            code,
            KeyModifiers::ALT,
        ))
    }

    #[test]
    fn catalog_jk_wraps_around_list() {
        let mut snaps = vec![fake_snapshot("a"), fake_snapshot("b"), fake_snapshot("c")];
        let mut selected = 0usize;
        let mut mode = CatalogMode::Browse;

        // j j j → wrap back to 0
        for _ in 0..3 {
            handle_snapshot_catalog(
                SnapshotCatalogMut {
                    snapshots: &mut snaps,
                    selected: &mut selected,
                    mode: &mut mode,
                    picker_target: None,
                    status_msg: &mut None,
                },
                ctx_no_repos(),
                &key(KeyCode::Char('j')),
            );
        }
        assert_eq!(selected, 0);

        // k → wraps to last
        handle_snapshot_catalog(
            SnapshotCatalogMut {
                snapshots: &mut snaps,
                selected: &mut selected,
                mode: &mut mode,
                picker_target: None,
                status_msg: &mut None,
            },
            ctx_no_repos(),
            &key(KeyCode::Char('k')),
        );
        assert_eq!(selected, 2);
    }

    #[test]
    fn catalog_browse_enter_opens_detail() {
        let mut snaps = vec![fake_snapshot("a")];
        let mut selected = 0usize;
        let mut mode = CatalogMode::Browse;
        let outcome = handle_snapshot_catalog(
            SnapshotCatalogMut {
                snapshots: &mut snaps,
                selected: &mut selected,
                mode: &mut mode,
                picker_target: None,
                status_msg: &mut None,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        assert_consumed(&outcome);
        assert!(matches!(mode, CatalogMode::Detail { .. }));
    }

    #[test]
    fn catalog_picker_enter_submits_snapshot_name() {
        let mut snaps = vec![fake_snapshot("alpha"), fake_snapshot("beta")];
        let mut selected = 1usize;
        let mut mode = CatalogMode::Browse;
        let outcome = handle_snapshot_catalog(
            SnapshotCatalogMut {
                snapshots: &mut snaps,
                selected: &mut selected,
                mode: &mut mode,
                picker_target: Some(&PickerTarget::NewSession {
                    label_text: String::new(),
                    branch_text: String::new(),
                    idle_timeout_text: String::new(),
                    repo_url: String::new(),
                    existing_seed_from: None,
                    host_id: cm_daemon::host_id::HostId::local(),
                }),
                status_msg: &mut None,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        match outcome {
            InputOutcome::Submit(SubmitAction::SnapshotPicked { name }) => {
                assert_eq!(name, "beta");
            }
            other => panic!("expected SnapshotPicked, got {other:?}"),
        }
    }

    #[test]
    fn catalog_browse_r_opens_rename_with_current_name() {
        let mut snaps = vec![fake_snapshot("alpha")];
        let mut selected = 0usize;
        let mut mode = CatalogMode::Browse;
        handle_snapshot_catalog(
            SnapshotCatalogMut {
                snapshots: &mut snaps,
                selected: &mut selected,
                mode: &mut mode,
                picker_target: None,
                status_msg: &mut None,
            },
            ctx_no_repos(),
            &key(KeyCode::Char('r')),
        );
        match mode {
            CatalogMode::Rename { text, error } => {
                assert_eq!(text, "alpha");
                assert!(error.is_none());
            }
            other => panic!("expected Rename, got {other:?}"),
        }
    }

    #[test]
    fn catalog_browse_d_opens_confirm_delete() {
        let mut snaps = vec![fake_snapshot("alpha")];
        let mut selected = 0usize;
        let mut mode = CatalogMode::Browse;
        handle_snapshot_catalog(
            SnapshotCatalogMut {
                snapshots: &mut snaps,
                selected: &mut selected,
                mode: &mut mode,
                picker_target: None,
                status_msg: &mut None,
            },
            ctx_no_repos(),
            &key(KeyCode::Char('d')),
        );
        assert!(matches!(mode, CatalogMode::ConfirmDelete));
    }

    #[test]
    fn catalog_picker_disables_rename_and_delete() {
        let mut snaps = vec![fake_snapshot("alpha")];
        let mut selected = 0usize;

        for ch in ['r', 'd'] {
            let mut mode = CatalogMode::Browse;
            handle_snapshot_catalog(
                SnapshotCatalogMut {
                    snapshots: &mut snaps,
                    selected: &mut selected,
                    mode: &mut mode,
                    picker_target: Some(&PickerTarget::NewSession {
                        label_text: String::new(),
                        branch_text: String::new(),
                        idle_timeout_text: String::new(),
                        repo_url: String::new(),
                        existing_seed_from: None,
                        host_id: cm_daemon::host_id::HostId::local(),
                    }),
                    status_msg: &mut None,
                },
                ctx_no_repos(),
                &key(KeyCode::Char(ch)),
            );
            assert!(
                matches!(mode, CatalogMode::Browse),
                "picker mode should ignore `{ch}`, stayed in: {mode:?}"
            );
        }
    }

    #[test]
    fn catalog_alt_z_cancels_from_any_sub_mode() {
        let mut snaps = vec![fake_snapshot("alpha")];
        let mut selected = 0usize;

        for start in [
            CatalogMode::Browse,
            CatalogMode::Detail {
                head: Vec::new(),
                tail: Vec::new(),
            },
            CatalogMode::Rename {
                text: "x".into(),
                error: None,
            },
            CatalogMode::ConfirmDelete,
        ] {
            let mut mode = start;
            let outcome = handle_snapshot_catalog(
                SnapshotCatalogMut {
                    snapshots: &mut snaps,
                    selected: &mut selected,
                    mode: &mut mode,
                    picker_target: None,
                    status_msg: &mut None,
                },
                ctx_no_repos(),
                &alt(KeyCode::Char('z')),
            );
            assert_cancel(&outcome);
        }
    }

    #[test]
    fn catalog_detail_esc_returns_to_browse() {
        let mut snaps = vec![fake_snapshot("alpha")];
        let mut selected = 0usize;
        let mut mode = CatalogMode::Detail {
            head: Vec::new(),
            tail: Vec::new(),
        };
        handle_snapshot_catalog(
            SnapshotCatalogMut {
                snapshots: &mut snaps,
                selected: &mut selected,
                mode: &mut mode,
                picker_target: None,
                status_msg: &mut None,
            },
            ctx_no_repos(),
            &key(KeyCode::Esc),
        );
        assert!(matches!(mode, CatalogMode::Browse));
    }

    #[test]
    fn catalog_rename_typing_and_backspace() {
        let mut snaps = vec![fake_snapshot("alpha")];
        let mut selected = 0usize;
        let mut mode = CatalogMode::Rename {
            text: "alp".into(),
            error: Some("prior".into()),
        };
        handle_snapshot_catalog(
            SnapshotCatalogMut {
                snapshots: &mut snaps,
                selected: &mut selected,
                mode: &mut mode,
                picker_target: None,
                status_msg: &mut None,
            },
            ctx_no_repos(),
            &key(KeyCode::Char('x')),
        );
        match &mode {
            CatalogMode::Rename { text, error } => {
                assert_eq!(text, "alpx");
                assert!(error.is_none(), "typing should clear prior error");
            }
            other => panic!("expected Rename, got {other:?}"),
        }

        handle_snapshot_catalog(
            SnapshotCatalogMut {
                snapshots: &mut snaps,
                selected: &mut selected,
                mode: &mut mode,
                picker_target: None,
                status_msg: &mut None,
            },
            ctx_no_repos(),
            &key(KeyCode::Backspace),
        );
        match &mode {
            CatalogMode::Rename { text, .. } => assert_eq!(text, "alp"),
            other => panic!("expected Rename, got {other:?}"),
        }
    }

    #[test]
    fn catalog_rename_esc_returns_to_browse() {
        let mut snaps = vec![fake_snapshot("alpha")];
        let mut selected = 0usize;
        let mut mode = CatalogMode::Rename {
            text: "alpha-v2".into(),
            error: None,
        };
        handle_snapshot_catalog(
            SnapshotCatalogMut {
                snapshots: &mut snaps,
                selected: &mut selected,
                mode: &mut mode,
                picker_target: None,
                status_msg: &mut None,
            },
            ctx_no_repos(),
            &key(KeyCode::Esc),
        );
        assert!(matches!(mode, CatalogMode::Browse));
    }

    #[test]
    fn catalog_confirm_delete_n_returns_to_browse() {
        let mut snaps = vec![fake_snapshot("alpha")];
        let mut selected = 0usize;
        let mut mode = CatalogMode::ConfirmDelete;
        handle_snapshot_catalog(
            SnapshotCatalogMut {
                snapshots: &mut snaps,
                selected: &mut selected,
                mode: &mut mode,
                picker_target: None,
                status_msg: &mut None,
            },
            ctx_no_repos(),
            &key(KeyCode::Char('n')),
        );
        assert!(matches!(mode, CatalogMode::Browse));
        // List untouched on cancel.
        assert_eq!(snaps.len(), 1);
    }

    #[test]
    fn format_relative_time_buckets() {
        // Pick `now` well above one year so the 1y+ case can subtract
        // cleanly without overflow in the literal computation.
        let one_year_secs: u64 = 60 * 60 * 24 * 365;
        let now: u64 = one_year_secs + 10;
        assert_eq!(super::format_relative_time(now, now), "just now");
        assert_eq!(super::format_relative_time(now - 59, now), "just now");
        assert_eq!(super::format_relative_time(now - 60, now), "1m ago");
        assert_eq!(super::format_relative_time(now - 60 * 59, now), "59m ago");
        assert_eq!(super::format_relative_time(now - 60 * 60, now), "1h ago");
        assert_eq!(super::format_relative_time(now - 60 * 60 * 24, now), "1d ago");
        assert_eq!(
            super::format_relative_time(now - one_year_secs, now),
            "1y+ ago"
        );
        // Saturating subtraction so future timestamps don't panic.
        assert_eq!(super::format_relative_time(now + 5, now), "just now");
    }

    #[test]
    fn truncate_keeps_short_strings_intact() {
        assert_eq!(super::truncate("hello", 10), "hello");
        assert_eq!(super::truncate("hello", 5), "hello");
    }

    #[test]
    fn truncate_inserts_ellipsis_when_too_long() {
        let out = super::truncate("hello world", 6);
        assert_eq!(out.chars().count(), 6);
        assert!(out.ends_with('\u{2026}'));
    }

    #[test]
    fn sanitize_for_display_strips_ansi_and_control_bytes() {
        // ESC + CSI red, then "red", then ESC + CSI reset. All ESCs and
        // control bytes must be stripped — otherwise opening the catalog
        // replays the sequence into the user's terminal.
        let out = super::sanitize_for_display("\x1b[31mred\x1b[0m");
        assert!(!out.contains('\x1b'), "ESC must be stripped: {out:?}");
        assert_eq!(out, "[31mred[0m");

        // C0 controls (other than tab) and DEL are stripped.
        let out = super::sanitize_for_display(
            "a\x00b\x07c\x08d\x0ae\x0df\x7fg",
        );
        assert_eq!(out, "abcdefg");

        // Tab is preserved (it's the one C0 control we let through).
        let out = super::sanitize_for_display("a\tb");
        assert_eq!(out, "a\tb");

        // OSC (ESC ] ... BEL) — ESC and BEL both go, content between
        // remains as inert text. Acceptable: the dangerous part is the
        // ESC that begins the sequence; without it the terminal sees
        // only literal characters.
        let out = super::sanitize_for_display("\x1b]0;title\x07rest");
        assert!(!out.contains('\x1b'));
        assert!(!out.contains('\x07'));
        assert_eq!(out, "]0;titlerest");
    }

    #[test]
    fn read_transcript_head_tail_streams_large_file() {
        // 1000-line transcript. Head should be the first N, tail should
        // be the last N. Function must work without slurping the whole
        // file (covered by code review; the assertion here verifies
        // correctness of the windowing).
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().to_path_buf();
        let path = snap_dir.join("transcript.jsonl");
        let mut buf = String::new();
        for i in 0..1000 {
            buf.push_str(&format!("line {i}\n"));
        }
        std::fs::write(&path, buf).unwrap();

        let (head, tail) = super::read_transcript_head_tail(&snap_dir, 5);
        assert_eq!(head.len(), 5);
        assert_eq!(head[0], "line 0");
        assert_eq!(head[4], "line 4");
        assert_eq!(tail.len(), 5);
        assert_eq!(tail[0], "line 995");
        assert_eq!(tail[4], "line 999");
    }

    #[test]
    fn read_transcript_head_tail_no_overlap_for_small_files() {
        // 8 lines, n=5 → head = 0..4, tail = 5..7. (Tail must not
        // include lines already in head.)
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().to_path_buf();
        let path = snap_dir.join("transcript.jsonl");
        let buf: String = (0..8).map(|i| format!("L{i}\n")).collect();
        std::fs::write(&path, buf).unwrap();

        let (head, tail) = super::read_transcript_head_tail(&snap_dir, 5);
        assert_eq!(head, vec!["L0", "L1", "L2", "L3", "L4"]);
        assert_eq!(tail, vec!["L5", "L6", "L7"]);
    }

    #[test]
    fn read_transcript_head_tail_empty_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let (head, tail) =
            super::read_transcript_head_tail(dir.path(), 5);
        assert!(head.is_empty());
        assert!(tail.is_empty());
    }

    #[test]
    fn snapshot_rename_error_is_sanitized_on_render() {
        // Render the rename overlay with an ANSI-laced validation error
        // into a TestBackend and inspect the buffer. No ESC byte should
        // appear anywhere on screen — otherwise a paste-injected name
        // that fails `validate_name` would echo its bytes back through
        // the terminal when the error renders.
        //
        // Render methods on `App` for this overlay don't dereference
        // `self`, so we can avoid building a full App by using ratatui's
        // pure widget API directly to construct the same lines the
        // method does and asserting on that. (Tested via the line-build
        // helper below — extracted from the render method so the test
        // can drive it without a Terminal.)
        let lines = rename_overlay_lines("newname", Some("invalid character: '\x1b[31m'"));
        // Flatten all spans into a single text dump.
        let dump: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(
            !dump.contains('\x1b'),
            "ESC byte present in rendered text: {dump:?}"
        );
        // Sanitized text is still visible.
        assert!(
            dump.contains("invalid character"),
            "expected the (sanitized) error text on screen, got: {dump:?}"
        );
    }

    #[test]
    fn visible_range_centers_selection_when_possible() {
        // selected=40, total=50, visible=10 → window around 40 with 5
        // items above the selection (half = 10/2 = 5) and 4 after.
        let (start, end) = super::visible_range(40, 50, 10);
        assert_eq!(end - start, 10, "window must be exactly visible-sized");
        assert!(
            start <= 40 && 40 < end,
            "selected 40 must be within window [{start}, {end})"
        );
        assert_eq!(start, 35);
        assert_eq!(end, 45);
    }

    #[test]
    fn visible_range_clamps_at_top() {
        let (start, end) = super::visible_range(0, 50, 10);
        assert_eq!((start, end), (0, 10));
    }

    #[test]
    fn visible_range_clamps_at_bottom() {
        let (start, end) = super::visible_range(49, 50, 10);
        assert_eq!((start, end), (40, 50));
    }

    #[test]
    fn visible_range_fits_whole_list_when_room_allows() {
        let (start, end) = super::visible_range(2, 5, 10);
        assert_eq!((start, end), (0, 5));
    }

    #[test]
    fn visible_range_handles_empty_or_zero_height() {
        assert_eq!(super::visible_range(0, 0, 10), (0, 0));
        assert_eq!(super::visible_range(3, 10, 0), (0, 0));
    }

    #[test]
    fn catalog_delete_failure_preserves_list_and_surfaces_error() {
        // Construct an in-memory snapshot list whose entries don't exist
        // on disk under HOME. `agent_memory::delete` returns NotFound;
        // the handler must keep the list intact and set status_msg
        // instead of silently blanking the list via list().unwrap_or_default().
        use crate::test_support::home_lock;
        let dir = tempfile::tempdir().unwrap();
        let fake_home = dir.path();

        let mut snaps = vec![fake_snapshot("ghost"), fake_snapshot("other")];
        let mut selected = 0usize;
        let mut mode = CatalogMode::ConfirmDelete;
        let mut status: Option<String> = None;

        let _guard = home_lock();
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", fake_home) };
        let outcome = handle_snapshot_catalog(
            SnapshotCatalogMut {
                snapshots: &mut snaps,
                selected: &mut selected,
                mode: &mut mode,
                picker_target: None,
                status_msg: &mut status,
            },
            ctx_no_repos(),
            &key(KeyCode::Char('y')),
        );
        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        assert_consumed(&outcome);
        assert_eq!(snaps.len(), 2, "list must be preserved on delete failure");
        assert_eq!(snaps[0].name, "ghost");
        assert!(matches!(mode, CatalogMode::Browse));
        let msg = status.expect("status_msg should be set on delete failure");
        assert!(
            msg.starts_with("Delete failed: "),
            "expected delete-failure status, got: {msg:?}"
        );
    }

    // ── TaskSettings ──────────────────────────────────────────────

    #[test]
    fn task_settings_backspace_pops_name() {
        let task_id = "task-id-1".to_string();
        let mut name = "abc".to_string();
        let (mut color, mut active) = (None::<String>, 0u8);
        let outcome = handle_task_settings(
            TaskSettingsMut {
                task_id: task_id.as_str(),
                name: &mut name,
                color: &mut color,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Backspace),
        );
        assert_consumed(&outcome);
        assert_eq!(name, "ab");
    }

    #[test]
    fn task_settings_enter_submits_save_task_name() {
        let task_id = "task-id-1".to_string();
        let mut name = " new name ".to_string();
        let (mut color, mut active) = (Some("cyan".to_string()), 0u8);
        let outcome = handle_task_settings(
            TaskSettingsMut {
                task_id: task_id.as_str(),
                name: &mut name,
                color: &mut color,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        match outcome {
            InputOutcome::Submit(SubmitAction::SaveTaskName { task_id, name, color }) => {
                assert_eq!(task_id, "task-id-1");
                assert_eq!(name, "new name");
                assert_eq!(color.as_deref(), Some("cyan"));
            }
            other => panic!("expected SaveTaskName, got {:?}", other),
        }
    }

    #[test]
    fn task_settings_space_cycles_color_on_color_field() {
        let task_id = "task-id-1".to_string();
        let mut name = "abc".to_string();
        let (mut color, mut active) = (None::<String>, 1u8);
        let outcome = handle_task_settings(
            TaskSettingsMut {
                task_id: task_id.as_str(),
                name: &mut name,
                color: &mut color,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Char(' ')),
        );
        assert_consumed(&outcome);
        // First cycle step lands on the first palette entry; name untouched
        // (the space must NOT be typed into the name field).
        assert_eq!(color.as_deref(), Some(theme::USER_COLORS[0].0));
        assert_eq!(name, "abc");
    }

    #[test]
    fn user_color_cycle_roundtrips_through_none() {
        // Forward through every palette slot and back to None.
        let mut cur: Option<String> = None;
        for (name, _) in theme::USER_COLORS {
            cur = theme::cycle_user_color(cur.as_deref(), true);
            assert_eq!(cur.as_deref(), Some(*name));
        }
        cur = theme::cycle_user_color(cur.as_deref(), true);
        assert_eq!(cur, None);
        // One step back from None lands on the last palette entry.
        cur = theme::cycle_user_color(cur.as_deref(), false);
        assert_eq!(cur.as_deref(), Some(theme::USER_COLORS[theme::USER_COLORS.len() - 1].0));
        // Unknown stored names behave like None (palette drift recovery).
        assert_eq!(
            theme::cycle_user_color(Some("mauve-nonexistent"), true).as_deref(),
            Some(theme::USER_COLORS[0].0)
        );
        assert_eq!(theme::user_color("mauve-nonexistent"), None);
    }

    // ── WorkflowLaunchConfirm ─────────────────────────────────────

    fn make_slot() -> WorkflowSlotChoice {
        WorkflowSlotChoice {
            role: "worker".to_string(),
            options: vec![
                WorkflowSlotSource::New(Engine::ClaudeCode),
                WorkflowSlotSource::New(Engine::Codex),
            ],
            option_index: 0,
        }
    }

    #[test]
    fn workflow_launch_down_advances_active_slot() {
        let mut slots = vec![make_slot()];
        let mut active = 0usize;
        let mut goal = String::new();
        let outcome = handle_workflow_launch_confirm(
            WorkflowLaunchConfirmMut {
                ws_id: "ws-1",
                workflow_name: "feedback",
                slots: &mut slots,
                active_slot: &mut active,
                goal: &mut goal,
                cursor_task_id: None,
            },
            ctx_no_repos(),
            &key(KeyCode::Down),
        );
        assert_consumed(&outcome);
        // positions = slots.len() + 1 = 2; from 0 → 1 (the goal field).
        assert_eq!(active, 1);
    }

    #[test]
    fn workflow_launch_backspace_pops_goal_when_focused() {
        let mut slots = vec![make_slot()];
        let mut active = slots.len(); // goal-focused
        let mut goal = "ab".to_string();
        let outcome = handle_workflow_launch_confirm(
            WorkflowLaunchConfirmMut {
                ws_id: "ws-0",
                workflow_name: "feedback",
                slots: &mut slots,
                active_slot: &mut active,
                goal: &mut goal,
                cursor_task_id: None,
            },
            ctx_no_repos(),
            &key(KeyCode::Backspace),
        );
        assert_consumed(&outcome);
        assert_eq!(goal, "a");
    }

    #[test]
    fn workflow_launch_enter_submits_with_optional_goal() {
        let mut slots = vec![make_slot()];
        let mut active = slots.len();
        let mut goal = "  refactor the parser  ".to_string();
        let outcome = handle_workflow_launch_confirm(
            WorkflowLaunchConfirmMut {
                ws_id: "ws-5",
                workflow_name: "feedback",
                slots: &mut slots,
                active_slot: &mut active,
                goal: &mut goal,
                cursor_task_id: None,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        match outcome {
            InputOutcome::Submit(SubmitAction::LaunchWorkflow {
                ws_id,
                workflow_name,
                slots: launched_slots,
                goal,
                cursor_task_id: _,
            }) => {
                assert_eq!(ws_id, "ws-5");
                assert_eq!(workflow_name, "feedback");
                assert_eq!(launched_slots.len(), 1);
                assert_eq!(goal.as_deref(), Some("refactor the parser"));
            }
            other => panic!("expected LaunchWorkflow, got {:?}", other),
        }
    }

    #[test]
    fn workflow_launch_esc_cancels() {
        let mut slots = vec![make_slot()];
        let mut active = 0usize;
        let mut goal = String::new();
        let outcome = handle_workflow_launch_confirm(
            WorkflowLaunchConfirmMut {
                ws_id: "ws-0",
                workflow_name: "feedback",
                slots: &mut slots,
                active_slot: &mut active,
                goal: &mut goal,
                cursor_task_id: None,
            },
            ctx_no_repos(),
            &key(KeyCode::Esc),
        );
        assert_cancel(&outcome);
    }

    // Phase 3 (doc/existing-session-binding.md): an `Existing` selection in the
    // launch modal threads `role -> daemon_session_uid` into the launch; a
    // `New` selection sends no entry for that role. The handler emits the slots
    // on submit; `slots_to_role_sessions` (called inside
    // `launch_workflow_via_daemon`) maps them against the workspace's per-index
    // daemon uids. Asserted on the daemon uid, NOT the local UI handle.
    #[test]
    fn workflow_launch_existing_slot_maps_to_role_sessions() {
        // worker: eligible role offering [New(claude), Existing(0)] with the
        // existing session selected. reviewer: a New-only slot (e.g. an
        // ineligible role) — must contribute no binding.
        let worker = WorkflowSlotChoice {
            role: "worker".to_string(),
            options: vec![
                WorkflowSlotSource::New(Engine::ClaudeCode),
                WorkflowSlotSource::Existing(0),
            ],
            option_index: 1, // Existing(0) selected
        };
        let reviewer = WorkflowSlotChoice {
            role: "reviewer".to_string(),
            options: vec![WorkflowSlotSource::New(Engine::ClaudeCode)],
            option_index: 0, // New selected
        };
        let mut slots = vec![worker, reviewer];
        let mut active = slots.len(); // goal-focused, so Enter submits
        let mut goal = String::new();
        let outcome = handle_workflow_launch_confirm(
            WorkflowLaunchConfirmMut {
                ws_id: "ws-7",
                workflow_name: "feedback",
                slots: &mut slots,
                active_slot: &mut active,
                goal: &mut goal,
                cursor_task_id: None,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        let launched = match outcome {
            InputOutcome::Submit(SubmitAction::LaunchWorkflow { slots, .. }) => slots,
            other => panic!("expected LaunchWorkflow, got {:?}", other),
        };
        // Two daemon-owned sessions in the workspace; index 0 (selected by the
        // worker slot) carries this uid. The map is keyed on the DAEMON uid.
        let session_uids = vec![
            Some("daemon-uid-worker".to_string()),
            Some("daemon-uid-other".to_string()),
        ];
        let map = slots_to_role_sessions(&launched, &session_uids);
        // Exactly the worker is bound, to the daemon uid at the selected index;
        // the New reviewer slot produces no entry.
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("worker").map(String::as_str), Some("daemon-uid-worker"));
        assert!(!map.contains_key("reviewer"));

        // And with NO Existing slots selected (all New), the map is empty —
        // byte-identical to the pre-Phase-3 fresh-spawn launch.
        let all_new = vec![
            WorkflowSlotChoice {
                role: "worker".to_string(),
                options: vec![
                    WorkflowSlotSource::New(Engine::ClaudeCode),
                    WorkflowSlotSource::Existing(0),
                ],
                option_index: 0, // New selected
            },
            WorkflowSlotChoice {
                role: "reviewer".to_string(),
                options: vec![WorkflowSlotSource::New(Engine::ClaudeCode)],
                option_index: 0,
            },
        ];
        assert!(slots_to_role_sessions(&all_new, &session_uids).is_empty());
    }

    // Phase 3: an `Existing(si)` selection pointing at a session WITHOUT a
    // daemon uid (a purely TUI-local session, or an index that no longer
    // resolves) contributes no binding — the daemon then fresh-spawns the role
    // rather than receiving an unbindable uid.
    #[test]
    fn workflow_launch_existing_slot_without_daemon_uid_is_dropped() {
        let slots = vec![WorkflowSlotChoice {
            role: "worker".to_string(),
            options: vec![
                WorkflowSlotSource::New(Engine::ClaudeCode),
                WorkflowSlotSource::Existing(0),
            ],
            option_index: 1, // Existing(0) selected
        }];
        // Session 0 has no daemon uid (local-only); index 1 doesn't exist.
        let session_uids = vec![None];
        assert!(slots_to_role_sessions(&slots, &session_uids).is_empty());

        let slots_oob = vec![WorkflowSlotChoice {
            role: "worker".to_string(),
            options: vec![
                WorkflowSlotSource::New(Engine::ClaudeCode),
                WorkflowSlotSource::Existing(5),
            ],
            option_index: 1,
        }];
        assert!(slots_to_role_sessions(&slots_oob, &session_uids).is_empty());
    }

    // Engine choice ("new claude" vs "new codex"): a `New(engine)` selection that
    // DIFFERS from the role's TOML default (always options[0]) threads a
    // `role -> engine` override; an unchanged default — or an `Existing` binding,
    // which keeps the bound session's own engine — contributes nothing, so a
    // default launch stays byte-identical on the wire.
    #[test]
    fn workflow_launch_engine_override_maps_only_non_default_new_slots() {
        let slots = vec![
            // claude-default role cycled to the codex alternate → override.
            WorkflowSlotChoice {
                role: "worker".to_string(),
                options: vec![
                    WorkflowSlotSource::New(Engine::ClaudeCode),
                    WorkflowSlotSource::New(Engine::Codex),
                ],
                option_index: 1,
            },
            // codex-default role cycled to the claude alternate → override.
            WorkflowSlotChoice {
                role: "reviewer".to_string(),
                options: vec![
                    WorkflowSlotSource::New(Engine::Codex),
                    WorkflowSlotSource::New(Engine::ClaudeCode),
                ],
                option_index: 1,
            },
            // claude-default role left at its default → NO override.
            WorkflowSlotChoice {
                role: "manager".to_string(),
                options: vec![
                    WorkflowSlotSource::New(Engine::ClaudeCode),
                    WorkflowSlotSource::New(Engine::Codex),
                ],
                option_index: 0,
            },
            // Existing binding selected → engine comes from the bound session,
            // never overridden here.
            WorkflowSlotChoice {
                role: "auditor".to_string(),
                options: vec![
                    WorkflowSlotSource::New(Engine::ClaudeCode),
                    WorkflowSlotSource::Existing(0),
                ],
                option_index: 1,
            },
        ];
        let map = slots_to_role_engines(&slots);
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("worker").map(String::as_str), Some("codex"));
        assert_eq!(map.get("reviewer").map(String::as_str), Some("claude-code"));
        assert!(!map.contains_key("manager"));
        assert!(!map.contains_key("auditor"));
    }

    // ── WorkflowPicker ────────────────────────────────────────────

    #[test]
    fn workflow_picker_j_advances_selection_with_wraparound() {
        let names = vec!["a".to_string(), "b".to_string()];
        let mut selected = 1usize;
        let outcome = handle_workflow_picker(
            WorkflowPickerMut {
                ws_id: "ws-0",
                focused_si: None,
                names: &names,
                selected: &mut selected,
                cursor_task_id: None,
            },
            ctx_no_repos(),
            &key(KeyCode::Char('j')),
        );
        assert_consumed(&outcome);
        assert_eq!(selected, 0);
    }

    #[test]
    fn workflow_picker_enter_submits_selected_name() {
        let names = vec!["alpha".to_string(), "beta".to_string()];
        let mut selected = 1usize;
        let outcome = handle_workflow_picker(
            WorkflowPickerMut {
                ws_id: "ws-7",
                focused_si: Some(2),
                names: &names,
                selected: &mut selected,
                cursor_task_id: None,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        match outcome {
            InputOutcome::Submit(SubmitAction::EnterWorkflowLaunchConfirm {
                ws_id,
                focused_si,
                workflow_name,
                cursor_task_id: _,
            }) => {
                assert_eq!(ws_id, "ws-7");
                assert_eq!(focused_si, Some(2));
                assert_eq!(workflow_name, "beta");
            }
            other => panic!("expected EnterWorkflowLaunchConfirm, got {:?}", other),
        }
    }

    #[test]
    fn workflow_picker_enter_with_empty_names_submits_none() {
        let names: Vec<String> = vec![];
        let mut selected = 0usize;
        let outcome = handle_workflow_picker(
            WorkflowPickerMut {
                ws_id: "ws-0",
                focused_si: None,
                names: &names,
                selected: &mut selected,
                cursor_task_id: None,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        // Pre-extraction behavior: out-of-bounds selection still closes
        // the modal, just without firing the side effect.
        assert!(matches!(
            outcome,
            InputOutcome::Submit(SubmitAction::None)
        ));
    }

    // ── WorkflowHistory ───────────────────────────────────────────

    #[test]
    fn workflow_history_q_cancels() {
        let outcome = handle_workflow_history(ctx_no_repos(), &key(KeyCode::Char('q')));
        assert_cancel(&outcome);
    }

    #[test]
    fn workflow_history_other_key_consumes() {
        let outcome = handle_workflow_history(ctx_no_repos(), &key(KeyCode::Char('x')));
        assert_consumed(&outcome);
    }

    // ── Confirm ───────────────────────────────────────────────────

    #[test]
    fn confirm_y_submits_mark_done() {
        let outcome = handle_confirm(
            &ConfirmAction::MarkDone,
            ctx_no_repos(),
            &key(KeyCode::Char('y')),
        );
        assert!(matches!(
            outcome,
            InputOutcome::Submit(SubmitAction::MarkActiveDone)
        ));
    }

    #[test]
    fn confirm_capital_y_submits_delete() {
        let outcome = handle_confirm(
            &ConfirmAction::Delete,
            ctx_no_repos(),
            &key(KeyCode::Char('Y')),
        );
        assert!(matches!(
            outcome,
            InputOutcome::Submit(SubmitAction::DeleteActive)
        ));
    }

    #[test]
    fn confirm_n_cancels() {
        let outcome = handle_confirm(
            &ConfirmAction::MarkDone,
            ctx_no_repos(),
            &key(KeyCode::Char('n')),
        );
        assert_cancel(&outcome);
    }

    #[test]
    fn confirm_esc_cancels() {
        let outcome = handle_confirm(
            &ConfirmAction::MarkDone,
            ctx_no_repos(),
            &key(KeyCode::Esc),
        );
        assert_cancel(&outcome);
    }

    #[test]
    fn confirm_enter_routes_stop_workflow_run_id() {
        let outcome = handle_confirm(
            &ConfirmAction::StopWorkflow {
                run_id: "run-abc".to_string(),
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        match outcome {
            InputOutcome::Submit(SubmitAction::StopWorkflow { run_id }) => {
                assert_eq!(run_id, "run-abc");
            }
            other => panic!("expected StopWorkflow, got {:?}", other),
        }
    }
}
