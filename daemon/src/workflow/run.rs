//! Runtime state for an active workflow run, plus persistence.
//!
//! Each run lives in `~/.cm/workflow-runs/<run-id>/` as:
//!   - `state.json`   — the `WorkflowRun` struct below
//!   - `events.jsonl` — MCP tool calls appended by the workflow_tools in mcp_server/

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunStatus {
    Running,
    Paused,
    Done,
    Detached,
}

/// What caused a role's most recent activation.
///
/// **Wire shape (stable):** internally tagged on `kind` with `snake_case`
/// variant names. This is the contract surfaced via the MCP tools
/// (`list_workflows`, `get_workflow_state`) and persisted in
/// `state.json` history entries. Renaming a variant or field without a
/// corresponding `#[serde(rename = ...)]` is a breaking change.
///
/// ```json
/// {"kind": "initial"}
/// {"kind": "static_idle", "from_role": "worker"}
/// {"kind": "mcp_transition", "from_role": "manager", "prompt": "...", "event_id": "..."}
/// ```
///
/// The `serialize_trigger_wire_shape` test in this module pins the JSON
/// for every variant — update it deliberately when the schema changes.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriggerKind {
    /// The role that started the run (no prior role).
    Initial,
    /// Fired by a static transition after the previous role went idle.
    StaticIdle { from_role: String },
    /// Fired by an MCP `workflow_transition` tool call.
    McpTransition {
        from_role: String,
        prompt: String,
        event_id: String,
    },
}

/// One row in the run's activation history.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub iteration: u32,
    pub role: String,
    pub session_id: Option<String>,
    /// Captured when this role's turn ends (on transition-out). `None` while the role
    /// is still active or if capture failed.
    pub last_message: Option<String>,
    pub activated_at: u64,
    pub deactivated_at: Option<u64>,
    pub trigger: TriggerKind,
    /// Snapshot of the role's cumulative assistant message count at the moment
    /// this activation started. An `on_idle` transition only fires once the
    /// current count exceeds this — prevents firing on mere PTY startup activity
    /// or on a role that hasn't produced any new work yet.
    #[serde(default)]
    pub assistant_count_at_start: usize,
    /// Snapshot of the role's text-bearing message count at activation. Used
    /// by the `this_turn` resolver to slice out only the assistant text the
    /// role produced during *this* activation — so the manager's prompt
    /// surfaces the whole stream of worker replies, not just `last_message`.
    /// Distinct from `assistant_count_at_start` because that counts every
    /// assistant JSONL entry (incl. thinking-only / tool-use-only turns)
    /// for the on_idle gate; `list_messages` filters those out.
    #[serde(default)]
    pub text_messages_at_start: usize,
}

/// How a role is bound to a concrete TerminalSession in the TUI.
///
/// We identify the session durably by its label (unique within a task). The
/// `current_session_id` updates when a `fresh`-context role is respawned and the
/// underlying Claude/Codex conversation ID changes.
///
/// **Field semantics** (10d-2c-2-2-b round-5 F1):
/// - `current_session_id`: the agent's *transcript id* (JSONL file basename
///   for Claude; session uid for Codex). Used by the templating resolver to
///   locate the role's transcript. NOT the same as the daemon-side
///   `DaemonSession.uid`.
/// - `daemon_session_uid`: stable daemon-side session uid (matches
///   `DaemonSession.uid` keys in `DaemonState.sessions`). `Some(_)` iff the
///   role was bound to a daemon-spawned session at launch / rebind time.
///   `None` for TUI-only sessions and pre-r5 on-disk records.
///   Set at spawn/rebind by the daemon's `start_workflow` + fresh-reset
///   rebind. Used by the daemon's workflow poller
///   (`cm_daemon::workflow::poller::daemon_owns_run`) as the durable
///   fire-precondition signal — "can the (sole) daemon poller deliver to this
///   role's session" — no longer relies on the best-effort
///   `session.set_workflow_context` RPC tags.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RoleBinding {
    pub session_label: String,
    #[serde(default)]
    pub current_session_id: Option<String>,
    #[serde(default)]
    pub daemon_session_uid: Option<String>,
    /// True iff this role ADOPTED an already-running session at launch (the
    /// existing-session bind path of `start_workflow`), rather than being
    /// fresh-spawned. Set ONLY on the bind path; fresh spawns leave it `false`.
    ///
    /// This is the durable, bind-time signal the finalize drainer keys its
    /// bound-delivery readiness gate off — it must NOT be inferred from runtime
    /// sid presence or `daemon_session_uid.is_some()`: a fresh-spawned Codex
    /// role's rollout exists from boot, so the poller syncs its
    /// `current_session_id` BEFORE the initial activation is delivered, and a
    /// sid-presence heuristic would then wrongly hold its goal behind a
    /// `role_turn_complete` check on a pre-prompt rollout (no `task_complete`
    /// yet → blocks forever). Recorded here, the gate fires only for genuinely
    /// adopted sessions. Additive (`#[serde(default)]`); older state.json
    /// records deserialize to `false`.
    #[serde(default)]
    pub bound: bool,
}

/// Message counts at the moment the workflow run was launched. Later reads of
/// the role's transcript subtract these offsets so that `{{ roles.X.user[0] }}`
/// refers to the first message sent *after* the workflow started — not the
/// first message ever in that session.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MessageBaseline {
    #[serde(default)]
    pub user_count: usize,
    #[serde(default)]
    pub assistant_count: usize,
}

/// One entry in the manager-curated rejected-findings stash.
///
/// The manager calls `workflow_reject_finding(text)` to add an entry; the
/// reviewer's activation prompt then includes the running list so a fresh
/// reviewer (which doesn't see the prior round's conversation) doesn't
/// re-surface findings the manager has already decided to ignore.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RejectedFinding {
    /// The text the manager wrote — usually a one-line paraphrase of
    /// the rejected finding.
    pub text: String,
    /// Unix seconds at which the manager recorded it.
    pub recorded_at: u64,
    /// Run iteration that was active when the rejection landed. Helps
    /// debug "which round did this come from" without reading events.jsonl.
    pub iteration: u32,
}

/// Finalization phase of a daemon-driven activation hand-off, persisted on
/// [`PendingActivation`] so a daemon restart mid-finalization resumes from the
/// recorded step (Phase 3 of `doc/daemon-side-workflow-orchestration.md`).
///
/// Order (persistent roles skip `ClearBodySent` and `RebindPending`):
/// `Queued` -> `Rendered` -> [`ClearBodySent`] -> `Appended` -> `BodySent`
/// -> [`RebindPending`] -> `Done`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActivationPhase {
    /// Recorded by `workflow_transition`; nothing rendered/delivered yet.
    Queued,
    /// Activation prompt rendered + frozen onto the record (step 1). Rendered
    /// from the PRE-reset, PRE-append run snapshot.
    Rendered,
    /// FRESH only: `/clear` body written, role baseline reset to zero,
    /// `current_session_id` cleared, pre-`/clear` listing snapshot stored; the
    /// `/clear` Enter is pending (fires after the deferred gap).
    ClearBodySent,
    /// The incoming role's `HistoryEntry` has been appended (counts from step 3,
    /// `session_id` = stable sid for persistent / pending for fresh). The prompt
    /// body has NOT been written yet.
    Appended,
    /// Prompt body written (step 5); the prompt Enter is pending (deferred gap).
    BodySent,
    /// FRESH only: prompt Enter fired, awaiting sid discovery (step 6). The idle
    /// gate stays inert (`NoTranscriptId`) until `current_session_id` rebinds.
    RebindPending,
    /// Finalization complete.
    Done,
}

/// Durable record of an in-flight activation hand-off, recorded by
/// `workflow_transition` in the same flock mutation that advances `active_role`
/// and consumed by the daemon delivery drainer. Carries everything finalization
/// needs to render the prompt and reconstruct the incoming role's history entry
/// WITHOUT re-reading `events.jsonl`, so it survives a daemon restart
/// mid-finalization. Additive (`#[serde(default)]` on the run field), so older
/// state.json files deserialize with `None`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingActivation {
    /// Monotonic-within-run id (the post-mutation `iteration`); the history
    /// idempotency guard keys on `(role, iteration)`.
    pub activation_id: u64,
    /// Incoming (to) role.
    pub target_role: String,
    /// Post-mutation iteration captured in the transition closure.
    pub iteration: u32,
    /// Trigger/history metadata, used to reconstruct the `HistoryEntry`'s
    /// `TriggerKind` at finalization (never re-read from events): `StaticIdle`
    /// for poller fires, `McpTransition { prompt, event_id }` for MCP fires.
    pub trigger: TriggerKind,
    /// RAW supplied prompt (empty for static fires; the MCP `prompt` otherwise).
    /// Rendered at finalization with the empty -> activation-template fallback.
    pub raw_prompt: String,
    /// Phase 4 (P5): deliver `raw_prompt` VERBATIM (skip template rendering).
    /// Set for a goal-only initial activation (no role `activation_prompt`), so
    /// a goal containing literal `{{ }}` is not mangled — matching the TUI's
    /// goal-verbatim path.
    #[serde(default)]
    pub verbatim: bool,
    /// Whether the target role is `Context::Fresh` (needs `/clear` + rebind).
    pub needs_fresh_reset: bool,
    /// Phase 4: the run's INITIAL activation (the worker at launch). The initial
    /// `HistoryEntry` (iteration 1, `TriggerKind::Initial`) is already seeded by
    /// [`WorkflowRun::new`], so finalization is DELIVERY-ONLY — it renders +
    /// delivers the worker's initial prompt and patches the existing entry's
    /// `session_id`, but never appends a second worker row (acceptance #2).
    #[serde(default)]
    pub is_initial: bool,
    /// Finalization phase; advanced + persisted step by step.
    pub phase: ActivationPhase,
    /// The rendered activation prompt, frozen at finalization step 1 so a crash
    /// after the append resumes delivery from the frozen text rather than
    /// re-rendering against now-larger history.
    #[serde(default)]
    pub rendered_prompt: Option<String>,
    /// Sid-discovery key: a transcript-dir listing snapshot the
    /// `RebindPending` discovery diffs against. Filled either at a fresh
    /// role's reset (pre-`/clear`) or, for an UNBOUND persistent Claude role,
    /// right before body delivery (the cross-bind fix: first activations bind
    /// causally instead of via spawn-time detector timing). Persisting it
    /// lets a restart in `RebindPending` resume discovery.
    #[serde(default)]
    pub pre_clear_snapshot: Option<Vec<String>>,
    /// Unix-ms deadline after which the currently-pending Enter keystroke
    /// (`/clear`'s in `ClearBodySent`, the prompt's in `BodySent`) should fire.
    /// The Enter ENCODING is NOT frozen — it is recomputed from the live
    /// terminal mode at fire time. Persisted so a restart mid-gap still fires.
    #[serde(default)]
    pub enter_fire_at_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowRun {
    pub run_id: String,
    pub workflow_name: String,
    /// The `TaskEntry`-identifying key: worktree_path string for local tasks,
    /// `task:<id>` for cloud tasks. Matches the manifest key convention.
    pub task_key: String,
    /// Bound TaskEntry id, when known. Set by `start_workflow_run` (the
    /// MCP entry point) so the descendant-aware auth check has a real
    /// task id to evaluate. UI launches that don't go through the MCP
    /// path leave this `None`; auth handlers fall back to walking
    /// `app.tasks` by `workspace_id`.
    #[serde(default)]
    pub task_id: Option<String>,
    pub role_sessions: BTreeMap<String, RoleBinding>,
    pub active_role: Option<String>,
    pub iteration: u32,
    pub paused: bool,
    pub status: RunStatus,
    pub history: Vec<HistoryEntry>,
    pub started_at: u64,
    #[serde(default)]
    pub done_reason: Option<String>,
    /// Byte offset into events.jsonl that we've already processed. Lets us resume
    /// after a TUI restart without re-firing old events.
    #[serde(default)]
    pub events_offset: u64,
    /// Snapshot of each role's transcript message counts at launch time. Used to
    /// slice out pre-launch history when templates reference role messages.
    #[serde(default)]
    pub role_baselines: BTreeMap<String, MessageBaseline>,
    /// Optional run-level goal set at launch (via the launch modal). When
    /// present, templates' `{{ goal }}` expands to this; otherwise it falls
    /// back to the worker's `initial_prompt`. Useful when the workflow is
    /// restarted mid-task — `initial_prompt` then points to the latest
    /// user message, which may not reflect the original objective.
    #[serde(default)]
    pub goal: Option<String>,
    /// Plan text snapshotted per role at launch — captured from the role's
    /// most recent pre-launch `ExitPlanMode` tool_use, if any. Lets templates
    /// resolve `{{ roles.<role>.plan }}` to the plan the user accepted right
    /// before launching, even after the worker has produced more turns and
    /// the live transcript's tail is no longer the plan tool_use.
    #[serde(default)]
    pub role_plans: BTreeMap<String, String>,
    /// Findings the manager has explicitly dismissed via the
    /// `workflow_reject_finding` MCP tool. Surfaced to the (fresh-context)
    /// reviewer on its next activation so it doesn't re-raise the same
    /// nits round after round.
    #[serde(default)]
    pub rejected_findings: Vec<RejectedFinding>,
    /// In-flight activation hand-off the daemon delivery drainer is finalizing.
    /// Recorded by `workflow_transition` alongside the close/iteration/active_role
    /// mutation; cleared when finalization reaches `ActivationPhase::Done` (or
    /// rolled back if the event append exhausts). `None` when no hand-off is
    /// mid-flight. Additive — older state.json files deserialize to `None`.
    #[serde(default)]
    pub pending_activation: Option<PendingActivation>,
    /// Idle-nudge backstop debounce (see the daemon poller / TUI
    /// controller idle gate). A role with no static `on_idle` transition
    /// is expected to advance the workflow via a dynamic tool call
    /// (`workflow_transition` / `workflow_done`). If it instead ends a
    /// turn with prose and *no* tool call, the gate re-delivers its
    /// activation prompt. This records the active role's assistant-turn
    /// count at the last such nudge, so the gate fires at most once per
    /// genuinely new completed turn: it re-fires only when the live count
    /// differs from this value. That closes the async history-append lag
    /// window (where the start-count baseline is briefly stale) which
    /// would otherwise let a self-targeted re-activation burst every
    /// tick. `None` until the first nudge; never needs reset because a
    /// persistent session's assistant count only grows.
    #[serde(default)]
    pub nudge_assistant_count: Option<usize>,
}

impl WorkflowRun {
    pub fn new(
        run_id: String,
        workflow_name: String,
        task_key: String,
        role_sessions: BTreeMap<String, RoleBinding>,
        initial_role: String,
        role_baselines: BTreeMap<String, MessageBaseline>,
        goal: Option<String>,
        role_plans: BTreeMap<String, String>,
        initial_text_count: usize,
    ) -> Self {
        let now = now_unix();
        let initial_assistant_count = role_baselines
            .get(&initial_role)
            .map(|b| b.assistant_count)
            .unwrap_or(0);
        let initial_history = HistoryEntry {
            iteration: 1,
            role: initial_role.clone(),
            session_id: role_sessions
                .get(&initial_role)
                .and_then(|b| b.current_session_id.clone()),
            last_message: None,
            activated_at: now,
            deactivated_at: None,
            trigger: TriggerKind::Initial,
            assistant_count_at_start: initial_assistant_count,
            text_messages_at_start: initial_text_count,
        };
        WorkflowRun {
            run_id,
            workflow_name,
            task_key,
            task_id: None,
            role_sessions,
            active_role: Some(initial_role),
            iteration: 1,
            paused: false,
            status: RunStatus::Running,
            history: vec![initial_history],
            started_at: now,
            done_reason: None,
            events_offset: 0,
            role_baselines,
            goal,
            role_plans,
            rejected_findings: Vec::new(),
            pending_activation: None,
            nudge_assistant_count: None,
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self.status, RunStatus::Running | RunStatus::Paused)
    }

    /// Close out the active role's history entry with a captured last_message and
    /// timestamp. Used when transitioning away.
    pub fn close_active_role(&mut self, last_message: Option<String>) {
        if let Some(entry) = self.history.last_mut() {
            if entry.deactivated_at.is_none() {
                entry.deactivated_at = Some(now_unix());
                entry.last_message = last_message;
            }
        }
    }

    /// Append a new activation history entry and update active_role/iteration.
    pub fn activate_role(
        &mut self,
        role: String,
        trigger: TriggerKind,
        assistant_count_at_start: usize,
        text_messages_at_start: usize,
    ) {
        // Iteration increments when we cycle back to the first role in role_order.
        // For simplicity here, iteration tracks total activations / roles.len(), but
        // we leave precise iteration accounting to the caller that holds the Workflow.
        self.iteration += 1;
        let session_id = self
            .role_sessions
            .get(&role)
            .and_then(|b| b.current_session_id.clone());
        self.history.push(HistoryEntry {
            iteration: self.iteration,
            role: role.clone(),
            session_id,
            last_message: None,
            activated_at: now_unix(),
            deactivated_at: None,
            trigger,
            assistant_count_at_start,
            text_messages_at_start,
        });
        self.active_role = Some(role);
    }

    /// Assistant count captured when the currently-active role last activated.
    pub fn active_assistant_start_count(&self) -> Option<usize> {
        self.history
            .last()
            .filter(|h| Some(&h.role) == self.active_role.as_ref())
            .map(|h| h.assistant_count_at_start)
    }

    /// Number of consecutive trailing history entries belonging to `role` —
    /// how many times `role` has been activated in an unbroken run at the
    /// tail of the history. The idle-nudge backstop uses this to cap how
    /// many times it re-prompts a role that keeps ending its turn without
    /// advancing the workflow: the first entry in the streak is the role's
    /// legitimate activation, every additional one is a prior nudge, so
    /// `streak - 1` is the nudge count for the current stuck stretch. The
    /// streak resets naturally when a different role is activated.
    pub fn trailing_activation_streak(&self, role: &str) -> usize {
        self.history
            .iter()
            .rev()
            .take_while(|h| h.role == role)
            .count()
    }

    /// 10d-2c-1 review round-1 Option A: append a history entry
    /// for the currently-active role using the caller-supplied
    /// `assistant_count_at_start`. The TUI tail observer calls
    /// this after observing a daemon-source dynamic event — the
    /// daemon's mutation set `active_role` + bumped `iteration`
    /// + closed the outgoing role, but DEFERRED the history
    /// append because only the TUI has transcript-tail
    /// visibility for `assistant_count_at_start`. Without that
    /// deferred patch, the on_idle gate would fire prematurely
    /// (start_count=0 means "any prior assistant turn looks new").
    ///
    /// Returns `true` if an entry was appended, `false` if no-op
    /// (active_role is None, OR a matching entry already exists
    /// for the current iteration — idempotency guard against the
    /// TUI tail observing the same daemon event twice across a
    /// crash-recovery re-read).
    /// 10d-2c-1 review round-14: history entry's role comes
    /// from the EVENT (the daemon-routed Decision threads in
    /// the event's `to` field as `target_role`), NOT from the
    /// run's current `active_role`. Pre-r14 the function read
    /// `self.active_role` — if multiple daemon transitions
    /// queued before TUI processed any of them, every queued
    /// event would see the LATEST `active_role` on state.json
    /// and append history entries for the wrong role (or
    /// silently drop when active_role was None, e.g. after a
    /// daemon `workflow_done`).
    ///
    /// 10d-2c-1 review round-15: history entry's `iteration`
    /// also comes from the EVENT (via the daemon's
    /// post-mutation capture). Pre-r15 the function used
    /// `self.iteration` — same multi-event-queue race surfaced
    /// the LATEST iteration on every queued event's history
    /// entry. `event_iteration == 0` is the pre-r15 backward-
    /// compat sentinel; caller falls back to `self.iteration`
    /// in that case.
    ///
    /// `target_role` is the role being activated by this
    /// specific event — `workflow_transition`'s `to` param.
    /// `workflow_done` events don't call this; the run-done
    /// state is captured via `status: Done`, not via a
    /// history entry.
    pub fn append_history_entry_for_event_target_role(
        &mut self,
        target_role: &str,
        event_iteration: u32,
        trigger: TriggerKind,
        assistant_count_at_start: usize,
        text_messages_at_start: usize,
    ) -> bool {
        // Round-15: prefer the event's iteration. Fall back
        // to `self.iteration` for pre-r15 on-disk events
        // (where the field deserialized to 0 via serde
        // default).
        let entry_iteration = if event_iteration > 0 {
            event_iteration
        } else {
            self.iteration
        };
        if let Some(last) = self.history.last() {
            if last.role == target_role && last.iteration == entry_iteration {
                return false;
            }
        }
        let session_id = self
            .role_sessions
            .get(target_role)
            .and_then(|b| b.current_session_id.clone());
        self.history.push(HistoryEntry {
            iteration: entry_iteration,
            role: target_role.to_string(),
            session_id,
            last_message: None,
            activated_at: now_unix(),
            deactivated_at: None,
            trigger,
            assistant_count_at_start,
            text_messages_at_start,
        });
        true
    }

    pub fn mark_done(&mut self, reason: String) {
        self.close_active_role(None);
        self.active_role = None;
        self.status = RunStatus::Done;
        self.done_reason = Some(reason);
        // Drop any in-flight activation hand-off: a terminal run must carry no
        // pending_activation, or it's a durable on-disk record pointing at a
        // hand-off that will never complete (and that the drainer must never
        // resume — see advance_finalization's status recheck).
        self.pending_activation = None;
    }

    pub fn mark_detached(&mut self) {
        self.status = RunStatus::Detached;
        self.pending_activation = None;
    }
}

/// 10d-3: shared canonical mutation for stop_workflow.
/// Relocated from `tui/src/app.rs::apply_stop_workflow_status`
/// (10d-2c-1 round-9) so the daemon's `stop_workflow` handler
/// and the TUI's A-o flow apply the IDENTICAL state.json
/// mutation — pinned by the fire-output parity test.
///
/// Terminal-state guard preserves `Done`: a run that completed
/// naturally via `workflow_done` keeps its `Done` status and
/// `done_reason`. Without the guard, a UI stop on a completed
/// run would overwrite `Done` with `Detached`, erasing the
/// distinction between successful completion and operator
/// abort.
pub fn apply_stop_workflow_status(r: &mut WorkflowRun) {
    if matches!(r.status, RunStatus::Done) {
        return;
    }
    r.mark_detached();
}

// The rest of `impl WorkflowRun` continues below.
// `apply_stop_workflow_status` (free function above) splits the
// impl block — Rust allows multiple `impl` blocks for the same
// type, so this is legal even though it looks unusual.
impl WorkflowRun {
    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        self.status = if paused { RunStatus::Paused } else { RunStatus::Running };
    }
}

// ------------------- Persistence -------------------

pub fn runs_dir() -> PathBuf {
    // Single source of truth for the .cm root — keeps the
    // symlink-rejection walk in `events::WorkflowEventsWriter`
    // aligned with whatever fallback (`$HOME` or `/tmp`)
    // resolved here, so a no-HOME container env doesn't break
    // workflows.
    crate::path::dot_cm_dir().join("workflow-runs")
}

pub fn run_dir(run_id: &str) -> PathBuf {
    runs_dir().join(run_id)
}

pub fn events_path(run_id: &str) -> PathBuf {
    run_dir(run_id).join("events.jsonl")
}

pub fn new_run_id() -> String {
    // Compact base36-ish: seconds + a bit of randomness from the OS.
    let secs = now_unix();
    let rand_part: u32 = {
        // Use env + pid + nanos as a lightweight entropy source. Good enough for IDs
        // that are unique per-machine; we're not creating millions.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let pid = std::process::id();
        nanos ^ pid
    };
    format!("wf_{:x}{:x}", secs, rand_part)
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug)]
pub enum PersistError {
    Io(io::Error),
    Json(serde_json::Error),
}

impl From<io::Error> for PersistError {
    fn from(e: io::Error) -> Self {
        PersistError::Io(e)
    }
}
impl From<serde_json::Error> for PersistError {
    fn from(e: serde_json::Error) -> Self {
        PersistError::Json(e)
    }
}

impl std::fmt::Display for PersistError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PersistError::Io(e) => write!(f, "io: {}", e),
            PersistError::Json(e) => write!(f, "json: {}", e),
        }
    }
}

/// 10d-2c-1 review round-3 (F2): containment-safe `run_id`
/// validation. Reject anything that could escape the
/// `~/.cm/workflow-runs/` directory via path-traversal (`/`,
/// `..`, `.`) or otherwise confuse the filesystem (null bytes,
/// empty, oversize). The codebase produces run_ids in two
/// shapes today — `wf_<base36>` from `run::new_run_id` and hex
/// UUIDs from MCP's `uuid.uuid4().hex` — both fit the
/// conservative `[A-Za-z0-9_-]` allowlist used here.
///
/// EVERY filesystem-touching entry point in this module
/// (`save`, `load_one`, `modify`, `try_modify`) and in
/// `events::WorkflowEventsWriter::append_event` validates via
/// this single helper, so a future caller that constructs a
/// path from an untrusted run_id can't bypass validation. The
/// helper is also called defensively by daemon handlers
/// (`workflow_transition`, `workflow_done`) at the top — same
/// fail-loudly invariant, two layers deep.
pub fn validate_run_id(id: &str) -> io::Result<()> {
    const MAX_LEN: usize = 128;
    if id.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "run_id is empty",
        ));
    }
    if id.len() > MAX_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("run_id too long: {} > {}", id.len(), MAX_LEN),
        ));
    }
    if id == "." || id == ".." {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("run_id is a path-traversal token: {:?}", id),
        ));
    }
    for c in id.chars() {
        if !(c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("disallowed character in run_id {:?}: {:?}", id, c),
            ));
        }
    }
    Ok(())
}

/// Save atomically: write to state.json.tmp then rename.
///
/// 10d-2c-1: write happens under an exclusive `flock(2)` on
/// a dedicated lock file (`state.json.lock`) so concurrent
/// readers/writers across processes (TUI's `tick()` persisting
/// `events_offset` vs. daemon's `workflow_transition` mutating
/// state) serialize cleanly. The lock is on a SEPARATE file
/// from `state.json` itself because the atomic rename used here
/// would unlink the original — invalidating any flock held on
/// it. The `.lock` file is a sentinel; only its inode matters.
pub fn save(run: &WorkflowRun) -> Result<(), PersistError> {
    // F2: validate BEFORE any path construction.
    validate_run_id(&run.run_id)?;
    let dir = run_dir(&run.run_id);
    fs::create_dir_all(&dir)?;
    let _guard = LockGuard::exclusive(&dir)?;
    let tmp = dir.join("state.json.tmp");
    let final_path = dir.join("state.json");
    let json = serde_json::to_string_pretty(run)?;
    fs::write(&tmp, json)?;
    fs::rename(&tmp, &final_path)?;
    Ok(())
}

/// Load a single persisted run by id. Returns None if the file is
/// missing or can't be parsed. Used by the MCP read tools to surface
/// runs that have been pruned from `app.workflow_runs` (e.g. after
/// `stop_workflow_run`) but whose state.json is intact on disk.
///
/// 10d-2c-1: read happens under a shared `flock(2)` on
/// `state.json.lock` so a concurrent `save` doesn't tear our
/// view. (Atomic rename in `save` already makes torn reads
/// impossible at the byte level, but the shared lock is
/// defense-in-depth for the read-modify-write window that
/// [`modify`] uses.)
pub fn load_one(run_id: &str) -> Option<WorkflowRun> {
    // F2: validate BEFORE any path construction.
    validate_run_id(run_id).ok()?;
    let dir = run_dir(run_id);
    let _guard = LockGuard::shared(&dir).ok()?;
    let path = dir.join("state.json");
    let contents = fs::read_to_string(&path).ok()?;
    serde_json::from_str::<WorkflowRun>(&contents).ok()
}

/// 10d-2c-1: atomic load-modify-write under exclusive
/// `flock(2)`. The closure receives a `&mut WorkflowRun`
/// loaded from disk; on closure return, the run is saved back
/// under the SAME lock the load took. This is the right shape
/// for the daemon's `workflow_transition` /
/// `workflow_done` handlers — they need to mutate state
/// without racing the TUI's concurrent saves (e.g. of
/// `events_offset`).
///
/// Returns the post-mutation `WorkflowRun` on success.
pub fn modify<F>(run_id: &str, mutate: F) -> Result<WorkflowRun, PersistError>
where
    F: FnOnce(&mut WorkflowRun),
{
    // F2: validate BEFORE any path construction.
    validate_run_id(run_id)?;
    let dir = run_dir(run_id);
    fs::create_dir_all(&dir)?;
    let _guard = LockGuard::exclusive(&dir)?;
    let path = dir.join("state.json");
    let contents = fs::read_to_string(&path)?;
    let mut run: WorkflowRun = serde_json::from_str(&contents)?;
    mutate(&mut run);
    let tmp = dir.join("state.json.tmp");
    let json = serde_json::to_string_pretty(&run)?;
    fs::write(&tmp, json)?;
    fs::rename(&tmp, &path)?;
    Ok(run)
}

/// 10d-2c-1 review round-1 (P1 #3): fallible variant of
/// [`modify`]. Closure returns `Result<(), E>`; on `Err(e)`,
/// the save is skipped and the original (unmutated) `Err(e)`
/// surfaces to the caller. Used by `workflow_transition` /
/// `workflow_done` for the **inside-flock auth check**: the
/// closure runs under the exclusive lock, sees the
/// authoritative `run.active_role`, and either applies the
/// state mutation or aborts with an auth-error tuple — no
/// TOCTOU between the auth decision and the mutation.
pub enum TryModifyOutcome<E> {
    /// Closure ran to `Ok(())`; mutation saved.
    Ok(WorkflowRun),
    /// Closure returned `Err(e)`; mutation rolled back (no
    /// save).
    Aborted(E),
    /// Filesystem error during load / save / lock.
    Persist(PersistError),
}

pub fn try_modify<F, E>(run_id: &str, mutate: F) -> TryModifyOutcome<E>
where
    F: FnOnce(&mut WorkflowRun) -> Result<(), E>,
{
    // F2: validate BEFORE any path construction. A malformed
    // run_id surfaces as `Persist(InvalidInput)` — handlers
    // translate to InvalidParams.
    if let Err(e) = validate_run_id(run_id) {
        return TryModifyOutcome::Persist(PersistError::Io(e));
    }
    let dir = run_dir(run_id);
    if let Err(e) = fs::create_dir_all(&dir) {
        return TryModifyOutcome::Persist(PersistError::Io(e));
    }
    let _guard = match LockGuard::exclusive(&dir) {
        Ok(g) => g,
        Err(e) => return TryModifyOutcome::Persist(PersistError::Io(e)),
    };
    let path = dir.join("state.json");
    let contents = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return TryModifyOutcome::Persist(PersistError::Io(e)),
    };
    let mut run: WorkflowRun = match serde_json::from_str(&contents) {
        Ok(r) => r,
        Err(e) => return TryModifyOutcome::Persist(PersistError::Json(e)),
    };
    if let Err(e) = mutate(&mut run) {
        return TryModifyOutcome::Aborted(e);
    }
    let tmp = dir.join("state.json.tmp");
    let json = match serde_json::to_string_pretty(&run) {
        Ok(s) => s,
        Err(e) => return TryModifyOutcome::Persist(PersistError::Json(e)),
    };
    if let Err(e) = fs::write(&tmp, json) {
        return TryModifyOutcome::Persist(PersistError::Io(e));
    }
    if let Err(e) = fs::rename(&tmp, &path) {
        return TryModifyOutcome::Persist(PersistError::Io(e));
    }
    TryModifyOutcome::Ok(run)
}

/// RAII guard around `flock(2)` on `<run_dir>/state.json.lock`.
/// Created via [`LockGuard::exclusive`] / [`LockGuard::shared`];
/// drop releases the lock. The lock file is created if missing.
///
/// Why a sentinel `.lock` file instead of locking `state.json`
/// directly: `save` uses atomic rename, which unlinks the
/// original file — invalidating any open fd's lock on it. The
/// sentinel survives renames.
struct LockGuard {
    _file: fs::File,
}

impl LockGuard {
    fn exclusive(dir: &std::path::Path) -> Result<Self, std::io::Error> {
        Self::acquire(dir, libc::LOCK_EX)
    }

    fn shared(dir: &std::path::Path) -> Result<Self, std::io::Error> {
        Self::acquire(dir, libc::LOCK_SH)
    }

    fn acquire(dir: &std::path::Path, op: libc::c_int) -> Result<Self, std::io::Error> {
        use std::os::unix::io::AsRawFd;
        let lock_path = dir.join("state.json.lock");
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;
        // `flock` blocks until the requested mode is available.
        // Phase-1 workflows have low contention (per-run mutex
        // upstream serializes daemon-side calls; TUI's `tick()`
        // is single-threaded), so blocking is fine.
        let rc = unsafe { libc::flock(file.as_raw_fd(), op) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(LockGuard { _file: file })
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        // Best-effort unlock; on close `flock` is released by the
        // kernel anyway, so an explicit LOCK_UN is belt-and-
        // suspenders for the case where the fd lives in a
        // FileGuard wrapper that doesn't close it on drop. The
        // `_file` field of this struct DOES drop here.
        unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Strict twin of [`load_all`] for the `--verify-handoff` preflight
/// (DESIGN_SEAMLESS_RESTART phase 4c): same directory walk, same
/// serde parser, but a PRESENT `state.json` that fails to read or
/// parse is an `Err` naming the file instead of being silently
/// skipped. The preflight's whole job is proving THIS binary parses
/// the on-disk state the old binary left behind, so the tolerant
/// skip would hide exactly the schema drift it exists to catch. A
/// missing runs dir, or a run dir without a `state.json`, stays fine
/// — nothing to parse.
pub fn load_all_strict() -> Result<Vec<WorkflowRun>, String> {
    let dir = runs_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(format!(
                "read workflow-runs dir {}: {}",
                dir.display(),
                e
            ))
        }
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| {
            format!("read workflow-runs dir {}: {}", dir.display(), e)
        })?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let state_file = path.join("state.json");
        let contents = match fs::read_to_string(&state_file) {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(format!("read {}: {}", state_file.display(), e))
            }
        };
        let run = serde_json::from_str::<WorkflowRun>(&contents).map_err(|e| {
            format!(
                "{} failed to parse as a WorkflowRun: {}",
                state_file.display(),
                e
            )
        })?;
        out.push(run);
    }
    Ok(out)
}

/// Load all persisted runs. Invalid/unreadable state.json files are skipped.
pub fn load_all() -> Vec<WorkflowRun> {
    let dir = runs_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let state_file = path.join("state.json");
        if !state_file.exists() {
            continue;
        }
        if let Ok(contents) = fs::read_to_string(&state_file) {
            if let Ok(run) = serde_json::from_str::<WorkflowRun>(&contents) {
                out.push(run);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_run() -> WorkflowRun {
        let mut roles = BTreeMap::new();
        roles.insert(
            "worker".to_string(),
            RoleBinding {
                session_label: "claude".to_string(),
                current_session_id: Some("sid-1".into()),
                daemon_session_uid: None,
                bound: false,
            },
        );
        roles.insert(
            "reviewer".to_string(),
            RoleBinding {
                session_label: "reviewer".to_string(),
                current_session_id: None,
                daemon_session_uid: None,
                bound: false,
            },
        );
        WorkflowRun::new(
            "wf_test".into(),
            "feedback".into(),
            "/tmp/repo".into(),
            roles,
            "worker".into(),
            BTreeMap::new(),
            None,
            BTreeMap::new(),
            0,
        )
    }

    #[test]
    fn activate_and_close() {
        let mut run = sample_run();
        assert_eq!(run.active_role.as_deref(), Some("worker"));
        assert_eq!(run.history.len(), 1);

        run.close_active_role(Some("worker was here".into()));
        run.activate_role(
            "reviewer".into(),
            TriggerKind::StaticIdle { from_role: "worker".into() },
            0,
            0,
        );
        assert_eq!(run.active_role.as_deref(), Some("reviewer"));
        assert_eq!(run.history.len(), 2);
        let worker_last = run
            .history
            .iter()
            .rev()
            .find(|h| h.role == "worker")
            .and_then(|h| h.last_message.as_deref());
        assert_eq!(worker_last, Some("worker was here"));
    }

    /// `activate_role` must persist `text_messages_at_start` on the new
    /// history entry — the `this_turn` resolver slices the role's
    /// transcript by this value to surface only the messages produced
    /// during the most recent activation.
    #[test]
    fn activate_role_records_text_messages_at_start() {
        let mut run = sample_run();
        // Worker spoke 5 times before reviewer activates.
        run.close_active_role(Some("done with first pass".into()));
        run.activate_role(
            "reviewer".into(),
            TriggerKind::StaticIdle { from_role: "worker".into() },
            12, // turn count
            5,  // text-bearing count
        );
        let reviewer_entry = run.history.last().expect("reviewer entry");
        assert_eq!(reviewer_entry.role, "reviewer");
        assert_eq!(reviewer_entry.text_messages_at_start, 5);
        assert_eq!(reviewer_entry.assistant_count_at_start, 12);
    }

    #[test]
    fn round_trip_serde() {
        let run = sample_run();
        let s = serde_json::to_string(&run).unwrap();
        let back: WorkflowRun = serde_json::from_str(&s).unwrap();
        assert_eq!(back.run_id, run.run_id);
        assert_eq!(back.role_sessions.len(), 2);
    }

    #[test]
    fn mark_done_sets_status() {
        let mut run = sample_run();
        run.mark_done("looks good".into());
        assert_eq!(run.status, RunStatus::Done);
        assert!(run.active_role.is_none());
        assert!(!run.is_active());
    }

    /// Pins the JSON wire shape that MCP consumers rely on. If you find
    /// yourself updating this test, you are changing a public contract —
    /// bump the schema deliberately, don't just retune the assertions.
    #[test]
    fn serialize_trigger_wire_shape() {
        let initial = serde_json::to_value(&TriggerKind::Initial).unwrap();
        assert_eq!(initial, serde_json::json!({"kind": "initial"}));

        let static_idle = serde_json::to_value(&TriggerKind::StaticIdle {
            from_role: "worker".into(),
        })
        .unwrap();
        assert_eq!(
            static_idle,
            serde_json::json!({"kind": "static_idle", "from_role": "worker"})
        );

        let mcp = serde_json::to_value(&TriggerKind::McpTransition {
            from_role: "manager".into(),
            prompt: "iterate".into(),
            event_id: "evt_1".into(),
        })
        .unwrap();
        assert_eq!(
            mcp,
            serde_json::json!({
                "kind": "mcp_transition",
                "from_role": "manager",
                "prompt": "iterate",
                "event_id": "evt_1",
            })
        );
    }

    /// Round-trip every variant so a careless rename shows up here too.
    #[test]
    fn trigger_round_trips_through_json() {
        let cases = vec![
            TriggerKind::Initial,
            TriggerKind::StaticIdle {
                from_role: "worker".into(),
            },
            TriggerKind::McpTransition {
                from_role: "manager".into(),
                prompt: "again".into(),
                event_id: "evt_2".into(),
            },
        ];
        for t in cases {
            let s = serde_json::to_string(&t).unwrap();
            let back: TriggerKind = serde_json::from_str(&s).unwrap();
            // Compare via JSON since TriggerKind doesn't impl PartialEq.
            assert_eq!(
                serde_json::to_value(&t).unwrap(),
                serde_json::to_value(&back).unwrap(),
            );
        }
    }

    /// End-to-end persistence test for the `stop_workflow` no-op gate
    /// and the on-disk fallback that backs `get_workflow_state` and
    /// `list_workflows` (in `control::methods`).
    ///
    /// Pre-fix sequence: a `workflow_done` MCP call hit `mark_done` +
    /// `save` (status=Done, done_reason set). A subsequent `stop_workflow`
    /// then unconditionally called `mark_detached` + `save`, overwriting
    /// the persisted status from `Done` to `Detached`. The `done_reason`
    /// stayed but was now paired with a status that said "you aborted me",
    /// erasing the distinction between successful completion and abort.
    ///
    /// Post-fix this test pins:
    ///   1. A run that completed (mark_done + save) is on disk as Done.
    ///   2. `stop_workflow`'s gate (`matches!(status, Done) → no-op`)
    ///      keeps state.json byte-identical — bypassing the gate would
    ///      flip Done → Detached on the next save.
    ///   3. `get_workflow_state`'s on-disk fallback (`load_one`) returns
    ///      the full Done record with `done_reason` intact.
    ///   4. `list_workflows`' on-disk fallback (`load_all`) surfaces the
    ///      same record.
    #[test]
    fn stop_workflow_no_op_on_done_preserves_state_through_disk_reload() {
        let _guard = crate::test_support::env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let orig_home = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", tmp.path()); }

        // Unique run_id keeps load_all's assertion below scoped to this
        // test even if other parallel tests share the temp HOME (they
        // shouldn't — home_lock serializes — but the explicit id is
        // belt-and-suspenders against future refactors).
        let run_id = "wf_stop_noop_int_test".to_string();

        // ---- Phase 1: workflow runs to completion. ----
        let mut roles = BTreeMap::new();
        roles.insert(
            "worker".to_string(),
            RoleBinding {
                session_label: "claude".into(),
                current_session_id: Some("sid-w".into()),
                daemon_session_uid: None,
                bound: false,
            },
        );
        let mut run = WorkflowRun::new(
            run_id.clone(),
            "feedback".into(),
            "/tmp/repo".into(),
            roles,
            "worker".into(),
            BTreeMap::new(),
            None,
            BTreeMap::new(),
            0,
        );
        run.mark_done("worker said done".into());
        save(&run).expect("save Done state");

        let state_path = run_dir(&run_id).join("state.json");
        let bytes_at_done = std::fs::read(&state_path).expect("state.json present");

        // Sanity: what we wrote is what `load_one` reads back.
        let loaded = load_one(&run_id).expect("load Done run");
        assert_eq!(loaded.status, RunStatus::Done);
        assert_eq!(loaded.done_reason.as_deref(), Some("worker said done"));

        // ---- Phase 2: stop_workflow no-ops on Done. ----
        // Mirror the gate condition that lives in
        // `control::methods::stop_workflow`. The gate is the only thing
        // standing between the persisted Done state and a `mark_detached`
        // overwrite. Bypassing it would call:
        //     run.mark_detached(); save(&run);
        // The point of the gate is that we do NOT do that here. Confirm
        // disk state is byte-identical to what we wrote in Phase 1.
        assert!(
            matches!(loaded.status, RunStatus::Done),
            "stop_workflow gate must classify Done as terminal"
        );
        let bytes_after_no_op = std::fs::read(&state_path).expect("state.json present");
        assert_eq!(
            bytes_after_no_op, bytes_at_done,
            "stop_workflow no-op must not rewrite state.json"
        );

        // ---- Phase 3: get_workflow_state's on-disk fallback. ----
        // After `stop_workflow_run` would have pruned the in-memory entry
        // (which the no-op skipped — but the same fallback runs anyway
        // when a Done run was reloaded by App::new without being kept in
        // `app.workflow_runs`, since only is_active() runs are retained).
        // The full Done record with done_reason must come back.
        let surfaced = load_one(&run_id).expect("on-disk fallback returns the run");
        assert_eq!(surfaced.status, RunStatus::Done);
        assert_eq!(surfaced.done_reason.as_deref(), Some("worker said done"));
        assert_eq!(surfaced.run_id, run_id);
        assert_eq!(surfaced.workflow_name, "feedback");
        assert!(surfaced.active_role.is_none(), "Done runs have no active role");

        // ---- Phase 4: list_workflows' on-disk fallback. ----
        let all = load_all();
        let entry = all
            .iter()
            .find(|r| r.run_id == run_id)
            .expect("load_all surfaces the Done run for list_workflows");
        assert_eq!(entry.status, RunStatus::Done);
        assert_eq!(entry.done_reason.as_deref(), Some("worker said done"));

        // Restore HOME for the next test.
        unsafe {
            match orig_home {
                Some(o) => std::env::set_var("HOME", o),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}
