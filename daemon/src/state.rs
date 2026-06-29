//! Daemon-side mutable state. Slice 10a of doc/persistent-host-daemon.md
//! (see daemon/NOTES.md for the full slicing plan).
//!
//! ## What lives here
//!
//! `DaemonState` is the daemon's analog of the TUI's `App` struct
//! today — the shared mutable state every JSON-RPC method handler
//! needs access to. Lives behind `Arc<Mutex<...>>` in `run()` so
//! per-connection threads in the accept loop serialize their
//! mutations.
//!
//! ## What's here NOW (10a-shell + 10a-types)
//!
//! - [`DaemonState::sessions`] — `HashMap<uid, DaemonSession>` for
//!   the daemon-owned PTY/fanout side of the Session split (slice 7
//!   primitive). Unused until 10c wires session-spawn to the daemon.
//! - [`DaemonState::workspaces`] — workspace map keyed by stable id,
//!   loaded read-only from `~/.cm/tui-sessions.json` via
//!   [`DaemonState::load_manifest_from_disk`]. Until slice 10e flips
//!   ownership, the TUI remains the sole writer; the daemon's copy
//!   is a snapshot taken at startup. Methods (when 10b lands) read
//!   from this snapshot for `list_sessions` / `resolve_authorized_session`
//!   / etc.
//! - [`DaemonState::bindings`] — `task_id → workspace_id` map,
//!   also from the manifest. Same read-only-until-10e disposition.
//!
//! Everything else (manifest persister, attach-ticket allocator
//! handle, workflow controller state) joins this struct as later
//! sub-slices land.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

use crate::attach::TicketAllocator;
use crate::manifest::{Manifest, ManifestWorkspace};
use crate::session::DaemonSession;

/// Max number of recently-exited sessions retained for read-after-exit (see
/// [`DaemonState::recently_exited`]). A small bound: this backs the MCP
/// `read_session_output` / `list_sessions(include_exited)` "read a session's
/// final output after it exits" contract, which callers use within seconds of
/// an exit — old tombstones have no readers. The transcript FILE is untouched
/// by eviction (it lives on disk); evicting a tombstone only drops the daemon's
/// in-memory `uid → (transcript_path, final state)` lookup.
pub const RECENTLY_EXITED_CAP: usize = 256;

/// A session that has exited and been removed from [`DaemonState::sessions`],
/// retained briefly so `resolve_authorized_session` / `list_sessions` can still
/// answer "where's its transcript + what was its final state" for read-after-
/// exit. Holds only the fields those two methods need — including `task_id` /
/// `workspace_id` so the descendant-scope auth check still applies to a dead
/// target (see `auth::check_session_caller_for_exited`).
#[derive(Clone, Debug)]
pub struct ExitedTombstone {
    pub session_uid: String,
    pub transcript_path: Option<String>,
    pub generation: u64,
    pub session_type: String,
    pub workspace_id: String,
    pub task_id: Option<String>,
    pub managed_by_uid: Option<String>,
    pub label: String,
    pub workflow_run_id: Option<String>,
    pub workflow_role: Option<String>,
    pub worktree_path: Option<String>,
    /// Global-perms grant the session carried while live. Kept on
    /// the tombstone so the enriched `list_sessions(include_exited)`
    /// output reports the same `global_perms` field for exited
    /// sessions as for live ones (auth itself keys off the live
    /// caller, never the tombstone).
    pub global_perms: bool,
    pub exited_at: f64,
}

/// Sub-2b-3 review-5 #1: per-worktree FIFO sequence queue.
///
/// Replaces the review-4 raw mutex so `mcp_start_session` can
/// return immediately (within the Python MCP
/// `control_client.call()` 30s default timeout) while
/// detection still serializes against prior in-flight
/// detectors in the same worktree — necessary for transcript
/// ownership correctness (review-3 dedup + review-4 serial
/// argument).
///
/// Shape: each spawn into a worktree mints a monotonic
/// sequence number; the detector waits until all prior seqs
/// have signaled completion before polling, then signals its
/// own completion on the way out.
///
/// **Spawns in DIFFERENT worktrees use DIFFERENT queue Arcs
/// and don't serialize against each other**.
///
/// ## Sub-2b-3 review-6 #2a: strict in-order advance
///
/// Pre-fix the cursor advance was `completed_seq.max(my_seq +
/// 1)` — any seq calling `signal_done` could jump the cursor
/// ahead of in-flight earlier seqs. Combined with bash spawns
/// (whose `signal_done` fired immediately, before any prior
/// detector had bound), the queue could let detector C
/// proceed while detector A was still polling. That defeats
/// the ownership-serialization guarantee that round-4 +
/// round-5 set up.
///
/// New shape:
///   - `done_count` tracks the exclusive upper bound of the
///     contiguous completed prefix (seqs in `[0, done_count)`
///     are done).
///   - `pending_done` buffers out-of-order completions until
///     `done_count` catches up.
///   - `signal_done(my_seq)` advances `done_count` ONLY when
///     `my_seq == done_count`; otherwise it inserts `my_seq`
///     into `pending_done`. After each advance, drain
///     `pending_done` of any seqs that are now next-in-line.
///
/// With this, no out-of-order `signal_done` call can let a
/// later seq's detector slip past an earlier in-flight one.
pub struct WorktreeSpawnQueue {
    state: Mutex<WorktreeSpawnQueueState>,
    cond: Condvar,
}

struct WorktreeSpawnQueueState {
    /// Sequence number of the next spawn to enqueue.
    next_seq: u64,
    /// Exclusive upper bound of the contiguous completed
    /// prefix. Sequence numbers in `[0, done_count)` have all
    /// signaled completion AND every prefix gap has been
    /// filled. A detector with `my_seq` starts polling when
    /// `done_count >= my_seq` (all prior seqs done).
    done_count: u64,
    /// Out-of-order completions: seqs >= `done_count` that
    /// have signaled done but are waiting for the cursor to
    /// catch up. Drained into `done_count` whenever the next
    /// in-line completes (sub-2b-3 review-6 #2a). In our
    /// current design with bash skipping the queue entirely
    /// (review-6 #2b), only the detector-thread `Drop` path
    /// can populate this set — and detectors are themselves
    /// FIFO via `wait_for_turn`, so the set typically stays
    /// empty. Keeping it as a defensive shape makes the queue
    /// correct against future non-detector signalers.
    pending_done: BTreeSet<u64>,
}

impl WorktreeSpawnQueue {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(WorktreeSpawnQueueState {
                next_seq: 0,
                done_count: 0,
                pending_done: BTreeSet::new(),
            }),
            cond: Condvar::new(),
        }
    }

    /// Atomically mint a fresh sequence number. Used by
    /// `mcp_start_session` main thread; cheap, non-blocking.
    pub fn enqueue(&self) -> u64 {
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let seq = s.next_seq;
        s.next_seq += 1;
        seq
    }

    /// Block until all sequence numbers strictly less than
    /// `my_seq` have signaled completion. Called by the
    /// detector thread before polling so its dedup check
    /// against `state.sessions.values().transcript_path`
    /// observes every prior detector's binding.
    pub fn wait_for_turn(&self, my_seq: u64) {
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        while s.done_count < my_seq {
            s = self.cond.wait(s).unwrap_or_else(|p| p.into_inner());
        }
    }

    /// Sub-2b-3 review-9: bounded variant of
    /// [`wait_for_turn`]. Returns `Ok(())` when the turn is
    /// acquired; returns `Err(())` if the wait elapsed
    /// without `done_count` reaching `my_seq`.
    ///
    /// `mcp_start_session` uses this with a 20s bound — well
    /// below the Python `control_client.call()` 30s default
    /// timeout. Pre-fix the wait was unbounded; a slow
    /// in-flight detector (up to the 60s detector
    /// `MAX_DURATION`) could leave the client timeout
    /// firing while the daemon was still inside
    /// `mcp_start_session` — the daemon would later resume,
    /// spawn the child, and create an orphan session the
    /// client believed had failed.
    ///
    /// Callers that hit the timeout MUST drop their ticket
    /// (signal_done fires via RAII) and surface a transient
    /// error to the agent. The agent's retry will succeed
    /// once the prior detector completes.
    pub fn wait_for_turn_timeout(
        &self,
        my_seq: u64,
        timeout: std::time::Duration,
    ) -> Result<(), ()> {
        let deadline = std::time::Instant::now() + timeout;
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        while s.done_count < my_seq {
            let now = std::time::Instant::now();
            if now >= deadline {
                return Err(());
            }
            let remaining = deadline - now;
            let (next_s, timeout_result) = self
                .cond
                .wait_timeout(s, remaining)
                .unwrap_or_else(|p| p.into_inner());
            s = next_s;
            if timeout_result.timed_out() && s.done_count < my_seq {
                return Err(());
            }
        }
        Ok(())
    }

    /// Mark `my_seq` as completed (sub-2b-3 review-6 #2a:
    /// strict in-order advance). If `my_seq` is next-in-line
    /// (`my_seq == done_count`), advance `done_count` and
    /// drain any contiguous buffered completions. Otherwise
    /// buffer it in `pending_done` until the cursor catches
    /// up. A stale signal (`my_seq < done_count`) is
    /// idempotent — already counted.
    ///
    /// Notifies all so any seq blocked in `wait_for_turn`
    /// rechecks. Notification fires even when the signal was
    /// only buffered (cheap; waiters re-check the same
    /// predicate).
    pub fn signal_done(&self, my_seq: u64) {
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if my_seq < s.done_count {
            // Already counted — defensive idempotency.
            return;
        }
        if my_seq == s.done_count {
            s.done_count += 1;
            // Drain any buffered next-in-line completions.
            loop {
                let next = s.done_count;
                if s.pending_done.remove(&next) {
                    s.done_count += 1;
                } else {
                    break;
                }
            }
            self.cond.notify_all();
        } else {
            // Out-of-order completion — buffer until the
            // cursor catches up. With review-6 #2b excluding
            // bash and detectors serializing via
            // `wait_for_turn`, this branch is typically
            // unreachable; kept as a defensive shape so a
            // future non-detector signaler can't break FIFO.
            s.pending_done.insert(my_seq);
        }
    }
}

/// RAII guard owning a queue slot. Drop calls
/// `signal_done` so any early return — error in
/// `mcp_start_session` between enqueue and the detector
/// thread spawn, panic inside the detector, timeout — still
/// releases the slot. Subsequent same-worktree spawns can't
/// block forever (sub-2b-3 review-6 #1).
///
/// The guard is `Send` (its fields are `Arc` + `u64` + `bool`)
/// so it can transfer ownership into the detector thread.
/// In the success path `mcp_start_session` moves the ticket
/// into the spawned thread's closure; the thread drops it
/// after detection completes or times out. In the error
/// path, the guard drops on function return.
pub struct WorktreeSpawnTicket {
    queue: Arc<WorktreeSpawnQueue>,
    seq: u64,
    signaled: bool,
}

impl WorktreeSpawnTicket {
    pub fn new(queue: Arc<WorktreeSpawnQueue>, seq: u64) -> Self {
        Self { queue, seq, signaled: false }
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Read-only handle to the underlying queue Arc. The
    /// detector thread uses this to call `wait_for_turn` and
    /// inspect the queue without consuming the ticket
    /// (consumption would defeat the Drop-on-panic guarantee).
    pub fn queue_arc_clone(&self) -> Arc<WorktreeSpawnQueue> {
        self.queue.clone()
    }
}

impl Drop for WorktreeSpawnTicket {
    fn drop(&mut self) {
        if !self.signaled {
            self.queue.signal_done(self.seq);
            self.signaled = true;
        }
    }
}

pub type WorktreeSpawnQueues = Arc<Mutex<HashMap<PathBuf, Arc<WorktreeSpawnQueue>>>>;

/// Daemon process state. Lives behind `Arc<Mutex<DaemonState>>` in
/// `run()`; per-connection threads lock it for the duration of one
/// JSON-RPC dispatch. Mutex (not RwLock) because method handlers
/// almost always mutate — even read-only-looking calls like
/// `list_sessions` need a consistent snapshot.
pub struct DaemonState {
    /// Daemon-owned per-session state (PTY, fanout, memory cap).
    /// Empty in 10a; populated by 10c when the daemon starts
    /// spawning sessions. Indexed by the stable session uid that
    /// already lives on `ManifestEntry` / `TerminalSession`.
    pub sessions: HashMap<String, DaemonSession>,
    /// Recently-exited sessions, retained for read-after-exit (the MCP
    /// `read_session_output` / `list_sessions(include_exited)` contract). A
    /// session is moved here from `sessions` on exit (see
    /// `handle_session_exit`) and evicted oldest-first past
    /// [`RECENTLY_EXITED_CAP`]. Front = oldest. Recorded via
    /// [`DaemonState::record_exited`].
    pub recently_exited: VecDeque<ExitedTombstone>,
    /// Snapshot of the persisted manifest's workspaces, keyed by
    /// stable workspace id. Loaded at daemon startup via
    /// [`load_manifest_from_disk`](Self::load_manifest_from_disk).
    /// Read-only through slice 10e — the TUI is still the sole
    /// writer of `~/.cm/tui-sessions.json`. The daemon does NOT
    /// re-read the file after startup; consistency with TUI writes
    /// follows from the snapshot being a Phase-1 read-only view.
    pub workspaces: HashMap<String, ManifestWorkspace>,
    /// `task_id → workspace_id` bindings from the same manifest.
    pub bindings: HashMap<String, String>,
    /// Pending attach-ticket store. Slice 5 primitive; slice 10b
    /// wires `session.attach` / `attach.open` to allocate + consume
    /// through this. One allocator per daemon instance — tickets
    /// from one daemon can't be consumed by another, which matches
    /// the "tickets bind to the daemon that issued them" semantics
    /// the design doc specifies.
    pub tickets: TicketAllocator,
    /// Address (typically a socket path) that the TUI dials for a
    /// dedicated attach connection. Returned by `session.attach`
    /// alongside the ticket. Configured at daemon startup from the
    /// socket path the accept loop bound (see `run()`).
    pub attach_addr: String,
    /// Planning task tree snapshot. **Source of truth: TUI.** The
    /// TUI calls `task.update_tree` whenever `App.tasks` mutates
    /// (slice 10d-mcp-surface-2a). Keyed by `task_id`, value is
    /// `Some(parent_task_id)` for child tasks / `None` for top-
    /// level tasks. Reset (not merged) on each update — semantic
    /// shape is "snapshot replace" so callers don't have to reason
    /// about stale ancestors.
    ///
    /// **Why TUI-authoritative for Phase 1**: cheaper to land than
    /// daemon-owns-tasks (which would entangle with the planning
    /// API HTTP layer); the auth check just reads from the cache.
    /// When the workflow controller relocates daemon-side
    /// (10d-workflow-controller), the snapshot becomes redundant
    /// — the controller owns task transitions and can write
    /// directly. Sub-2c can unwind this dependency.
    ///
    /// Used by `crate::control::auth::check_session_caller` for
    /// the descendant-task-tree authorization branch and by
    /// `crate::control::methods::list_sessions`'s `task_id` filter
    /// (slice 10d-mcp-surface-2a no-op → honored).
    pub task_tree: HashMap<String, Option<String>>,
    /// Sub-2b-3 review-2 #1: task → workspace_id mapping
    /// pushed by the TUI alongside `task_tree` via the
    /// `task.update_tree` RPC. Used by `mcp_start_session` to
    /// resolve a descendant task's bound workspace WITHOUT
    /// requiring a live anchor session in that workspace
    /// first. Pre-fix the resolver walked `state.sessions` for
    /// a session tagged with the requested task — which fails
    /// for first-spawn-into-fresh-subtask (the common case
    /// `mcp_start_session` is meant to serve).
    ///
    /// Replace-not-merge on every `task.update_tree` push, in
    /// lockstep with `task_tree`. Missing entries are valid (a
    /// task in `task_tree` with no `task_workspaces` entry is
    /// in backlog — no workspace yet).
    pub task_workspaces: HashMap<String, String>,
    /// Mirrors [`tui_sessions_pushed`] for `task_tree`. `false`
    /// until the TUI sends its first `task.update_tree` snapshot
    /// (even an empty one), `true` thereafter. Lets
    /// `auth::check_session_caller` distinguish "tree was pushed
    /// and has no descendant relationship" (true `OutOfScope`) from
    /// "tree hasn't been pushed yet" (retryable). Without the
    /// distinction, a Session-caller RPC that arrives in the
    /// startup window before the TUI's first push gets
    /// `Unauthorized` for what would normally be a valid
    /// descendant-task call once the snapshot lands.
    pub task_tree_pushed: bool,
    /// Sub-2b-3 review-5 #1: per-worktree FIFO spawn queues.
    /// `Arc`-shared so the spawn-main path can clone the
    /// queue out of the state lock and enqueue without
    /// holding the outer `DaemonState` mutex.
    pub worktree_spawn_queues: WorktreeSpawnQueues,
    /// 10d-1: TUI-pushed session snapshot. The TUI is
    /// authoritative for sessions it spawned locally
    /// (`SpawnTarget::TuiLocal`); the daemon needs to know
    /// about them so 10d-2's workflow-method auth can
    /// recognize TUI-minted callers (today the daemon only
    /// knows about sessions in `self.sessions` — those it
    /// spawned itself).
    ///
    /// **Authoritative source:** TUI. The daemon never writes
    /// to this map except via `tui.update_sessions_snapshot`.
    /// Replace-not-merge on every push, same shape as
    /// `task_tree` / `task_workspaces`.
    ///
    /// **Empty-vs-unset distinction**: `tui_sessions_pushed`
    /// is false until the first push. After that, an empty
    /// `tui_sessions` map is meaningful ("TUI has no
    /// sessions") and consumers must distinguish that from
    /// "TUI hasn't pushed yet, no information available".
    /// The flag flips on first push and never flips back.
    ///
    /// 10d-1 lands the push + storage. The auth consumer in
    /// workflow methods is 10d-2; no callers consume this
    /// field yet at 10d-1.
    pub tui_sessions: HashMap<String, TuiSessionSnapshot>,
    /// 10d-1: see [`tui_sessions`] — `true` once the TUI has
    /// ever pushed a snapshot (even an empty one), `false`
    /// before the first push. Lets the future 10d-2 auth
    /// consumer distinguish "TUI deliberately has no
    /// sessions" from "TUI hasn't pushed yet."
    pub tui_sessions_pushed: bool,
    /// 10d-2a: daemon-side workflow runs keyed by run_id.
    /// **Scaffold only at 10d-2a** — no dispatch arms drive
    /// this map yet. The TUI's controller continues to own
    /// in-flight workflow runs through 10d-2's transitional
    /// sub-slices; 10d-2b adds the `workflow_transition` /
    /// `workflow_done` auth consumer that reads from this map,
    /// 10d-2c moves the state machine driver here, and 10d-2d
    /// adds `start_workflow` / `stop_workflow` /
    /// `get_workflow_state` / `list_workflows` Operator-only
    /// dispatch. The `WorkflowRun` type is the same one TUI
    /// uses today (re-exported from `cm_daemon::workflow::run`),
    /// so on-disk `state.json` round-trips byte-for-byte between
    /// the two — see `daemon::workflow::run` for the wire
    /// shape.
    pub workflow_runs: HashMap<String, crate::workflow::run::WorkflowRun>,

    /// 10d-2c-2-1: TUI-pushed workflow TOML definitions (
    /// `~/.cm/workflows/*.toml`, loaded TUI-side at startup).
    /// Daemon needs them for the upcoming on_idle driver
    /// (2c-2-2): looking up `static_transition_on_idle` for the
    /// active role + reading the target role's
    /// `activation_prompt` template + the role's `engine` /
    /// `context` knobs.
    ///
    /// Replace-not-merge semantics: each call to
    /// `workflow.update_definitions` clears and re-populates.
    /// Same shape as `task_tree` and `tui_sessions`. Operator-
    /// only on the wire — Session-callable would let an agent
    /// rewrite the workflow's transition map, defeating the
    /// daemon's static-idle gate.
    pub workflow_definitions: HashMap<String, crate::workflow::toml_schema::Workflow>,

    /// Phase 4 (doc/daemon-side-workflow-orchestration.md §B2): the BASE layer
    /// of workflow definitions, loaded from the daemon's own `workflows_dir`
    /// (`config.workflows_dir`) at startup. `workflow_definitions` above is the
    /// OVERRIDE layer fed by the TUI's `workflow.update_definitions` push.
    /// Lookups check override first, then base (see [`Self::workflow_definition`]),
    /// so a daemon with NO TUI still has definitions to drive headless runs, and
    /// a TUI reconnect (which `clear()`s the override) can never wipe the base.
    pub base_workflow_definitions:
        HashMap<String, crate::workflow::toml_schema::Workflow>,

    /// 10e-a: in-memory broadcaster for `manifest.watch` subscribers.
    /// The reaper's `on_exit` callback (see
    /// `crate::control::methods::start_session`) populates
    /// `state.workspaces[ws_id].sessions[*].last_exit` for a daemon-
    /// spawned session that exits, then `broadcast`s a
    /// `ManifestDiff::Exited` here. The matching `manifest.watch` RPC
    /// handler (10e-b) will dial subscribers from this broadcaster.
    ///
    /// `Arc` so the broadcaster outlives any one subscriber and so
    /// the on_exit callback can read it while holding the state lock
    /// without forcing a clone of the entire `DaemonState`.
    ///
    /// No backing replay buffer: subscribers see live diffs PLUS the
    /// initial snapshot that the RPC handler will compose from a
    /// `state.workspaces` walk. Matches `PtyByteFanout`'s broadcaster
    /// shape minus the ring buffer (manifest diffs are infrequent
    /// enough that a ring buffer would only add lifecycle complexity
    /// without buying replay).
    pub manifest_watcher: Arc<crate::manifest::ManifestWatcher>,
    /// Phase 2 slice 11a: broadcaster for workflow `events.jsonl`
    /// writes. Hooked into `append_event_with_retry`; every
    /// successfully-persisted event fans out to subscribers via
    /// the [11b] `events.subscribe` RPC. Same RAII guard +
    /// `sync_channel` shape as `manifest_watcher`. See
    /// `daemon/src/workflow/events.rs::WorkflowEventWatcher` and
    /// daemon/NOTES.md §"Phase 2: Workflow events over RPC".
    pub workflow_event_watcher: Arc<crate::workflow::events::WorkflowEventWatcher>,
    /// Slice 12f: loaded `daemon.toml`. Read once at startup
    /// (`run()` reads + validates, then stuffs the result
    /// here). Immutable after load — `start_session` reads
    /// `mcp_server_path` / `api_url` / `api_token` / etc.
    /// from this to populate every agent's env. Defaults to
    /// `DaemonConfig::default()` so an in-test `DaemonState::new()`
    /// doesn't touch the filesystem.
    pub config: crate::config::DaemonConfig,
}

/// 10d-1: TUI-side view of a single session. Carried by the
/// `tui.update_sessions_snapshot` wire shape.
///
/// Field set is the minimum the 10d-2 workflow-method auth
/// needs: identity (uid), task binding (for descendant-task
/// scope), and workflow tags (for future workflow-method
/// auth). Type and `hidden` are useful for `list_sessions`
/// parity if/when that method consumes this map (currently
/// list_sessions only inspects `state.sessions`, but a
/// future slice could merge views for opt-in-off mode).
///
/// All optional fields use `#[serde(default)]` so the wire
/// shape can evolve additively. Sub-2a's `task_tree` proved
/// the pattern.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct TuiSessionSnapshot {
    pub uid: String,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default, rename = "type")]
    pub session_type: Option<String>,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub workflow_run_id: Option<String>,
    #[serde(default)]
    pub workflow_role: Option<String>,
    /// Global-permissions grant for this TUI-minted session, pushed
    /// by the TUI's `tui.update_sessions_snapshot`. Lets the
    /// daemon's unified view report the grant for TUI-owned
    /// sessions too. `#[serde(default)]` keeps older pushes (no
    /// field) loading as `false`.
    #[serde(default)]
    pub global_perms: bool,
}

/// 10d-1: unified session view used by future workflow-
/// method auth (10d-2). Tells the consumer whether the
/// session is daemon-minted (in `state.sessions`) or TUI-
/// minted (in `state.tui_sessions`), without changing the
/// answer to "does this session exist and what's its
/// task_id."
#[derive(Clone, Debug)]
pub struct SessionViewAny {
    pub uid: String,
    /// `true` if the lookup found the session in
    /// `state.sessions` (daemon-owned PTY); `false` if it
    /// came from `state.tui_sessions` (TUI-owned).
    pub daemon_owned: bool,
    pub task_id: Option<String>,
    pub workspace_id: Option<String>,
    pub workflow_run_id: Option<String>,
    pub workflow_role: Option<String>,
    /// Global-permissions grant from whichever map the session was
    /// found in.
    pub global_perms: bool,
}

impl Default for DaemonState {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
            recently_exited: VecDeque::new(),
            workspaces: HashMap::new(),
            bindings: HashMap::new(),
            tickets: TicketAllocator::new(),
            attach_addr: String::new(),
            task_tree: HashMap::new(),
            task_workspaces: HashMap::new(),
            task_tree_pushed: false,
            worktree_spawn_queues: Arc::new(Mutex::new(HashMap::new())),
            tui_sessions: HashMap::new(),
            tui_sessions_pushed: false,
            workflow_runs: HashMap::new(),
            workflow_definitions: HashMap::new(),
            base_workflow_definitions: HashMap::new(),
            manifest_watcher: Arc::new(crate::manifest::ManifestWatcher::new()),
            workflow_event_watcher: Arc::new(
                crate::workflow::events::WorkflowEventWatcher::new(),
            ),
            config: crate::config::DaemonConfig::default(),
        }
    }
}

impl DaemonState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a just-exited session as a tombstone for read-after-exit, evicting
    /// the oldest past [`RECENTLY_EXITED_CAP`]. A repeat uid (a slot reused by a
    /// fresh-context respawn that later exits again) drops any prior tombstone
    /// for that uid first, so the newest exit wins and lookups stay unambiguous.
    pub fn record_exited(&mut self, tomb: ExitedTombstone) {
        self.recently_exited.retain(|t| t.session_uid != tomb.session_uid);
        self.recently_exited.push_back(tomb);
        while self.recently_exited.len() > RECENTLY_EXITED_CAP {
            self.recently_exited.pop_front();
        }
    }

    /// Look up a recently-exited session's tombstone by uid.
    pub fn exited_tombstone(&self, uid: &str) -> Option<&ExitedTombstone> {
        self.recently_exited
            .iter()
            .rev()
            .find(|t| t.session_uid == uid)
    }

    /// Phase 4 §B2: resolve a workflow definition by name through the two-layer
    /// model — the TUI-pushed OVERRIDE layer (`workflow_definitions`) first,
    /// then the daemon's own BASE layer loaded from `workflows_dir`
    /// (`base_workflow_definitions`). This is the single lookup the poller,
    /// `start_workflow`, and `get_workflow_state` use, so a locally-edited TOML
    /// overrides the deployed base while the base survives a TUI reconnect.
    pub fn workflow_definition(
        &self,
        name: &str,
    ) -> Option<&crate::workflow::toml_schema::Workflow> {
        self.workflow_definitions
            .get(name)
            .or_else(|| self.base_workflow_definitions.get(name))
    }

    /// Populate `workspaces` and `bindings` from
    /// `~/.cm/tui-sessions.json` (or from the explicit path passed
    /// in for tests). Read-only: this never writes to the file, and
    /// the daemon doesn't re-read it after startup.
    ///
    /// Returns `Ok(true)` when a manifest was found and loaded,
    /// `Ok(false)` when the file is absent (clean-home boot —
    /// daemon starts with empty workspaces, ready for TUI activity
    /// to create them).
    ///
    /// Parse failures bubble up; the calling layer logs and
    /// continues with an empty state (matching the TUI's
    /// behavior — a corrupt manifest gets backed up and replaced
    /// with `Manifest::default()`). This function doesn't do the
    /// corrupt-file backup itself because in 10a the TUI is still
    /// performing that recovery on its load path; duplicating
    /// it on the daemon side would risk two writers racing on the
    /// backup filename.
    pub fn load_manifest_from_disk(
        &mut self,
        manifest_path: &Path,
    ) -> std::io::Result<bool> {
        let contents = match std::fs::read_to_string(manifest_path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(false);
            }
            Err(e) => return Err(e),
        };
        let manifest: Manifest = serde_json::from_str(&contents).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, e)
        })?;
        self.workspaces = manifest.workspaces;
        self.bindings = manifest.bindings;
        Ok(true)
    }

    /// 10d-1: unified session lookup across daemon-owned
    /// (`self.sessions`) and TUI-pushed (`self.tui_sessions`)
    /// maps. Daemon-owned wins on collision — `self.sessions`
    /// is authoritative for the sessions the daemon spawned
    /// itself.
    ///
    /// Returns `None` if neither map knows about `uid`. The
    /// flag `tui_sessions_pushed` is NOT consulted here:
    /// callers that need to distinguish "TUI never pushed"
    /// from "TUI pushed an empty map" should consult the
    /// flag separately. This helper just answers "does any
    /// known map have a row for this uid?"
    ///
    /// No callers yet at 10d-1 — the auth consumer lands in
    /// 10d-2's workflow methods. Shipping the lookup here
    /// keeps the auth wiring in 10d-2 a one-line change.
    pub fn lookup_session_any(&self, uid: &str) -> Option<SessionViewAny> {
        if let Some(daemon_sess) = self.sessions.get(uid) {
            // 10d-2c-1 review round-5 (F1): for daemon-owned
            // sessions, workflow context lives on `DaemonSession`
            // itself (populated at spawn time via
            // `StartSessionParams.workflow_run_id` / `.workflow_role`
            // OR after-the-fact via the
            // `session.set_workflow_context` RPC). DO NOT fall
            // through to `tui_sessions` for workflow fields —
            // round-3's snapshot filter removes daemon-attached
            // sessions from the TUI's pushed map, so the fallthrough
            // would always be `None` AND would set the wrong
            // authoritative source. Daemon-owned is canonical here.
            return Some(SessionViewAny {
                uid: uid.to_string(),
                daemon_owned: true,
                task_id: daemon_sess.task_id.clone(),
                workspace_id: Some(daemon_sess.workspace_id.clone()),
                workflow_run_id: daemon_sess.workflow_run_id.clone(),
                workflow_role: daemon_sess.workflow_role.clone(),
                global_perms: daemon_sess.global_perms,
            });
        }
        if let Some(tui_sess) = self.tui_sessions.get(uid) {
            return Some(SessionViewAny {
                uid: uid.to_string(),
                daemon_owned: false,
                task_id: tui_sess.task_id.clone(),
                workspace_id: None,
                workflow_run_id: tui_sess.workflow_run_id.clone(),
                workflow_role: tui_sess.workflow_role.clone(),
                global_perms: tui_sess.global_perms,
            });
        }
        None
    }
}

/// Default path the daemon loads the manifest from. Matches the
/// TUI's `Self::manifest_path()` — both must point at the same
/// inode for 10a's read-only-snapshot model to be coherent.
pub fn default_manifest_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".cm/tui-sessions.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ManifestEntry, SessionTombstone};
    use tempfile::TempDir;

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_support::env_lock()
    }

    fn tomb(uid: &str) -> ExitedTombstone {
        ExitedTombstone {
            session_uid: uid.to_string(),
            transcript_path: Some(format!("/tmp/{uid}.jsonl")),
            generation: 0,
            session_type: "claude-code".to_string(),
            workspace_id: "ws".to_string(),
            task_id: None,
            managed_by_uid: None,
            label: uid.to_string(),
            workflow_run_id: None,
            workflow_role: None,
            worktree_path: None,
            global_perms: false,
            exited_at: 0.0,
        }
    }

    #[test]
    fn record_exited_evicts_oldest_past_cap_and_dedups_uid() {
        let mut s = DaemonState::default();
        // Fill past the cap: the oldest must be evicted, length capped.
        for i in 0..(RECENTLY_EXITED_CAP + 5) {
            s.record_exited(tomb(&format!("ts-{i:04}")));
        }
        assert_eq!(s.recently_exited.len(), RECENTLY_EXITED_CAP);
        assert!(s.exited_tombstone("ts-0000").is_none(), "oldest evicted");
        assert!(
            s.exited_tombstone(&format!("ts-{:04}", RECENTLY_EXITED_CAP + 4)).is_some(),
            "newest retained",
        );

        // Re-recording a uid drops the prior tombstone (newest exit wins) and
        // doesn't grow the deque with a duplicate.
        let before = s.recently_exited.len();
        let mut t = tomb("ts-0100");
        t.generation = 7;
        s.record_exited(t);
        assert_eq!(s.recently_exited.len(), before, "no duplicate row for same uid");
        assert_eq!(s.exited_tombstone("ts-0100").unwrap().generation, 7, "newest wins");
    }

    fn write_manifest(dir: &TempDir, contents: &str) -> PathBuf {
        let path = dir.path().join("tui-sessions.json");
        std::fs::write(&path, contents).expect("write manifest");
        path
    }

    fn entry(uid: &str) -> ManifestEntry {
        ManifestEntry {
            uid: uid.into(),
            managed_by_uid: None,
            generation: 0,
            label: format!("label-{}", uid),
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
            seeded_from_snapshot: None,
            last_exit: None,
            host_id: crate::host_id::HostId::local(),
            global_perms: false,
        }
    }

    #[test]
    fn default_state_has_no_sessions_or_workspaces() {
        let state = DaemonState::new();
        assert!(state.sessions.is_empty());
        assert!(state.workspaces.is_empty());
        assert!(state.bindings.is_empty());
    }

    #[test]
    fn load_missing_manifest_returns_false_without_error() {
        // Clean-home boot: file doesn't exist. Daemon should accept
        // this as a normal state (empty workspaces, ready for TUI
        // to populate via writes).
        let _g = lock();
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("does-not-exist.json");

        let mut state = DaemonState::new();
        let loaded = state
            .load_manifest_from_disk(&path)
            .expect("missing file is Ok(false)");
        assert!(!loaded);
        assert!(state.workspaces.is_empty());
    }

    #[test]
    fn load_populates_workspaces_and_bindings() {
        let _g = lock();
        let dir = TempDir::new().expect("tempdir");
        let manifest_json = serde_json::json!({
            "workspaces": {
                "ws-1": {
                    "id": "ws-1",
                    "name": "alpha",
                    "is_closed": false,
                    "is_cloud": false,
                    "sessions": [
                        {
                            "uid": "ts-1",
                            "generation": 0,
                            "label": "session-one",
                            "session_type": "claude",
                            "hidden": false,
                            "idle_timeout_secs": 0,
                            "burst_threshold": 0,
                            "notify_on_idle": false
                        }
                    ],
                    "tombstones": []
                },
                "ws-2": {
                    "id": "ws-2",
                    "name": "beta",
                    "is_closed": true,
                    "is_cloud": false,
                    "sessions": [],
                    "tombstones": []
                }
            },
            "bindings": {
                "task-foo": "ws-1"
            }
        })
        .to_string();
        let path = write_manifest(&dir, &manifest_json);

        let mut state = DaemonState::new();
        let loaded = state.load_manifest_from_disk(&path).expect("load ok");
        assert!(loaded);
        assert_eq!(state.workspaces.len(), 2);
        let ws1 = state.workspaces.get("ws-1").expect("ws-1");
        assert_eq!(ws1.name, "alpha");
        assert_eq!(ws1.sessions.len(), 1);
        assert_eq!(ws1.sessions[0].uid, "ts-1");
        let ws2 = state.workspaces.get("ws-2").expect("ws-2");
        assert!(ws2.is_closed);
        assert_eq!(state.bindings.get("task-foo").map(String::as_str), Some("ws-1"));
    }

    #[test]
    fn load_corrupt_manifest_surfaces_invalid_data() {
        // The TUI's load path backs up corrupt manifests and
        // continues; the daemon delegates that recovery to the TUI
        // for now (10a is read-only — duplicating backup logic
        // would risk two writers fighting). Daemon's load just
        // surfaces the parse error so the caller can decide what
        // to do (current `run()` startup would log + continue with
        // empty state when 10b wires it).
        let _g = lock();
        let dir = TempDir::new().expect("tempdir");
        let path = write_manifest(&dir, "not valid json {");

        let mut state = DaemonState::new();
        let err = state
            .load_manifest_from_disk(&path)
            .expect_err("corrupt manifest must surface");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        // State unchanged on error.
        assert!(state.workspaces.is_empty());
    }

    #[test]
    fn load_preserves_last_exit_field_on_entries() {
        // Round-trip check: a daemon-written `last_exit` (slice 9
        // field) on a manifest entry survives the load and lands in
        // `DaemonState.workspaces`. This is what slice 10e's flip
        // depends on — once the daemon owns the writer side, it
        // reads what it just wrote.
        let _g = lock();
        let dir = TempDir::new().expect("tempdir");
        let manifest_json = serde_json::json!({
            "workspaces": {
                "ws-1": {
                    "id": "ws-1",
                    "name": "test",
                    "is_closed": false,
                    "is_cloud": false,
                    "sessions": [
                        {
                            "uid": "ts-cap-killed",
                            "generation": 0,
                            "label": "killed",
                            "session_type": "claude",
                            "hidden": false,
                            "idle_timeout_secs": 0,
                            "burst_threshold": 0,
                            "notify_on_idle": false,
                            "last_exit": {
                                "code": 137,
                                "memory_cap_kill": true,
                                "exited_at": 1700000000.0
                            }
                        }
                    ],
                    "tombstones": []
                }
            },
            "bindings": {}
        })
        .to_string();
        let path = write_manifest(&dir, &manifest_json);

        let mut state = DaemonState::new();
        state.load_manifest_from_disk(&path).expect("load ok");
        let ws = state.workspaces.get("ws-1").expect("ws-1");
        let last_exit = ws.sessions[0]
            .last_exit
            .as_ref()
            .expect("last_exit present");
        assert!(last_exit.memory_cap_kill);
        assert_eq!(last_exit.code, Some(137));
    }

    #[test]
    fn load_passes_through_tombstones() {
        let _g = lock();
        let dir = TempDir::new().expect("tempdir");
        let manifest_json = serde_json::json!({
            "workspaces": {
                "ws-1": {
                    "id": "ws-1",
                    "name": "test",
                    "is_closed": false,
                    "is_cloud": false,
                    "sessions": [],
                    "tombstones": [
                        {
                            "uid": "ts-tomb",
                            "label": "closed",
                            "session_type": "claude",
                            "generation": 0,
                            "exited_at": 1700000000.0
                        }
                    ]
                }
            },
            "bindings": {}
        })
        .to_string();
        let path = write_manifest(&dir, &manifest_json);

        let mut state = DaemonState::new();
        state.load_manifest_from_disk(&path).expect("load ok");
        let tombs: &Vec<SessionTombstone> =
            &state.workspaces.get("ws-1").unwrap().tombstones;
        assert_eq!(tombs.len(), 1);
        assert_eq!(tombs[0].uid, "ts-tomb");
    }

    #[test]
    fn default_manifest_path_uses_home_dot_cm() {
        let _g = lock();
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", "/home/test-user") };
        let path = default_manifest_path();
        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        assert_eq!(
            path,
            PathBuf::from("/home/test-user/.cm/tui-sessions.json"),
        );
    }
}
