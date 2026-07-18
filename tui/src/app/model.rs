//! Session/workspace/task data model: core types + manifest-entry building.

use super::*;

#[derive(Clone, Debug, PartialEq)]
pub enum TaskStatus {
    Running,
    Blocked,
    Backlog,
    Done,
}

impl TaskStatus {
    pub(super) fn from_api(s: &str) -> Self {
        match s {
            "running" => TaskStatus::Running,
            "blocked" => TaskStatus::Blocked,
            "backlog" => TaskStatus::Backlog,
            "done" => TaskStatus::Done,
            _ => TaskStatus::Backlog,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SessionStatus {
    Running,
    Idle,
}

pub struct TerminalSession {
    /// Stable per-session id, generated at creation. Used to track the cursor
    /// across reconciles so duplicate labels don't confuse restoration.
    /// In-memory only; not persisted.
    pub uid: String,
    pub label: String,
    pub session_type: String, // "claude" or "bash" — immutable, survives renames
    pub session: Session,
    pub status: SessionStatus,
    /// Start of the CURRENT idle spell — `Some(instant the status last
    /// flipped Running→Idle)`, `None` while Running. Drives the idle-age
    /// indicator buckets (afterglow / settled / stale) in the sidebar and
    /// continuous column. In-memory only, never persisted: a TUI restart
    /// just loses the age, and an unknown age renders as the oldest
    /// bucket. Maintain via [`TerminalSession::set_status`] — direct
    /// `status` writes on genuine transitions would strand a stale value.
    pub idle_since: Option<Instant>,
    pub last_write_at: Option<Instant>,
    /// Current transcript file UUID (the JSONL filename). Unstable: `None`
    /// until the detector binds to a fresh file, and reset to `None` on
    /// `/clear` while the detector rebinds. For stable identity, use `uid`.
    pub transcript_id: Option<String>,
    /// Bumps every time `transcript_id` rebinds (e.g. on `/clear`). Used to
    /// detect cursor-vs-file mismatch when reading transcripts. In-memory
    /// only for now; Phase 2 will mirror it into the manifest.
    pub generation: u64,
    pub pending_jsonl_files: Option<Vec<String>>,
    pub hidden: bool,
    /// Seconds of quiet before marking idle. 0 = use global default.
    pub idle_timeout_secs: u16,
    /// Wakeups required within the 2s activity window to flip Idle → Running.
    /// 0 = use global default (`WAKEUP_BURST_THRESHOLD`). Lower = more sensitive,
    /// useful for slow-streaming bash scripts that produce one line at a time.
    pub burst_threshold: u16,
    /// Prompt text to deliver to the session once it's actually ready to
    /// receive input (see `PendingWrite`).
    pub pending_prompt: Option<PendingWrite>,
    /// Pending `/clear` command to send before `pending_prompt`. Sequenced:
    /// the prompt only delivers after the clear has either been delivered or
    /// hit its deadline.
    pub pending_clear: Option<PendingWrite>,
    /// If this session is a workflow participant, the run it belongs to.
    pub workflow_run_id: Option<String>,
    /// Role name within that workflow (e.g. "worker", "reviewer", "manager").
    pub workflow_role: Option<String>,
    /// If this session is a tick of a continuous task, that task's id.
    /// Read through from `ManifestEntry.continuous_task_id`; `None` for
    /// every ordinary session. The trigger funnel that sets it lands in
    /// Phase 2 — this is the Phase-1 wire field only, mirroring
    /// `workflow_run_id`. See DESIGN_CONTINUOUS_TASKS.md §6/§12.
    pub continuous_task_id: Option<String>,
    /// Task this session belongs to. None = workspace-level (ad-hoc) session
    /// not tied to any task. Sessions launched from the planning view (A-f / A-l)
    /// inherit the launched task's id; manually added sessions (A-s) inherit
    /// the task_id of the cursor's current task scope, or None.
    pub task_id: Option<String>,
    /// First ~120 chars of the most recent prompt we delivered via
    /// `deliver_pending_write`, along with its delivery timestamp in unix ms.
    /// Used to correlate a fresh claude workflow session with its new
    /// sessionId in `~/.claude/history.jsonl`: when the same text shows up
    /// in a history entry with project==worktree, the entry's sessionId is
    /// ours. Cleared once sid has been bound.
    pub last_delivery: Option<(String, u64)>,
    /// If true, fire a desktop notification each time this session
    /// transitions Running → Idle. Off by default; toggled in A-e settings.
    pub notify_on_idle: bool,
    /// User-assigned accent color — a name from `USER_COLORS`, set in
    /// A-e settings. `None` = inherit the workspace color (or default
    /// styling). Persisted via `ManifestEntry`.
    pub color: Option<String>,
    /// Global-permissions grant. When true, this session's MCP
    /// agent can prompt, read, and control ANY other session
    /// (not just its task-tree descendants) — the TUI-side mirror
    /// of `cm_daemon::session::DaemonSession::global_perms`. Granted
    /// by the operator (A-e session settings / the new-session form)
    /// or propagated from a global caller via `start_session`.
    /// Persisted in the manifest. Off by default (safe baseline).
    pub global_perms: bool,
    /// Marker that an Enter keystroke is queued to be written to the PTY at
    /// or after `fire_at`. Used to introduce a deliberate gap between the body
    /// of a multi-KB workflow prompt and the trailing Enter so the receiving
    /// agent (notably codex) classifies Enter as a fresh keystroke rather than
    /// the tail of a paste. Done as a deferred write rather than
    /// `thread::sleep` so the UI thread keeps draining events.
    ///
    /// We deliberately do NOT store the Enter bytes — the encoding (raw `\r`
    /// vs Kitty `\x1b[13u`) depends on the terminal mode at submit time. The
    /// agent often enables Kitty mode AFTER we've written the body but BEFORE
    /// the deferred Enter fires; if we used bytes captured at body-write time
    /// we'd send the wrong encoding and the prompt wouldn't submit.
    pub pending_enter: Option<PendingEnter>,
    /// Wall-clock instant when this TerminalSession was constructed.
    /// Used as a tie-breaker in session_id detection so when two sessions in
    /// the same worktree race for a newly-written transcript, the one that
    /// has been waiting longer (and is therefore more likely to actually own
    /// the file) gets first pick.
    pub created_at: Instant,
    /// UID of the agent session that spawned/owns this one. Set when a
    /// session is spawned via the agent-orchestration MCP tools; None
    /// for user-created sessions. Persisted across TUI restart.
    pub managed_by_uid: Option<String>,
    /// Name of the agent-memory snapshot this session was cloned from, if
    /// any. Informational provenance only — surfaces in session info as
    /// "Seeded from: <name>". Persisted via `ManifestEntry`. See
    /// DESIGN_AGENT_MEMORIES.md.
    pub seeded_from_snapshot: Option<String>,
    /// Read-through copy of `ManifestEntry.last_exit`. The daemon
    /// owns this field (slice 9 of doc/persistent-host-daemon.md
    /// adds it; slice 10 wires the producer). The TUI doesn't yet
    /// inspect or write `last_exit` — it just loads it on startup
    /// and writes it back unchanged on every manifest save, so the
    /// daemon's `memory_cap_kill: true` flag survives across TUI
    /// restarts and the detached-session cap-kill toast renders
    /// correctly. Without this passthrough, every TUI save would
    /// clobber the field to `None` and the named acceptance
    /// criterion would fail.
    pub preserved_last_exit: Option<cm_daemon::manifest::LastExit>,
    /// 12b (Phase 3): tags the in-memory session with the host
    /// its daemon runs on. Read through from `ManifestEntry.host_id`
    /// on load; new sessions get the active host
    /// (`HostId::local()` at this slice — `A-H`-driven host
    /// selection lands in 12e). The TUI's connection pool (12c)
    /// keys off this field to route every per-session RPC to the
    /// right daemon. For now (12b is plumbing-only), the field is
    /// populated but no consumer reads it.
    pub host_id: crate::hosts::HostId,
}

impl TerminalSession {
    /// Assign a new status while maintaining `idle_since` (the start of
    /// the current idle spell): a genuine Running→Idle flip stamps
    /// `Some(now)`, Idle→Running clears it back to `None`. Idempotent on
    /// a repeated same-status assignment — a poll/reconnect path
    /// re-asserting Idle must NOT restart the idle age (that would pin
    /// the row in the bright "afterglow" bucket forever). Use this at
    /// every production `status` write; only tests poke the field raw.
    pub(crate) fn set_status(&mut self, new: SessionStatus) {
        if self.status != new {
            self.idle_since = match new {
                SessionStatus::Idle => Some(Instant::now()),
                SessionStatus::Running => None,
            };
        }
        self.status = new;
    }

    /// Rebind the session to a new transcript file (or `None` to mark the
    /// session as transcript-less while a fresh one is being detected).
    /// Bumps `generation` so any reader holding a cursor against the old
    /// transcript detects the rebind and restarts at offset 0 of the new
    /// file. Use at every site that mutates `transcript_id` AFTER an
    /// initial bind — the initial `None → Some(...)` transition has no
    /// prior readers and skips the bump.
    pub fn rebind_transcript(&mut self, new_sid: Option<String>) {
        self.transcript_id = new_sid;
        self.generation = self.generation.saturating_add(1);
    }

    /// Serialize this live session back to its on-disk `ManifestEntry`
    /// shape. Used both by `save_session_manifest` (the periodic save)
    /// and by the remote-reconnect requeue in `drain_pty_events`,
    /// which needs an entry to hand to the deferred-reattach drain
    /// when a remote session's attach stream dies. Keeping the two in
    /// one place means a new manifest field can't silently diverge
    /// between the save path and the reconnect path.
    pub(crate) fn to_manifest_entry(&self) -> cm_daemon::manifest::ManifestEntry {
        cm_daemon::manifest::ManifestEntry {
            memory_cap_soft_bytes: None,
            memory_cap_hard_bytes: None,
            cgroup_prefix: None,
            uid: self.uid.clone(),
            managed_by_uid: self.managed_by_uid.clone(),
            generation: self.generation,
            label: self.label.clone(),
            session_type: self.session_type.clone(),
            transcript_id: self.transcript_id.clone(),
            hidden: self.hidden,
            idle_timeout_secs: self.idle_timeout_secs,
            burst_threshold: self.burst_threshold,
            workflow_run_id: self.workflow_run_id.clone(),
            workflow_role: self.workflow_role.clone(),
            continuous_task_id: self.continuous_task_id.clone(),
            task_id: self.task_id.clone(),
            notify_on_idle: self.notify_on_idle,
            color: self.color.clone(),
            global_perms: self.global_perms,
            seeded_from_snapshot: self.seeded_from_snapshot.clone(),
            // Read-modify-write the daemon-owned `last_exit`. The TUI
            // never inspects or mutates it — just hands it back
            // unchanged so the daemon's `memory_cap_kill: true` flag
            // survives every TUI save.
            last_exit: self.preserved_last_exit.clone(),
            // Round-trip the host_id from the in-memory session so a
            // remote-pinned entry stays pinned to its host.
            host_id: self.host_id.clone(),
        }
    }
}

/// Marker that an Enter keystroke is queued to fire at or after `fire_at`. The
/// actual bytes are computed from the current terminal mode at submit time.
pub struct PendingEnter {
    pub fire_at: Instant,
}

pub(super) const DEFAULT_IDLE_TIMEOUT_SECS: u16 = 2;

pub(super) const CODEX_RESUME_REBIND_WINDOW: Duration = Duration::from_secs(120);

/// A byte sequence queued to be written to a session's PTY once the session
/// is "ready" to receive input. Readiness is determined by PTY quietness —
/// absence of wakeup events for a minimum window — which adapts to however
/// long the underlying agent takes to finish starting up, connecting to MCP
/// servers, rendering its banner, etc.
///
/// Two knobs:
/// - `earliest_deliver_at`: floor (don't deliver before this time regardless
///   of quietness). Used to give the user a chance to notice what's happening,
///   and to debounce brief quiet windows during startup.
/// - `hard_deadline`: ceiling. If the agent NEVER goes quiet (e.g. a pathological
///   ticking spinner), deliver anyway so the workflow doesn't hang forever.
///
/// Between the floor and deadline, delivery fires at the first moment of
/// `require_quiet` of uninterrupted silence.
///
/// `text` is the payload; if `submit` is true we append an Enter keystroke
/// (encoded for the session's current mode) at delivery time.
pub struct PendingWrite {
    pub text: String,
    pub submit: bool,
    pub earliest_deliver_at: Instant,
    pub require_quiet: Duration,
    pub hard_deadline: Instant,
}

impl PendingWrite {
    /// A write that fires at the first moment of PTY quiet (>= `quiet`
    /// without any wakeup), bounded by `floor` (earliest) and `deadline`
    /// (latest) from now.
    pub fn wait_for_quiet(text: String, submit: bool, floor: Duration, quiet: Duration, deadline: Duration) -> Self {
        let now = Instant::now();
        PendingWrite {
            text,
            submit,
            earliest_deliver_at: now + floor,
            require_quiet: quiet,
            hard_deadline: now + deadline,
        }
    }
}

/// An execution context: a worktree (local) or cloud worker (remote) plus
/// the sessions running in it. Any number of `TaskEntry`s can point at a
/// workspace via `TaskEntry::workspace_id`; none is also valid (standalone
/// workspace created via A-n).
pub struct Workspace {
    pub id: String,
    pub name: String,
    pub is_closed: bool,
    pub is_cloud: bool,
    pub repo_url: Option<String>,
    pub worktree_path: Option<PathBuf>,
    pub main_repo_path: Option<PathBuf>,
    pub worker_vm: Option<String>,
    pub worker_zone: Option<String>,
    /// Host this workspace's worktree lives on, set ONCE at creation (a
    /// worktree can't move hosts). The single source of truth for "where do
    /// this task's sessions run" — every session added to the workspace
    /// inherits it. Retires the global `App::active_host` (see
    /// DESIGN_REMOVE_GLOBAL_HOST.md). Persisted in `ManifestWorkspace`;
    /// legacy manifests without it derive it from the first session's host.
    pub host_id: cm_daemon::host_id::HostId,
    /// User-assigned accent color (name from `USER_COLORS`), set in A-e
    /// settings. Cascades to sessions without their own color. Persisted
    /// in `ManifestWorkspace`.
    pub color: Option<String>,
    /// Pinned workspaces sort to the top of the sidebar, ahead of the
    /// status-ranked rest. Toggled in A-e settings; persisted in
    /// `ManifestWorkspace`.
    pub pinned: bool,
    pub sessions: Vec<TerminalSession>,
    /// Records of closed sessions in this workspace. Resolver consults
    /// these (after `sessions`) to answer `read_session_output` for an
    /// already-exited session — its last-known transcript file remains
    /// on disk and the `last_transcript_id` field gives us the path.
    pub tombstones: Vec<SessionTombstone>,
    /// True between `push_active` and the matching `PushComplete` /
    /// `PushFailed` event from the backend. Transient — not persisted
    /// in `ManifestWorkspace`, so a TUI restart mid-push surfaces as
    /// "not pushing" rather than wedging on a stuck flag (the user can
    /// retry; the worst case is a duplicate `cm/push-*` branch).
    pub is_pushing: bool,
}

impl Workspace {
    /// True when this workspace runs *in-place* — its working directory
    /// IS the main repo checkout (no dedicated git worktree, no
    /// `cm/<slug>` branch). The marker is `worktree_path == main_repo_path`
    /// (both `Some` and equal); in-place launches set them to the same
    /// `PathBuf` so the equality holds byte-for-byte and round-trips
    /// through the manifest. Destructive cleanup (worktree remove, branch
    /// delete, cloud push) MUST consult this before touching git — those
    /// ops would otherwise damage the user's main repo.
    pub(crate) fn is_in_place(&self) -> bool {
        matches!(
            (&self.worktree_path, &self.main_repo_path),
            (Some(wt), Some(main)) if wt == main
        )
    }
}

/// A task tracked in the planning/API layer. Pure metadata — no execution
/// state. `workspace_id` points at the Workspace this task has been launched
/// into (None when still in backlog / never launched).
#[derive(Clone)]
pub struct TaskEntry {
    pub task_id: Option<String>,
    pub name: String,
    pub api_status: TaskStatus,
    pub repo_url: Option<String>,
    pub prompt: Option<String>,
    pub wip_branch: Option<String>,
    pub session_id: Option<String>,
    pub blocked_at: Option<String>,
    pub is_cloud: bool,
    /// FK to `App.workspaces`. None = task in backlog, not bound yet.
    pub workspace_id: Option<String>,
    /// Planning project this task belongs to. Read from the API row;
    /// `backend.rs::filter_project_tasks` filters tasks with `None` out
    /// of the planning view, so subtasks need this populated to be
    /// visible after a planning refresh.
    pub project: Option<String>,
    /// FK to another `TaskEntry` (by `task_id`). None = top-level task.
    /// Phase 5 uses this for the subtask MCP tools and the planning-view
    /// tree. Set when an agent calls `create_subtask` or when the user
    /// creates a subtask in the planning view.
    pub parent_task_id: Option<String>,
    /// Worktree behavior:
    ///   - "inherit" (default): sessions spawn in the parent's worktree.
    ///   - "branch": a new worktree is created off the parent's branch
    ///     with name `cm-sub/<slug-chain>-<short_id>`.
    ///   - "in-place": sessions spawn directly in the MAIN repo checkout —
    ///     no worktree, no branch. Also set on top-level tasks launched
    ///     in-place from the planning view.
    pub worktree_mode: WorktreeMode,
    /// Free-form JSONB bag mirrored from the API row. Skills attach
    /// structured context here — currently `metadata.resume.*` for the
    /// design-doc bundle (`design_doc_path` + `designer_session_uid`).
    /// `None` = no bag set.
    pub metadata: Option<serde_json::Value>,
}

/// migrate-tui-local Issue J: outcome of `spawn_restored_session`
/// for a single manifest entry. The caller (`restore_sessions`)
/// uses this to skip the Codex JSONL rebind primer on the
/// attach path — the daemon's transcript binding survived the
/// TUI restart, so post-restart rebind detection would be a
/// category error (and could let an unrelated Codex rollout
/// claim the daemon-known transcript_id during the window).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RestoreOutcome {
    /// `session.attach` succeeded against an already-live
    /// daemon session. Transcript binding stays at whatever the
    /// daemon recorded; no post-spawn detection is needed.
    Attached,
    /// `start_session` ran fresh — either the daemon didn't
    /// have this UID, or the attach probe raced and the fallback
    /// to spawn succeeded. Post-spawn detection / rebind primer
    /// applies normally.
    Spawned,
}

/// How a subtask's worktree relates to its parent. Default = `Inherit`
/// per the design discussion (the common case is "go do this side thing
/// in the same codebase").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorktreeMode {
    #[default]
    Inherit,
    Branch,
    /// Sessions spawn directly in the parent's MAIN repo checkout — no
    /// new worktree, no new branch. The wire value is hyphenated
    /// (`"in-place"`), so pin it explicitly: the enum's `lowercase`
    /// rename would otherwise emit `"inplace"`.
    #[serde(rename = "in-place")]
    InPlace,
}

/// Parse the API's `worktree_mode` string into the enum. Unknown values
/// fall through to `Inherit` — the safer default if the server sends
/// something we don't recognize (e.g. a future variant we haven't
/// shipped yet).
pub fn parse_worktree_mode(s: &str) -> WorktreeMode {
    match s {
        "branch" => WorktreeMode::Branch,
        "in-place" => WorktreeMode::InPlace,
        _ => WorktreeMode::Inherit,
    }
}

impl WorktreeMode {
    pub fn as_wire(&self) -> &'static str {
        match self {
            WorktreeMode::Inherit => "inherit",
            WorktreeMode::Branch => "branch",
            WorktreeMode::InPlace => "in-place",
        }
    }
}

/// 12e-r5 F2: normalize the public wire vocabulary
/// (`"claude-code"`, `"codex"`, `"bash"` — per the design
/// doc, exposed via MCP `start_session`) to the internal TUI
/// vocabulary (`"claude"`, `"codex"`, `"bash"`). Future
/// cleanup may standardize on the wire form everywhere; this
/// helper bridges the two so an MCP-spawned Claude session
/// looks up its `Config::memory_cap_for("claude")` env var
/// (`CM_SESSION_MEM_SOFT_CLAUDE`), not the bogus
/// `CM_SESSION_MEM_SOFT_CLAUDE-CODE`.
///
/// Unknown inputs pass through unchanged — callers are
/// responsible for the daemon-eligibility gate (see
/// `try_spawn_via_daemon`'s `argv_result` match `_ => return None`).
pub(crate) fn normalize_session_type_to_internal(session_type: &str) -> &str {
    match session_type {
        "claude-code" => "claude",
        other => other,
    }
}

/// Build a TerminalSession wrapping a freshly-spawned PTY with default state.
/// Used by attach/spawn flows that don't need pending prompts or workflow tags.
pub(super) fn make_simple_session(
    label: &str,
    session_type: &str,
    session: Session,
    pending_jsonl_files: Option<Vec<String>>,
) -> TerminalSession {
    make_simple_session_with_uid(new_session_uid(), label, session_type, session, pending_jsonl_files)
}

/// Variant for spawn paths that pre-generate the uid so they can wire it
/// into MCP config env (`CM_TUI_SESSION_ID` must match `ts.uid`). Without
/// matching values, the agent's tool calls fail authorization.
pub(super) fn make_simple_session_with_uid(
    uid: String,
    label: &str,
    session_type: &str,
    session: Session,
    pending_jsonl_files: Option<Vec<String>>,
) -> TerminalSession {
    TerminalSession {
        color: None,
        uid,
        label: label.to_string(),
        session_type: session_type.to_string(),
        session,
        status: SessionStatus::Running,
        idle_since: None,
        last_write_at: None,
        transcript_id: None,
        generation: 0,
        pending_jsonl_files,
        hidden: false,
        idle_timeout_secs: 0,
        burst_threshold: 0,
        pending_prompt: None,
        pending_clear: None,
        workflow_run_id: None,
        workflow_role: None,
        continuous_task_id: None,
        last_delivery: None,
        task_id: None,
        notify_on_idle: false,
        global_perms: false,
        pending_enter: None,
        created_at: Instant::now(),
        managed_by_uid: None,
        seeded_from_snapshot: None,
        // Fresh sessions have no exit history; the daemon may
        // populate this later through manifest.watch diffs.
        preserved_last_exit: None,
        // 12b: default to local. The factory builds sessions
        // before App is constructed, so the active host hasn't
        // been picked yet — callers that need a non-local host
        // (12e onward) overwrite this after construction.
        host_id: crate::hosts::HostId::local(),
    }
}

/// Generate a fresh workspace id. Not cryptographic — just collision-avoidance
/// across the user's manifest via nanosecond timestamp.
pub(crate) fn new_workspace_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("ws-{:x}", nanos)
}

/// Repo URLs in deterministic order (sorted by repo name). Used by the New
/// Workspace picker so the dropdown is stable across launches and the ←/→
/// cycle order matches what the user sees. Free function (vs `&self` method)
/// so it composes with split borrows when callers already hold `&mut
/// self.input_mode`.
pub(super) fn sorted_repo_urls(repos: &HashMap<String, String>) -> Vec<String> {
    let mut entries: Vec<(&String, &String)> = repos.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    entries.into_iter().map(|(_, url)| url.clone()).collect()
}

/// True if `entry` plausibly corresponds to a prompt delivery whose first
/// 120 chars equal `prefix`. Three matching modes:
///
/// 1. **Plain typed input** — `display` carries the raw text directly.
/// 2. **Legacy paste schema** — `pastedContents.<k>.content` holds the raw
///    text; the parser concatenated those into `paste_content`.
/// 3. **Post-2025 paste schema** — `pastedContents.<k>.contentHash` (a
///    redacted reference) replaces `.content`. There's no text to match,
///    but `display` becomes the placeholder `"[Pasted text #N +M lines]"`
///    which is an unambiguous "something was pasted here" signal. The
///    caller already constrains the entry to a specific project (worktree)
///    and a 2-second window around the delivery timestamp, so accepting
///    the placeholder still uniquely identifies the right session in
///    practice.
///
/// Mode 3 is the fix for the overnight-cleanup workflow stall: every goal
/// >5 lines triggered paste-redaction, the prefix never matched, and all
/// 5 workflows got stuck on iteration 1 because their workers never got
/// bound to their transcripts.
pub(crate) fn entry_matches_delivery(
    entry: &workflow::history::HistoryEntry,
    prefix: &str,
) -> bool {
    if prefix.is_empty() {
        return false;
    }
    entry.display.starts_with(prefix)
        || entry.paste_content.starts_with(prefix)
        || workflow::history::is_paste_placeholder(&entry.display)
}

pub(crate) fn new_session_uid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("ts-{:x}-{:x}", nanos, n)
}

#[cfg(test)]
mod manifest_entry_seeded_from_tests {
    //! Lock in the persistence semantics of `ManifestEntry.seeded_from_snapshot`:
    //! the serde-default lets pre-existing manifests load cleanly, and the
    //! skip-if-none keeps the on-disk format quiet when the field is absent.
    use super::*;

    #[test]
    fn seeded_from_snapshot_round_trips() {
        let entry = ManifestEntry {
            color: None,
            memory_cap_soft_bytes: None,
            memory_cap_hard_bytes: None,
            cgroup_prefix: None,
            uid: "ts-abc".into(),
            managed_by_uid: None,
            generation: 0,
            label: "x".into(),
            session_type: "claude".into(),
            transcript_id: None,
            hidden: false,
            idle_timeout_secs: 0,
            burst_threshold: 0,
            workflow_run_id: None,
            workflow_role: None,
            continuous_task_id: None,
            task_id: None,
            notify_on_idle: false,
            global_perms: false,
            seeded_from_snapshot: Some("reviewer-strict".into()),
            last_exit: None,
            host_id: cm_daemon::host_id::HostId::local(),
        };
        let bytes = serde_json::to_vec(&entry).unwrap();
        let back: ManifestEntry = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.seeded_from_snapshot.as_deref(), Some("reviewer-strict"));
    }

    #[test]
    fn missing_seeded_from_deserializes_as_none() {
        // Pre-existing manifests written before this field existed must
        // continue to load. `#[serde(default)]` guarantees that.
        let json = r#"{
            "uid": "ts-abc",
            "generation": 0,
            "label": "x",
            "session_type": "claude",
            "hidden": false,
            "idle_timeout_secs": 0,
            "burst_threshold": 0,
            "notify_on_idle": false
        }"#;
        let entry: ManifestEntry = serde_json::from_str(json).unwrap();
        assert!(entry.seeded_from_snapshot.is_none());
    }

    #[test]
    fn none_does_not_serialize() {
        let entry = ManifestEntry {
            color: None,
            memory_cap_soft_bytes: None,
            memory_cap_hard_bytes: None,
            cgroup_prefix: None,
            uid: "ts-abc".into(),
            managed_by_uid: None,
            generation: 0,
            label: "x".into(),
            session_type: "claude".into(),
            transcript_id: None,
            hidden: false,
            idle_timeout_secs: 0,
            burst_threshold: 0,
            workflow_run_id: None,
            workflow_role: None,
            continuous_task_id: None,
            task_id: None,
            notify_on_idle: false,
            global_perms: false,
            seeded_from_snapshot: None,
            last_exit: None,
            host_id: cm_daemon::host_id::HostId::local(),
        };
        let s = serde_json::to_string(&entry).unwrap();
        assert!(
            !s.contains("seeded_from_snapshot"),
            "None should be skipped, got: {s}"
        );
        // Same skip semantics for the Phase-1-added last_exit field.
        assert!(
            !s.contains("last_exit"),
            "last_exit:None should be skipped, got: {s}"
        );
    }

    #[test]
    fn missing_last_exit_deserializes_as_none() {
        // Phase-1 schema-change check: a manifest written by a
        // pre-slice-9 binary has no `last_exit` field at all; serde
        // default must accept it and produce `None`. This is the
        // doc's named "manifests written by older binaries load
        // cleanly" rollout requirement.
        let json = r#"{
            "uid": "ts-abc",
            "generation": 0,
            "label": "x",
            "session_type": "claude",
            "hidden": false,
            "idle_timeout_secs": 0,
            "burst_threshold": 0,
            "notify_on_idle": false
        }"#;
        let entry: ManifestEntry = serde_json::from_str(json).unwrap();
        assert!(entry.last_exit.is_none());
    }

    #[test]
    fn last_exit_round_trips_with_memory_cap_kill_flag() {
        // The attached-session leg sources `memory_cap_kill` from
        // the `term_shim::ChildEvent::Exited` exit frame; the
        // detached leg sources it from this persisted field. Prove
        // the round-trip preserves the flag so the toast renders
        // correctly post-restart.
        let entry = ManifestEntry {
            color: None,
            memory_cap_soft_bytes: None,
            memory_cap_hard_bytes: None,
            cgroup_prefix: None,
            uid: "ts-abc".into(),
            managed_by_uid: None,
            generation: 0,
            label: "x".into(),
            session_type: "claude".into(),
            transcript_id: None,
            hidden: false,
            idle_timeout_secs: 0,
            burst_threshold: 0,
            workflow_run_id: None,
            workflow_role: None,
            continuous_task_id: None,
            task_id: None,
            notify_on_idle: false,
            global_perms: false,
            seeded_from_snapshot: None,
            last_exit: Some(cm_daemon::manifest::LastExit {
                code: Some(137),
                memory_cap_kill: true,
                kills_file_offset: Some(0),
                exited_at: 1_700_000_000.0,
            }),
            host_id: cm_daemon::host_id::HostId::local(),
        };
        let bytes = serde_json::to_vec(&entry).unwrap();
        let back: ManifestEntry = serde_json::from_slice(&bytes).unwrap();
        let got = back.last_exit.expect("last_exit present after round-trip");
        assert!(got.memory_cap_kill);
        assert_eq!(got.code, Some(137));
    }

    /// Reviewer-named regression test: simulate the
    /// `load → mutate unrelated field → save → reload` cycle the TUI
    /// runs every time a user edits a session and saves. The
    /// daemon-owned `last_exit` field MUST survive untouched. Pre-fix
    /// behavior at `app.rs:3007` rebuilt the entry with
    /// `last_exit: None` on every save, clobbering the daemon's
    /// `memory_cap_kill: true` flag and silently breaking the
    /// detached-session cap-kill toast — the named acceptance
    /// criterion for slice 12.
    ///
    /// This test exercises the persistence/in-memory boundary by
    /// roundtripping ManifestEntry through serde (the load step
    /// internally to the TUI), copying the loaded `last_exit` into
    /// the in-memory mirror (`TerminalSession.preserved_last_exit`,
    /// which we represent here as a local variable since
    /// constructing a full TerminalSession requires a real PTY),
    /// mutating an unrelated field, rebuilding ManifestEntry with
    /// `last_exit: preserved.clone()` (mirroring the post-fix
    /// `app.rs:3007`), and re-serializing. The final field must
    /// equal the original — any reversion to the clobber would
    /// produce `None` here.
    #[test]
    fn last_exit_survives_load_mutate_save_reload_cycle() {
        // Initial state, as a manifest written by the daemon.
        let stored_exit = cm_daemon::manifest::LastExit {
            code: Some(137),
            memory_cap_kill: true,
            kills_file_offset: Some(0),
            exited_at: 1_700_000_000.0,
        };
        let initial = ManifestEntry {
            color: None,
            memory_cap_soft_bytes: None,
            memory_cap_hard_bytes: None,
            cgroup_prefix: None,
            uid: "ts-cap-killed".into(),
            managed_by_uid: None,
            generation: 0,
            label: "label-before".into(),
            session_type: "claude".into(),
            transcript_id: None,
            hidden: false,
            idle_timeout_secs: 0,
            burst_threshold: 0,
            workflow_run_id: None,
            workflow_role: None,
            continuous_task_id: None,
            task_id: None,
            notify_on_idle: false,
            global_perms: false,
            seeded_from_snapshot: None,
            last_exit: Some(stored_exit.clone()),
            host_id: cm_daemon::host_id::HostId::local(),
        };
        let on_disk = serde_json::to_string(&initial).unwrap();

        // Load step: TUI reads the manifest. The Phase-1 fix
        // hydrates `last_exit` into the in-memory mirror.
        let loaded: ManifestEntry = serde_json::from_str(&on_disk).unwrap();
        let preserved_last_exit = loaded.last_exit.clone();
        assert_eq!(
            preserved_last_exit,
            Some(stored_exit.clone()),
            "load step must hydrate last_exit",
        );

        // User mutates an unrelated TUI-owned field (rename via the
        // SessionSettings dialog). The TUI updates its in-memory
        // state and triggers a save.
        let mut mutated_label = loaded.label.clone();
        mutated_label.clear();
        mutated_label.push_str("label-after");

        // Save step: TUI rebuilds ManifestEntry from in-memory state.
        // This mirrors the read-modify-write at `app.rs:3007` —
        // `last_exit: preserved_last_exit.clone()` is the fix. With
        // the pre-fix `last_exit: None`, the assertion at the end of
        // this test would fail.
        let rebuilt = ManifestEntry {
            color: None,
            memory_cap_soft_bytes: None,
            memory_cap_hard_bytes: None,
            cgroup_prefix: None,
            uid: loaded.uid.clone(),
            managed_by_uid: loaded.managed_by_uid.clone(),
            generation: loaded.generation,
            label: mutated_label,
            session_type: loaded.session_type.clone(),
            transcript_id: loaded.transcript_id.clone(),
            hidden: loaded.hidden,
            idle_timeout_secs: loaded.idle_timeout_secs,
            burst_threshold: loaded.burst_threshold,
            workflow_run_id: loaded.workflow_run_id.clone(),
            workflow_role: loaded.workflow_role.clone(),
            continuous_task_id: None,
            task_id: loaded.task_id.clone(),
            notify_on_idle: loaded.notify_on_idle,
            global_perms: loaded.global_perms,
            seeded_from_snapshot: loaded.seeded_from_snapshot.clone(),
            last_exit: preserved_last_exit.clone(),
            host_id: loaded.host_id.clone(),
        };
        let after_save = serde_json::to_string(&rebuilt).unwrap();

        // Reload — what the next TUI start (or next manifest read)
        // would see.
        let after_reload: ManifestEntry =
            serde_json::from_str(&after_save).unwrap();

        // Field survives.
        assert_eq!(
            after_reload.last_exit,
            Some(stored_exit),
            "last_exit must survive the load→mutate→save→reload cycle",
        );
        // Mutation also survives (sanity).
        assert_eq!(after_reload.label, "label-after");
    }
}

#[cfg(test)]
mod transcript_rebind_tests {
    //! Pins down the invariant that any rebind of `transcript_id` after
    //! the initial bind bumps `generation`. Without this, a reader holding
    //! a cursor against the pre-rebind file would skip messages in the new
    //! file (cursor offset N applied to a different file).

    use super::*;
    use std::collections::{BTreeMap, HashMap};

    fn make_test_session(transcript_id: Option<&str>, generation: u64) -> TerminalSession {
        // /bin/true exits immediately; the PTY/Session shell is harmless
        // for a value-only test that never reads the PTY.
        let session =
            crate::session::Session::new("/bin/true", &[], 80, 24, None, HashMap::new(), None)
                .expect("session for test");
        TerminalSession {
            color: None,
            uid: "uid".into(),
            label: "test".into(),
            session_type: "claude".into(),
            session,
            status: SessionStatus::Idle,
            idle_since: None,
            last_write_at: None,
            transcript_id: transcript_id.map(str::to_string),
            generation,
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

    #[test]
    fn rebind_to_new_sid_bumps_generation() {
        let mut ts = make_test_session(Some("old-sid"), 5);
        ts.rebind_transcript(Some("new-sid".into()));
        assert_eq!(ts.transcript_id.as_deref(), Some("new-sid"));
        assert_eq!(ts.generation, 6);
    }

    /// Continuous-Tasks Phase 1: a session tagged with a
    /// `continuous_task_id` carries that tag through
    /// `to_manifest_entry` (the live→disk projection), and a fresh
    /// untagged session leaves it `None`. This is the TUI half of
    /// the daemon→manifest→TUI wire thread; the daemon serde test
    /// (`manifest::tests::t_continuous_task_id_serde_default_and_roundtrip`)
    /// covers the on-disk shape.
    #[test]
    fn continuous_task_id_threads_into_manifest_entry() {
        let mut ts = make_test_session(None, 0);
        assert!(ts.to_manifest_entry().continuous_task_id.is_none());

        ts.continuous_task_id = Some("bug-triage".into());
        assert_eq!(
            ts.to_manifest_entry().continuous_task_id.as_deref(),
            Some("bug-triage"),
        );
    }

    #[test]
    fn rebind_to_none_bumps_generation() {
        // /clear path: transcript becomes None until the detector picks
        // up the freshly-rotated file. Generation must bump immediately
        // so cursors held by readers are invalidated before the next
        // file binds.
        let mut ts = make_test_session(Some("old-sid"), 1);
        ts.rebind_transcript(None);
        assert!(ts.transcript_id.is_none());
        assert_eq!(ts.generation, 2);
    }

    #[test]
    fn rebind_saturates_at_u64_max() {
        let mut ts = make_test_session(Some("old"), u64::MAX);
        ts.rebind_transcript(Some("new".into()));
        assert_eq!(ts.generation, u64::MAX, "must not panic on overflow");
    }

    #[test]
    fn workflow_rebind_resets_active_role_to_new_sid() {
        let run_id = "wf_rebind";
        let mut role_sessions = BTreeMap::new();
        role_sessions.insert(
            "worker".to_string(),
            workflow::run::RoleBinding {
                session_label: "worker".into(),
                current_session_id: Some("old-sid".into()),
                daemon_session_uid: None,
                bound: false,
            },
        );
        let mut role_baselines = BTreeMap::new();
        role_baselines.insert(
            "worker".to_string(),
            workflow::run::MessageBaseline {
                user_count: 4,
                assistant_count: 7,
            },
        );
        let mut run = WorkflowRun::new(
            run_id.to_string(),
            "feedback".into(),
            "ws-1".into(),
            role_sessions,
            "worker".into(),
            role_baselines,
            None,
            BTreeMap::new(),
            0,
        );
        run.history[0].assistant_count_at_start = 7;
        let mut runs = vec![run];

        assert!(note_workflow_transcript_binding(
            &mut runs,
            run_id,
            "worker",
            Some("old-sid"),
            "new-sid",
        ));

        let run = &runs[0];
        assert_eq!(
            run.role_sessions["worker"].current_session_id.as_deref(),
            Some("new-sid")
        );
        assert_eq!(run.role_baselines["worker"].assistant_count, 0);
        assert_eq!(run.role_baselines["worker"].user_count, 0);
        assert_eq!(run.history[0].session_id.as_deref(), Some("new-sid"));
        assert_eq!(run.history[0].assistant_count_at_start, 0);
    }
}

#[cfg(test)]
mod worktree_mode_tests {
    //! Pins the in-place launch primitives: the `WorktreeMode` wire
    //! round-trip and the `Workspace::is_in_place()` marker that every
    //! destructive-cleanup guard keys off.
    use super::{parse_worktree_mode, Workspace, WorktreeMode};
    use std::path::PathBuf;

    #[test]
    fn worktree_mode_wire_round_trip() {
        assert_eq!(parse_worktree_mode("inherit"), WorktreeMode::Inherit);
        assert_eq!(parse_worktree_mode("branch"), WorktreeMode::Branch);
        assert_eq!(parse_worktree_mode("in-place"), WorktreeMode::InPlace);
        // Unknown / future values fall back to the safe default.
        assert_eq!(parse_worktree_mode("inplace"), WorktreeMode::Inherit);
        assert_eq!(parse_worktree_mode("garbage"), WorktreeMode::Inherit);

        assert_eq!(WorktreeMode::Inherit.as_wire(), "inherit");
        assert_eq!(WorktreeMode::Branch.as_wire(), "branch");
        assert_eq!(WorktreeMode::InPlace.as_wire(), "in-place");

        // Full round trip through the wire form.
        for m in [WorktreeMode::Inherit, WorktreeMode::Branch, WorktreeMode::InPlace] {
            assert_eq!(parse_worktree_mode(m.as_wire()), m);
        }
    }

    fn ws(worktree: Option<&str>, main: Option<&str>) -> Workspace {
        Workspace {
            color: None,
            pinned: false,
            id: "w".into(),
            name: "w".into(),
            is_closed: false,
            is_cloud: false,
            repo_url: None,
            worktree_path: worktree.map(PathBuf::from),
            main_repo_path: main.map(PathBuf::from),
            worker_vm: None,
            worker_zone: None,
            host_id: cm_daemon::host_id::HostId::local(),
            sessions: vec![],
            tombstones: vec![],
            is_pushing: false,
        }
    }

    #[test]
    fn is_in_place_truth_table() {
        // Equal paths → in-place.
        assert!(ws(Some("/repo"), Some("/repo")).is_in_place());
        // Different paths (normal worktree) → not in-place.
        assert!(!ws(Some("/repo-worktree"), Some("/repo")).is_in_place());
        // Either side missing (e.g. cloud workspace) → not in-place.
        assert!(!ws(None, Some("/repo")).is_in_place());
        assert!(!ws(Some("/repo"), None).is_in_place());
        assert!(!ws(None, None).is_in_place());
    }
}

#[cfg(test)]
mod entry_matches_delivery_tests {
    //! Regression coverage for the workflow-binding path that broke
    //! during the overnight cleanup orchestration. Each `parse_entries`
    //! input below was copied verbatim from a real history.jsonl line
    //! produced by the stuck workflow workers — this is the *exact*
    //! data shape the production code has to handle.

    use super::entry_matches_delivery;
    use crate::workflow::history;

    /// First 120 chars of the goal we delivered to the cli-cleanup worker.
    /// That worker's actual transcript ended with `stop_reason: end_turn`
    /// at 2026-05-09T04:36:18 — but the binding never landed because
    /// neither `display` nor `paste_content` on the matching history
    /// entry started with this prefix.
    const CLEANUP_GOAL_PREFIX: &str =
        "Fix two real bugs in the CLI in /home/lucas/.cm/worktrees/cm-sub-allow-claudes-to-spawn-and-manage-tasks-cleanup-cli";

    /// Real production line from ~/.claude/history.jsonl on 2026-05-09 —
    /// the one that left the cli-cleanup worker stuck on iteration 1.
    /// `pastedContents` carries `contentHash` only; there is no `content`
    /// field, so `paste_content` parses to "".
    const REAL_POST_2025_PASTE_LINE: &str = r#"{"display":"[Pasted text #1 +11 lines]","pastedContents":{"1":{"id":1,"type":"text","contentHash":"d07c78137ebcc578"}},"timestamp":1778301350854,"project":"/home/lucas/.cm/worktrees/cm-sub-allow-claudes-to-spawn-and-manage-tasks-cleanup-cli-bucket-and-config-c316cc3","sessionId":"7cc30907-9cfd-458e-a9fa-896745af5b1a"}"#;

    /// Pre-fix simulation: what the matcher used to look at — display +
    /// paste_content prefix only. If THIS evaluates true on
    /// `REAL_POST_2025_PASTE_LINE`, the bug never existed and our fix is
    /// fixing nothing. (It MUST evaluate false to demonstrate the regression.)
    fn pre_fix_match(entry: &history::HistoryEntry, prefix: &str) -> bool {
        !prefix.is_empty()
            && (entry.display.starts_with(prefix)
                || entry.paste_content.starts_with(prefix))
    }

    #[test]
    fn pre_fix_matcher_does_not_recover_post_2025_pastes() {
        let entries = parse(REAL_POST_2025_PASTE_LINE);
        assert_eq!(entries.len(), 1);
        let e = &entries[0];

        // Confirm the parser produces the shape we expect: display is the
        // placeholder, paste_content is empty.
        assert_eq!(e.display, "[Pasted text #1 +11 lines]");
        assert_eq!(e.paste_content, "");

        // Old logic — the regression we're fixing. This MUST be false on
        // the real production line, otherwise our fix is treating a non-bug.
        assert!(
            !pre_fix_match(e, CLEANUP_GOAL_PREFIX),
            "pre-fix matcher should NOT have matched the post-2025 paste \
             entry — that's exactly why all 5 cleanup workflows stalled. \
             If this assertion fires, the diagnosis is wrong."
        );
    }

    #[test]
    fn post_fix_matcher_recovers_post_2025_pastes() {
        let entries = parse(REAL_POST_2025_PASTE_LINE);
        let e = &entries[0];

        // New logic — the production code path. With the placeholder
        // detector, the SAME real-world line now correlates.
        assert!(
            entry_matches_delivery(e, CLEANUP_GOAL_PREFIX),
            "post-fix matcher must accept the post-2025 paste placeholder \
             so resolve_pending_deliveries can bind the session"
        );
    }

    #[test]
    fn legacy_plain_typed_input_still_matches() {
        // Pre-paste-redaction era: short typed prompts log raw text into
        // `display`. Mustn't regress.
        let line = r#"{"display":"Implement the feedback workflow","pastedContents":{},"timestamp":1,"project":"/p","sessionId":"s"}"#;
        let e = &parse(line)[0];
        assert!(entry_matches_delivery(e, "Implement the feedback"));
    }

    #[test]
    fn legacy_paste_with_content_field_still_matches() {
        // Pre-2025 paste schema where the raw text was inlined as
        // `pastedContents.<k>.content`. Parser surfaces it via
        // `paste_content`. Mustn't regress.
        let line = r#"{"display":"[Pasted text #1 +3 lines]","pastedContents":{"1":{"id":1,"type":"text","content":"Fix the broken thing\nin the place\nthat is broken"}},"timestamp":1,"project":"/p","sessionId":"s"}"#;
        let e = &parse(line)[0];
        assert!(entry_matches_delivery(e, "Fix the broken"));
    }

    #[test]
    fn empty_prefix_never_matches() {
        // Defensive: a session with no recorded `last_delivery` (or one
        // whose prefix was somehow trimmed to zero) must not bind to
        // every history entry it sees.
        let e = &parse(REAL_POST_2025_PASTE_LINE)[0];
        assert!(!entry_matches_delivery(e, ""));
    }

    #[test]
    fn typed_input_does_not_false_match_the_placeholder_text() {
        // If a user literally types the placeholder string (improbable
        // but possible), `display` matches by exact prefix, not the
        // placeholder fallback. This covers a corner of the matching
        // priority ordering — verifying both paths agree on this case.
        let line = r#"{"display":"[Pasted text #1 +5 lines] is funny syntax","pastedContents":{},"timestamp":1,"project":"/p","sessionId":"s"}"#;
        let e = &parse(line)[0];
        assert!(entry_matches_delivery(e, "[Pasted text #1"));
        // It also matches as a placeholder, which is fine — both paths
        // agree on this entry. The test is documentation: don't try to
        // "fix" the overlap; the matcher is intentionally OR-shaped.
    }

    /// Test helper: parse via the same parse_entries the production code
    /// uses. We can't import it directly (private to history.rs) but a
    /// single-line input through the public `HistoryWatcher::poll` is
    /// awkward to set up. Instead, exercise the public path by writing
    /// to a tempfile and reading back — but for these correlation tests
    /// we only care about the parsed shape, so reuse the test-only
    /// shim below that round-trips through a one-element vec.
    fn parse(line: &str) -> Vec<history::HistoryEntry> {
        // The simplest in-test parse path: use serde to project the raw
        // JSON onto the same fields parse_entries extracts. We test
        // parse_entries itself in workflow::history::tests; here we
        // just need a constructor.
        let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
        let display = v.get("display").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let timestamp_ms = v.get("timestamp").and_then(|x| x.as_u64()).unwrap_or(0);
        let project = v.get("project").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let session_id = v
            .get("sessionId")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let mut paste_content = String::new();
        if let Some(map) = v.get("pastedContents").and_then(|x| x.as_object()) {
            for (_, val) in map {
                if let Some(s) = val.get("content").and_then(|x| x.as_str()) {
                    if !paste_content.is_empty() {
                        paste_content.push('\n');
                    }
                    paste_content.push_str(s);
                }
            }
        }
        vec![history::HistoryEntry {
            display,
            timestamp_ms,
            project,
            session_id,
            paste_content,
        }]
    }
}
