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
///
/// Serde since DESIGN_SEAMLESS_RESTART phase 4c (R11): tombstones ride
/// the persisted `daemon-sessions.json` (`Manifest::recently_exited`)
/// so kill provenance and final-report facts can survive a daemon
/// swap instead of dying with the old image (the P4b known gap).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
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
    /// `true` when this exit followed an explicit kill request (the
    /// `kill_session` RPC, or a daemon-internal kill that goes through
    /// the same handler) rather than the agent finishing on its own.
    ///
    /// Recorded so consumers never present a killed session's last
    /// transcript fragment as if it were the agent's final report — see
    /// `mcp_server/async_monitor.py::_format_fire_message`.
    pub killed: bool,
    /// Who asked for the kill: the CALLER session's uid when an agent
    /// called `kill_session`, `"operator"` on the operator route (the
    /// TUI's A-w, `resolve_stuck`, the scheduler's watchdog). `None`
    /// when `killed` is false, or when the kill came in through a path
    /// that didn't record provenance (a bare `session.kill()` outside
    /// the `kill_session` handler) — `killed` can still be true there,
    /// derived from the exit probe's operator-kill flag.
    pub killed_by: Option<String>,
    /// When the agent called `report_done` before this exit (unix
    /// seconds), or `None` when it never did.
    ///
    /// UX item 4a: "exited" alone doesn't say whether the agent finished
    /// its work or the process just went away. A session that reported
    /// done and THEN exited left a real conclusion behind; one that
    /// exited without reporting may have been cut off mid-task. The
    /// monitor fire message says which (see
    /// `mcp_server/async_monitor.py::_entry_lines`).
    pub reported_done_at: Option<f64>,
    /// The `report_done` reason carried onto the tombstone, so a
    /// read-after-exit caller still sees the agent's own summary.
    pub report_reason: Option<String>,
}

/// Bound on [`DaemonState::kill_requests`]. A request is normally
/// consumed within milliseconds (the reaper's `handle_session_exit`
/// takes it while building the tombstone), so anything still resident
/// past this bound is a leak — a kill whose session never reached the
/// reaper callback.
pub const KILL_REQUEST_CAP: usize = 256;

/// Provenance of an in-flight kill: WHO asked for a session to die,
/// stamped at `kill_session` time. The exit tombstone is built later
/// (asynchronously, on the reaper's `on_exit` callback), by which point
/// the requester is long gone from the call stack — this ledger carries
/// the attribution across that gap.
#[derive(Clone, Debug)]
pub struct KillRequest {
    /// Caller session uid, or `"operator"` for the operator route.
    pub killed_by: String,
    /// Unix seconds when the kill was requested. Used only for the
    /// bounded-eviction order.
    pub requested_at: f64,
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
    /// H3 (restart hardening): drain mode. While true, spawn-shaped
    /// operations (`mcp_start_session`, `session.revive`) refuse and
    /// the continuous scheduler's tick no-ops, so an operator can bring
    /// the daemon to a quiet seam before a restart instead of killing
    /// sessions mid-flight. Set/cleared by `daemon.drain`; surfaced on
    /// `daemon.health`. Deliberately NOT persisted — drain precedes a
    /// restart, and a fresh daemon must never come up refusing spawns.
    ///
    /// Also set (and restored on abort, to the pre-`begin` value) by
    /// the restart coordinator's quiescence barrier — see `restarting`
    /// below and [`crate::restart_coordinator`].
    pub draining: bool,
    /// DESIGN_SEAMLESS_RESTART phase 2d (R2, R10): a restart attempt is
    /// in flight. Doubles as the single-restart latch —
    /// `restart_coordinator::begin` refuses (`restart_in_progress`)
    /// while it is set, and the returned `QuiesceGuard` clears it on
    /// abort/Drop. Surfaced on `daemon.health` beside `draining`.
    /// Deliberately NOT persisted, same rationale as `draining`: a
    /// fresh (or re-exec'd) daemon must never come up mid-"restart".
    pub restarting: bool,
    /// DESIGN_SEAMLESS_RESTART phase 2d (R2, R10): the restart
    /// coordinator — in-flight mutation counter + safe-point subsystem
    /// registry. `Arc` so `dispatch_request` can take a mutation guard
    /// under a brief state-lock hold and carry it for the method body's
    /// duration WITHOUT the lock, and so a quiesce waiter can block on
    /// the counter while mutations take the state lock to finish. See
    /// [`crate::restart_coordinator`] for the barrier semantics.
    pub restart_coordinator: Arc<crate::restart_coordinator::RestartCoordinator>,
    /// Recently-exited sessions, retained for read-after-exit (the MCP
    /// `read_session_output` / `list_sessions(include_exited)` contract). A
    /// session is moved here from `sessions` on exit (see
    /// `handle_session_exit`) and evicted oldest-first past
    /// [`RECENTLY_EXITED_CAP`]. Front = oldest. Recorded via
    /// [`DaemonState::record_exited`].
    pub recently_exited: VecDeque<ExitedTombstone>,
    /// Pending kill attributions, keyed by target session uid. Written
    /// by the `kill_session` handler under the state lock BEFORE the
    /// SIGKILL's exit can be observed; consumed by `handle_session_exit`
    /// (which holds the same lock) when it builds the
    /// [`ExitedTombstone`]. Bounded by [`KILL_REQUEST_CAP`]. See
    /// [`DaemonState::record_kill_request`].
    pub kill_requests: HashMap<String, KillRequest>,
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
    /// Destination for the daemon's OWN durable session registry
    /// (P0 session durability, S1) — a file distinct from the TUI's
    /// `tui-sessions.json` so daemon-spawned sessions survive a
    /// `systemctl restart cm-daemon`. `Some` in production (set by
    /// [`crate::run`] to [`default_daemon_sessions_path`]); `None`
    /// in tests so unit tests never touch the real `~/.cm/` — a
    /// focused test sets it to a tempdir to exercise the lifecycle
    /// hooks. When `None`, [`Self::persist_sessions_best_effort`] is
    /// a no-op. See DESIGN_SESSION_DURABILITY.md.
    pub daemon_sessions_path: Option<PathBuf>,
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
    /// Daemon-authoritative `child_task → parent_task` edges for
    /// tasks MINTED BY AN AGENT: `propose_task` from a tasked
    /// Session caller and `create_subtask` (fix-start-session).
    ///
    /// Why `task_tree` alone isn't enough: `task_tree` is a
    /// replace-not-merge snapshot the TUI pushes from its
    /// planning-derived view, and that view either doesn't know
    /// the fresh task yet (the push races the TUI's next planning
    /// poll) or records it WITHOUT the edge forever:
    ///   * `propose_task` rows are deliberately TOP-LEVEL in
    ///     planning (`parent_task_id = null`) — the backlog is the
    ///     user's triage queue — yet the creating agent must be
    ///     able to `start_session(task_id=<proposed>)` and then
    ///     drive/kill the worker it spawned. Pre-fix every such
    ///     spawn was rejected `unauthorized: task '<id>' is not
    ///     the caller's task or a descendant` — the "different
    ///     phantom task each attempt" failure, since each retry
    ///     proposed a fresh task id.
    ///   * `create_subtask`'s parent-deleted self-heal creates the
    ///     planning row top-level (a dangling FK would be
    ///     rejected), which put the subtask outside its creator's
    ///     scope the moment the row was minted.
    ///
    /// So agent-minted edges live here as an overlay that TUI
    /// pushes never clear: `task_update_tree` re-applies surviving
    /// entries after each snapshot replace, and retires an entry
    /// as soon as the pushed snapshot itself parents the task
    /// (planning caught up, or the user re-parented it — either
    /// way the planning-derived edge wins from then on).
    ///
    /// Auth-scope note: this widens the creator's descendant scope
    /// to cover tasks it minted — deliberate, per
    /// AGENT_ORCHESTRATION.md the descendant check is an
    /// accidental-misuse guardrail ("keep an honest agent from
    /// drifting into unrelated work"), and work the agent itself
    /// created is its own work. Persisted in the daemon manifest
    /// (`daemon-sessions.json`) so the creator keeps scope over
    /// its spawned workers across a daemon restart.
    pub agent_task_edges: HashMap<String, String>,
    /// Startup MCP preflight result: `Ok(summary)` or `Err(diagnosis)`
    /// (fix-loud-preflight). `None` only before startup has run it.
    ///
    /// Retained so the health of this daemon's spawns is a QUESTION YOU CAN
    /// ASK over the socket rather than a line you have to find in a log. A
    /// failed preflight means every session this daemon spawns gets a dead
    /// MCP server and a dead cm Stop hook — it runs, but never reports a
    /// turn-end, so it never appears ready. Not persisted: it describes this
    /// process's environment and is recomputed at every startup.
    pub mcp_preflight: Option<Result<String, String>>,
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
    /// DESIGN_SEAMLESS_RESTART phase 3b: raw fd NUMBER of the bound
    /// control-socket listener, stamped by `run()` right after
    /// bind/adopt. **Manifest input only**: `crate::reexec` copies
    /// the number into the FD manifest so the new image inherits the
    /// bound socket (no connection-refused window). The listener is
    /// never closed, dup'd, or otherwise operated on through this
    /// field — the accept loop owns it. `None` only in states not
    /// built by `run()` (tests), where a re-exec attempt fails
    /// cleanly before any point of no return.
    pub listener_raw_fd: Option<std::os::fd::RawFd>,
    /// DESIGN_SEAMLESS_RESTART phase 3b: `CM_REEXEC=1` was in the
    /// daemon's env at startup — the dev flag that makes the
    /// `daemon.reexec_dev` skeleton trigger dispatchable at all.
    /// Checked ONCE in `run()` and stored here so the dispatch gate
    /// keys off an immutable startup fact rather than re-reading env
    /// (which a later `setenv` anywhere in-process could flip).
    /// `false` = the method answers exactly like any unknown method.
    pub reexec_enabled: bool,
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
    /// Workspace this TUI-minted session lives in (5d). Pushed so the
    /// daemon's unified `list_sessions` can report the same
    /// `workspace_id` grouping key for TUI-owned rows that it already
    /// reports for daemon-owned ones — and so it can join the
    /// workspace's `worktree_path` out of `state.workspaces` (which the
    /// TUI's `task.update_tree` push already populates).
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// Checkout the session runs in, carried directly on the row (5d).
    /// Belt-and-braces next to the `workspace_id` join: a workspace the
    /// TUI hasn't pushed to `state.workspaces` yet (or one GC'd out of
    /// it) would otherwise leave the row pathless, and "which sessions
    /// share my checkout?" is exactly what agents use this for.
    #[serde(default)]
    pub worktree_path: Option<String>,
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
            kill_requests: HashMap::new(),
            workspaces: HashMap::new(),
            bindings: HashMap::new(),
            daemon_sessions_path: None,
            tickets: TicketAllocator::new(),
            attach_addr: String::new(),
            task_tree: HashMap::new(),
            task_workspaces: HashMap::new(),
            task_tree_pushed: false,
            agent_task_edges: HashMap::new(),
            mcp_preflight: None,
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
            draining: false,
            restarting: false,
            restart_coordinator: Arc::new(
                crate::restart_coordinator::RestartCoordinator::new(),
            ),
            listener_raw_fd: None,
            reexec_enabled: false,
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

    /// Stamp who requested a session's kill, so the tombstone built later
    /// (on the reaper's exit callback) can attribute the exit. A repeat
    /// request for the same uid overwrites — the most recent requester is
    /// the one whose SIGKILL is in flight.
    ///
    /// Bounded: past [`KILL_REQUEST_CAP`] the oldest half is dropped by
    /// `requested_at`. Entries are normally removed by
    /// [`Self::take_kill_request`] milliseconds later, so eviction only
    /// ever touches leaked rows (a kill whose exit was never observed).
    pub fn record_kill_request(&mut self, uid: &str, killed_by: &str, requested_at: f64) {
        if self.kill_requests.len() >= KILL_REQUEST_CAP
            && !self.kill_requests.contains_key(uid)
        {
            let mut by_age: Vec<(String, f64)> = self
                .kill_requests
                .iter()
                .map(|(u, r)| (u.clone(), r.requested_at))
                .collect();
            // Newest first, then drop everything past the halfway mark.
            by_age.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            for (stale, _) in by_age.into_iter().skip(KILL_REQUEST_CAP / 2) {
                self.kill_requests.remove(&stale);
            }
        }
        self.kill_requests.insert(
            uid.to_string(),
            KillRequest {
                killed_by: killed_by.to_string(),
                requested_at,
            },
        );
    }

    /// Consume a pending kill attribution for `uid` (see
    /// [`Self::record_kill_request`]). Removing rather than reading keeps
    /// the ledger from accumulating rows for sessions that already exited.
    pub fn take_kill_request(&mut self, uid: &str) -> Option<KillRequest> {
        self.kill_requests.remove(uid)
    }

    /// Look up a recently-exited session's tombstone by uid.
    pub fn exited_tombstone(&self, uid: &str) -> Option<&ExitedTombstone> {
        self.recently_exited
            .iter()
            .rev()
            .find(|t| t.session_uid == uid)
    }

    /// Record an agent-minted task edge (see [`Self::agent_task_edges`]):
    /// the overlay entry plus the immediate `task_tree` edge, so a
    /// `start_session(task_id=<child>)` issued right after the mint
    /// passes the descendant walk without waiting for any push.
    pub fn record_agent_task_edge(&mut self, child_task_id: &str, parent_task_id: &str) {
        self.agent_task_edges
            .insert(child_task_id.to_string(), parent_task_id.to_string());
        self.task_tree
            .insert(child_task_id.to_string(), Some(parent_task_id.to_string()));
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
        // Agent-minted creator edges (fix-start-session). Empty for the
        // TUI-written `tui-sessions.json`; carries entries when this loads
        // a daemon-written manifest. Seed `task_tree` too so the walk
        // works before any TUI push arrives.
        for (child, parent) in manifest.agent_task_edges {
            self.task_tree.insert(child.clone(), Some(parent.clone()));
            self.agent_task_edges.insert(child, parent);
        }
        // Headless binding rehydration. The task→workspace `bindings` map is the
        // daemon's restart-survivable resolver for `mcp_start_session(task_id=…)`
        // — the re-spawn-into-an-EXISTING-subtask-worktree path. It is normally
        // populated by `create_subtask`, but the persisted manifest can carry an
        // empty/stale `bindings` (observed `{}` in the wild), and a HEADLESS
        // daemon has no TUI `task.update_tree` push to repopulate `task_workspaces`
        // either — so after a restart EVERY restored subtask's binding is lost and
        // `start_session(task_id=<subtask>)` fails NotFound "no bound workspace".
        // Each restored session already carries its task_id + workspace_id, so the
        // binding is fully reconstructable here. Merge (`or_insert`) so an explicit
        // persisted binding always wins over a derived one. Collect first to avoid
        // borrowing `self.workspaces` while mutating `self.bindings`.
        let derived: Vec<(String, String)> = self
            .workspaces
            .iter()
            .flat_map(|(ws_id, ws)| {
                ws.sessions
                    .iter()
                    .filter_map(move |s| s.task_id.as_ref().map(|tid| (tid.clone(), ws_id.clone())))
            })
            .collect();
        for (task_id, ws_id) in derived {
            self.bindings.entry(task_id).or_insert(ws_id);
        }
        Ok(true)
    }

    /// Project the LIVE daemon-owned session registry into a
    /// [`Manifest`] suitable for persisting to `daemon-sessions.json`
    /// (P0 session durability, S1).
    ///
    /// **`self.sessions` is the source of truth**, NOT
    /// `self.workspaces[].sessions`. The latter is only ever
    /// populated when a TUI-written `tui-sessions.json` seeded it at
    /// load — the production spawn path inserts into `self.sessions`
    /// and never mirrors into the workspace entry. So we rebuild each
    /// workspace's `sessions` vec from scratch: one entry per live
    /// `DaemonSession`, filed under its `workspace_id`, with the
    /// workspace's metadata (crucially `worktree_path`, the restore
    /// cwd) cloned from `self.workspaces` when known.
    ///
    /// Only workspaces that actually own a live session are emitted —
    /// restore needs no others, and a headless daemon's `workspaces`
    /// map is exactly the set auto-registered at spawn anyway. All
    /// `bindings` are kept (small, and symmetric with
    /// [`Self::load_manifest_from_disk`]).
    pub fn build_daemon_manifest(&self) -> Manifest {
        let mut workspaces: HashMap<String, ManifestWorkspace> = HashMap::new();
        for sess in self.sessions.values() {
            let ws = workspaces
                .entry(sess.workspace_id.clone())
                .or_insert_with(|| match self.workspaces.get(&sess.workspace_id) {
                    // Clone the known metadata but start the sessions
                    // vec empty — we fill it from the live registry.
                    Some(w) => ManifestWorkspace {
                        sessions: Vec::new(),
                        tombstones: Vec::new(),
                        ..w.clone()
                    },
                    // No metadata snapshot (shouldn't happen — spawn
                    // auto-registers — but stay defensive): a minimal
                    // entry keyed by id, no worktree path. Restore will
                    // fall back to a fresh spawn for such a session.
                    None => ManifestWorkspace {
                        id: sess.workspace_id.clone(),
                        ..Default::default()
                    },
                });
            ws.sessions.push(sess.to_manifest_entry());
        }
        Manifest {
            workspaces,
            bindings: self.bindings.clone(),
            // Kept whole like `bindings` (small, and restart-durable auth
            // matters: the creator must keep scope over workers it spawned
            // on agent-minted tasks after a daemon restart).
            agent_task_edges: self.agent_task_edges.clone(),
            view: None,
            hide_continuous: false,
            // TUI-only view state; the daemon-owned registry doesn't track it.
            continuous_column_on: false,
            task_colors: HashMap::new(),
        }
    }

    /// Project [`Self::recently_exited`] into the daemon-owned
    /// tombstone sidecar shape (DESIGN_SEAMLESS_RESTART phase 4c,
    /// R11). A SEPARATE file from `daemon-sessions.json` — the
    /// manifest type is shared with the TUI crate (which constructs
    /// it literally and owns its own persistence), so tombstones ride
    /// a purely daemon-owned sidecar instead of widening the shared
    /// shape. Pre-4c tombstones weren't persisted at all, so every
    /// pre-swap tombstone — kill provenance included — died with the
    /// old image (the P4b known gap).
    pub fn build_tombstone_file(&self) -> TombstoneFile {
        TombstoneFile {
            version: TOMBSTONE_FILE_VERSION,
            recently_exited: self.recently_exited.iter().cloned().collect(),
        }
    }

    /// Checked, durable persist of the tombstone sidecar — the
    /// [`Self::save_daemon_sessions_checked`] twin for
    /// `daemon-tombstones.json` (phase 4c). Same fsync discipline:
    /// temp file synced before the rename, containing directory
    /// synced after it.
    pub fn save_daemon_tombstones_checked(
        &self,
        path: &Path,
    ) -> std::io::Result<()> {
        let file = self.build_tombstone_file();
        let json = serde_json::to_string_pretty(&file).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, e)
        })?;
        write_json_atomic(path, &json, true)
    }

    /// Atomically write the daemon's durable session registry to
    /// `path` (P0 session durability, S1). Builds the manifest from
    /// the live registry, serializes it, and rename-swaps it into
    /// place so a reader (or a crash) never sees a half-written file.
    pub fn save_daemon_sessions(&self, path: &Path) -> std::io::Result<()> {
        let manifest = self.build_daemon_manifest();
        write_manifest_atomic(path, &manifest)
    }

    /// The CHECKED, DURABLE twin of [`Self::save_daemon_sessions`] for
    /// the re-exec swap's persistence pass (DESIGN_SEAMLESS_RESTART
    /// phase 4c, R15). Same snapshot, but the temp file is `fsync`ed
    /// before the rename and the containing directory is `fsync`ed
    /// after it — the rename-swap idiom keeps READERS from seeing a
    /// torn file, but without the dir fsync the rename itself isn't
    /// durable, and the swap must rebuild from exactly the snapshot
    /// that was frozen. Errors are the caller's to act on (the restart
    /// aborts); the best-effort lifecycle variant keeps its
    /// log-and-swallow behavior for normal operation.
    pub fn save_daemon_sessions_checked(&self, path: &Path) -> std::io::Result<()> {
        let manifest = self.build_daemon_manifest();
        write_manifest_atomic_impl(path, &manifest, true)
    }

    /// Best-effort persist to [`Self::daemon_sessions_path`]. A no-op
    /// when the path is unset (tests, or a daemon configured without
    /// durability). Called from the session lifecycle hooks
    /// (spawn / exit / transcript-bind). A write failure is logged
    /// and swallowed — losing one snapshot is recoverable (the next
    /// lifecycle event rewrites the full state), and a persist error
    /// must never fail the RPC that triggered it.
    pub fn persist_sessions_best_effort(&self) {
        if let Some(path) = &self.daemon_sessions_path {
            if let Err(e) = self.save_daemon_sessions(path) {
                eprintln!(
                    "cm-daemon: failed to persist daemon sessions to {}: {}",
                    path.display(),
                    e,
                );
            }
        }
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

/// Default path for the daemon's OWN durable session registry (P0
/// session durability, S1). DELIBERATELY distinct from
/// [`default_manifest_path`]: the TUI owns `tui-sessions.json`, the
/// daemon owns `daemon-sessions.json`, so the two writers never
/// contend (decision 1 in DESIGN_SESSION_DURABILITY.md). The TUI
/// learns about daemon-owned sessions via `list_sessions` /
/// `manifest.watch`, not by reading this file.
pub fn default_daemon_sessions_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".cm/daemon-sessions.json")
}

/// On-disk shape of the daemon-owned tombstone sidecar,
/// `~/.cm/daemon-tombstones.json` (DESIGN_SEAMLESS_RESTART phase 4c,
/// R11). Written by the re-exec swap's checked persistence pass
/// ([`DaemonState::save_daemon_tombstones_checked`]) so
/// recently-exited tombstones — kill attribution, final-report facts
/// — survive the swap instead of dying with the old image (the P4b
/// known gap). A sidecar rather than a `Manifest` field because the
/// manifest shape is shared with the TUI crate; see
/// [`DaemonState::build_tombstone_file`]. The read-side rebuild into
/// the new image's `recently_exited` is the rehydrate-rebuild slice's
/// consumer; [`read_daemon_tombstones`] is the loader it (and the
/// `--verify-handoff` preflight) uses.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TombstoneFile {
    /// Shape version — [`TOMBSTONE_FILE_VERSION`].
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub recently_exited: Vec<ExitedTombstone>,
}

/// Current [`TombstoneFile`] shape version.
pub const TOMBSTONE_FILE_VERSION: u32 = 1;

/// Default path of the tombstone sidecar — beside
/// [`default_daemon_sessions_path`]'s file, daemon-owned like it.
pub fn default_daemon_tombstones_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".cm/daemon-tombstones.json")
}

/// Read (parse, do NOT apply) the tombstone sidecar. `Ok(None)` when
/// the file is absent (nothing was ever persisted — fine); parse
/// failures bubble up (the `--verify-handoff` preflight treats a
/// present-but-unparseable sidecar as a refusal, same rule as every
/// other state file).
pub fn read_daemon_tombstones(
    path: &Path,
) -> std::io::Result<Option<TombstoneFile>> {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let file: TombstoneFile = serde_json::from_str(&contents)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(Some(file))
}

/// Read (parse, do NOT apply) the daemon's durable session registry from
/// `path` (P0 session durability, S2 — restore). Distinct from
/// [`DaemonState::load_manifest_from_disk`], which mutates `self.workspaces`
/// / `self.bindings`: restore must NOT overwrite live state with the daemon
/// file — it iterates the returned manifest and re-spawns each session
/// through `start_session`, which re-registers each workspace itself.
///
/// `Ok(None)` when the file is absent (clean boot — nothing to restore).
/// Parse failures bubble up so the caller can log + continue with no restored
/// sessions (never fatal — restore must never block startup).
pub fn read_daemon_sessions(path: &Path) -> std::io::Result<Option<Manifest>> {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let manifest: Manifest = serde_json::from_str(&contents)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(Some(manifest))
}

/// Serialize `manifest` and write it to `path` atomically: write to
/// a uniquely-named temp file in the same directory, then `rename`
/// over the target (atomic on the same filesystem). A reader never
/// observes a partial file, and a crash mid-write leaves the prior
/// good file intact.
///
/// The temp name is made unique per write (pid + a process-global
/// counter) so two concurrent writers — e.g. the spawn hook on the
/// RPC thread and the exit hook on the reaper thread — never clobber
/// each other's partial temp file. Each writes a complete snapshot,
/// so whichever `rename` lands last wins with a fully-valid manifest.
fn write_manifest_atomic(path: &Path, manifest: &Manifest) -> std::io::Result<()> {
    write_manifest_atomic_impl(path, manifest, false)
}

/// Shared body for the atomic manifest writers. `durable: false` is
/// the historical best-effort shape (no fsync anywhere — cheap, called
/// from lifecycle hot paths where losing one snapshot is recoverable);
/// `durable: true` is the phase-4c checked-persist shape: the temp
/// file's bytes are `fsync`ed before the rename and the containing
/// directory is `fsync`ed after it, so the snapshot is on stable
/// storage before the re-exec swap's point of no return.
fn write_manifest_atomic_impl(
    path: &Path,
    manifest: &Manifest,
    durable: bool,
) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(manifest)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    write_json_atomic(path, &json, durable)
}

/// Rename-swap a serialized JSON document into place. The temp name
/// is unique per write (pid + a process-global counter) so concurrent
/// writers never clobber each other's partial temp file. With
/// `durable`, the temp file is `fsync`ed before the rename and the
/// containing directory after it (the rename-swap idiom needs the dir
/// fsync for durability — phase 4c).
fn write_json_atomic(path: &Path, json: &str, durable: bool) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp.{}.{}", std::process::id(), n));
    if durable {
        let mut f = std::fs::File::create(&tmp)?;
        std::io::Write::write_all(&mut f, json.as_bytes())?;
        f.sync_all()?;
    } else {
        std::fs::write(&tmp, json.as_bytes())?;
    }
    std::fs::rename(&tmp, path)?;
    if durable {
        if let Some(parent) = path.parent() {
            // Directory fsync makes the rename itself durable (the
            // rename-swap idiom's missing half).
            std::fs::File::open(parent)?.sync_all()?;
        }
    }
    Ok(())
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
            killed: false,
            killed_by: None,
            reported_done_at: None,
            report_reason: None,
        }
    }

    /// The kill ledger carries "who asked" from the `kill_session` handler
    /// to the reaper callback that builds the tombstone, and `take_` is a
    /// consume (a second read finds nothing).
    #[test]
    fn kill_request_round_trips_and_is_consumed_once() {
        let mut s = DaemonState::default();
        s.record_kill_request("ts-a", "ts-caller", 100.0);
        let taken = s.take_kill_request("ts-a").expect("attribution recorded");
        assert_eq!(taken.killed_by, "ts-caller");
        assert_eq!(taken.requested_at, 100.0);
        assert!(s.take_kill_request("ts-a").is_none(), "consumed once");
        // Most recent requester wins for a repeat request.
        s.record_kill_request("ts-b", "ts-first", 1.0);
        s.record_kill_request("ts-b", "operator", 2.0);
        assert_eq!(s.take_kill_request("ts-b").unwrap().killed_by, "operator");
    }

    /// Leaked rows (a kill whose exit was never observed) can't grow the
    /// ledger without bound; the newest entry always survives the prune.
    #[test]
    fn kill_requests_are_bounded_and_keep_the_newest() {
        let mut s = DaemonState::default();
        for i in 0..(KILL_REQUEST_CAP + 10) {
            s.record_kill_request(&format!("ts-{i:04}"), "operator", i as f64);
        }
        assert!(
            s.kill_requests.len() <= KILL_REQUEST_CAP,
            "ledger bounded, got {}",
            s.kill_requests.len()
        );
        assert!(
            s.take_kill_request(&format!("ts-{:04}", KILL_REQUEST_CAP + 9)).is_some(),
            "newest request retained",
        );
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
            color: None,
            memory_cap_soft_bytes: None,
            memory_cap_hard_bytes: None,
            cgroup_prefix: None,
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

    // ---- P0 session durability (S1): persist ------------------------

    /// Spawn a real (sleep) daemon session, then project the registry
    /// into a manifest and assert it carries every re-spawn input: the
    /// engine, the resume key (`transcript_id` = stem of
    /// `transcript_path`), the task/workspace/managed-by identity, the
    /// continuous tag, the perms grant, and the workspace's worktree
    /// path (the restore cwd). `state.sessions` — NOT
    /// `workspaces[].sessions` — must be the source of truth.
    #[test]
    fn build_daemon_manifest_projects_live_sessions_with_respawn_fields() {
        use crate::session::{DaemonSession, SpawnParams};
        let mut state = DaemonState::default();
        let wt = std::env::temp_dir().join("cm-s1-build-wt");
        let _ = std::fs::create_dir_all(&wt);
        state.workspaces.insert(
            "ws-1".to_string(),
            ManifestWorkspace {
                id: "ws-1".to_string(),
                worktree_path: Some(wt.clone()),
                // A stale session here MUST be ignored — the live
                // registry is authoritative.
                sessions: vec![entry("ts-stale-should-be-dropped")],
                ..Default::default()
            },
        );
        let mut sp = SpawnParams::new("ts-aaaa-bbbb", "bug-002", "/bin/sleep");
        sp.args = vec!["120".to_string()];
        sp.workspace_id = "ws-1".to_string();
        sp.session_type = "codex".to_string();
        sp.task_id = Some("task-xyz".to_string());
        sp.managed_by_uid = Some("ts-parent".to_string());
        sp.continuous_task_id = Some("ct-1".to_string());
        sp.global_perms = true;
        let mut sess = DaemonSession::spawn(sp).expect("spawn");
        sess.transcript_path = Some(
            "/home/u/.claude/projects/enc/11111111-2222-3333-4444-555555555555.jsonl"
                .to_string(),
        );
        sess.generation = 3;
        state.sessions.insert("ts-aaaa-bbbb".to_string(), sess);

        let m = state.build_daemon_manifest();
        let ws = m.workspaces.get("ws-1").expect("workspace persisted");
        assert_eq!(
            ws.worktree_path.as_deref(),
            Some(wt.as_path()),
            "worktree carried — it's the restore cwd",
        );
        assert_eq!(
            ws.sessions.len(),
            1,
            "exactly the live session, not the stale workspace entry",
        );
        let e = &ws.sessions[0];
        assert_eq!(e.uid, "ts-aaaa-bbbb");
        assert_eq!(e.session_type, "codex", "engine carried");
        assert_eq!(e.label, "bug-002");
        assert_eq!(e.task_id.as_deref(), Some("task-xyz"));
        assert_eq!(e.managed_by_uid.as_deref(), Some("ts-parent"));
        assert_eq!(e.continuous_task_id.as_deref(), Some("ct-1"));
        assert!(e.global_perms, "perms grant carried");
        assert_eq!(e.generation, 3);
        assert_eq!(
            e.transcript_id.as_deref(),
            Some("11111111-2222-3333-4444-555555555555"),
            "transcript_id = file stem of transcript_path (the --resume key)",
        );
        // The DaemonSession's Drop SIGKILLs the sleep child on scope exit.
    }

    /// End-to-end: configure a `daemon_sessions_path`, spawn a session,
    /// `persist_sessions_best_effort()`, then load the written file with
    /// a fresh state's `load_manifest_from_disk` and confirm the session
    /// + its worktree survived the round-trip. This is the S1 contract:
    /// a restart reading the file sees the session it must restore.
    #[test]
    fn persist_round_trips_through_load_manifest_from_disk() {
        use crate::session::{DaemonSession, SpawnParams};
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("daemon-sessions.json");
        let wt = std::env::temp_dir().join("cm-s1-roundtrip-wt");
        let _ = std::fs::create_dir_all(&wt);

        let mut state = DaemonState::default();
        state.daemon_sessions_path = Some(path.clone());
        state.workspaces.insert(
            "ws-rt".to_string(),
            ManifestWorkspace {
                id: "ws-rt".to_string(),
                worktree_path: Some(wt.clone()),
                ..Default::default()
            },
        );
        state.bindings.insert("task-rt".to_string(), "ws-rt".to_string());
        // Agent-minted creator edge (fix-start-session) must round-trip
        // too, so creator scope survives a daemon restart.
        state.record_agent_task_edge("task-proposed-rt", "task-rt");
        let mut sp = SpawnParams::new("ts-cccc-dddd", "rt", "/bin/sleep");
        sp.args = vec!["120".to_string()];
        sp.workspace_id = "ws-rt".to_string();
        sp.task_id = Some("task-rt".to_string());
        let sess = DaemonSession::spawn(sp).expect("spawn");
        state.sessions.insert("ts-cccc-dddd".to_string(), sess);

        state.persist_sessions_best_effort();
        assert!(path.exists(), "persist wrote the durable file");

        // A fresh state — as if the daemon just restarted — reads it.
        let mut restored = DaemonState::default();
        let found = restored
            .load_manifest_from_disk(&path)
            .expect("load ok");
        assert!(found, "file present and parsed");
        let ws = restored.workspaces.get("ws-rt").expect("workspace restored");
        assert_eq!(ws.worktree_path.as_deref(), Some(wt.as_path()));
        assert_eq!(ws.sessions.len(), 1);
        assert_eq!(ws.sessions[0].uid, "ts-cccc-dddd");
        assert_eq!(ws.sessions[0].task_id.as_deref(), Some("task-rt"));
        assert_eq!(
            restored.bindings.get("task-rt").map(String::as_str),
            Some("ws-rt"),
            "task→workspace binding survived",
        );
        assert_eq!(
            restored.agent_task_edges.get("task-proposed-rt").map(String::as_str),
            Some("task-rt"),
            "agent-minted creator edge survived",
        );
        assert_eq!(
            restored.task_tree.get("task-proposed-rt"),
            Some(&Some("task-rt".to_string())),
            "restored edge is immediately visible to the descendant walk",
        );
    }

    /// DESIGN_SEAMLESS_RESTART phase 4c (R11): recently-exited
    /// tombstones ride the daemon-owned sidecar. The checked
    /// (fsynced) writer lands them on disk and the CURRENT loader
    /// (`read_daemon_tombstones`) reads them back with kill
    /// provenance and report facts intact. Pre-4c tombstones were
    /// not persisted anywhere, so every pre-swap tombstone died with
    /// the old image (the P4b known gap).
    #[test]
    fn checked_persist_carries_tombstones_and_reparses() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("daemon-tombstones.json");
        let mut state = DaemonState::default();
        let mut t = tomb("ts-4c-dead");
        t.killed = true;
        t.killed_by = Some("operator".to_string());
        t.reported_done_at = Some(1_700_000_000.0);
        t.report_reason = Some("finished".to_string());
        state.record_exited(t);

        state
            .save_daemon_tombstones_checked(&path)
            .expect("checked tombstone persist");
        let f = read_daemon_tombstones(&path)
            .expect("reparse with the current loader")
            .expect("file present");
        assert_eq!(f.version, TOMBSTONE_FILE_VERSION);
        assert_eq!(f.recently_exited.len(), 1);
        let back = &f.recently_exited[0];
        assert_eq!(back.session_uid, "ts-4c-dead");
        assert!(back.killed, "kill provenance survived");
        assert_eq!(back.killed_by.as_deref(), Some("operator"));
        assert_eq!(back.reported_done_at, Some(1_700_000_000.0));
        assert_eq!(back.report_reason.as_deref(), Some("finished"));

        // Absent file: fine (nothing ever persisted).
        assert!(read_daemon_tombstones(&dir.path().join("nope.json"))
            .expect("absent is Ok(None)")
            .is_none());
        // Present-but-corrupt: an error, not a skip.
        std::fs::write(&path, "{ not json").expect("corrupt file");
        assert!(read_daemon_tombstones(&path).is_err());
    }

    /// With no `daemon_sessions_path` set (the test/disabled default),
    /// the persist hook is a strict no-op — it must never write to the
    /// real `~/.cm/`. Guards the cfg-free test-isolation contract.
    #[test]
    fn persist_is_noop_when_path_unset() {
        let state = DaemonState::default();
        assert!(state.daemon_sessions_path.is_none());
        // Must not panic, must not write anywhere.
        state.persist_sessions_best_effort();
        // An empty registry projects to an empty manifest.
        let m = state.build_daemon_manifest();
        assert!(m.workspaces.is_empty());
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
    fn load_rehydrates_bindings_from_restored_sessions() {
        // Headless restart with an empty/stale persisted `bindings`: the
        // task→workspace binding must be rebuilt from each restored session's
        // task_id + workspace_id so `start_session(task_id=<subtask>)` resolves
        // after a daemon restart with no TUI `task.update_tree` push.
        let _g = lock();
        let dir = TempDir::new().expect("tempdir");
        let manifest_json = serde_json::json!({
            "workspaces": {
                "ws-sub": {
                    "id": "ws-sub",
                    "name": "subtask",
                    "is_closed": false,
                    "is_cloud": false,
                    "sessions": [{
                        "uid": "ts-sub", "generation": 0, "label": "bug-agent",
                        "session_type": "claude", "task_id": "task-sub",
                        "hidden": false, "idle_timeout_secs": 0,
                        "burst_threshold": 0, "notify_on_idle": false
                    }],
                    "tombstones": []
                },
                "ws-explicit": {
                    "id": "ws-explicit",
                    "name": "explicit",
                    "is_closed": false,
                    "is_cloud": false,
                    "sessions": [{
                        "uid": "ts-exp", "generation": 0, "label": "exp",
                        "session_type": "claude", "task_id": "task-pinned",
                        "hidden": false, "idle_timeout_secs": 0,
                        "burst_threshold": 0, "notify_on_idle": false
                    }],
                    "tombstones": []
                }
            },
            // Empty for task-sub (the wild-observed case); an explicit pin for
            // task-pinned that the session-derived value must NOT overwrite.
            "bindings": { "task-pinned": "ws-pinned-explicit" }
        })
        .to_string();
        let path = write_manifest(&dir, &manifest_json);

        let mut state = DaemonState::new();
        assert!(state.load_manifest_from_disk(&path).expect("load ok"));
        // Derived from the restored session — was absent in persisted bindings.
        assert_eq!(
            state.bindings.get("task-sub").map(String::as_str),
            Some("ws-sub"),
            "binding rebuilt from the restored subtask session",
        );
        // Explicit persisted binding wins over the derivable one (or_insert).
        assert_eq!(
            state.bindings.get("task-pinned").map(String::as_str),
            Some("ws-pinned-explicit"),
            "explicit persisted binding not overwritten by session-derived value",
        );
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
