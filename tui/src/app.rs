use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alacritty_terminal::event::Event as TermEvent;
use alacritty_terminal::term::TermMode;
use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};
use ratatui::Frame;

use crate::agent;
use crate::agent_memory;
use crate::api::Task;
use crate::backend::{BackendEvent, BackendHandle};
use crate::config::Config;
use crate::planning::{PlanAction, PlanningView, WorkspaceCandidate};
use crate::session::Session;
use crate::terminal_widget::TerminalWidget;
use crate::theme;
use crate::workflow::{self, toml_schema::Engine, Workflow, WorkflowRun};
// These live in `workflow::observer` now (workflow-observation glue split out
// of this file); re-import so the call sites here read unchanged.
use crate::workflow::observer::{
    drop_inactive_runs_from_in_mem, drop_run_from_in_mem, log_tick,
};
use cm_daemon::worktree;

mod model;
use model::*;
mod nav;
use nav::*;
mod workflow_ui;
use workflow_ui::*;
mod persist;
mod remote;
use remote::*;
mod events;
use events::*;
mod lifecycle;
use lifecycle::*;
mod input;
use input::*;
mod draw;

pub(crate) use lifecycle::try_attach_via_daemon_with_deps;

pub(crate) use model::{
    new_workspace_id, parse_worktree_mode, PendingWrite, SessionStatus, TaskEntry, TaskStatus,
    TerminalSession, Workspace, WorktreeMode,
};

/// Concatenated source of the `app` module tree, for the migration guard
/// tests that assert invariants by scanning the source text. Pre-split these
/// scans read the single monolithic app.rs; the corpus now spans the
/// extracted submodules as well.
// NOTE: extracted submodules come FIRST so that real definitions precede
// the guard tests' own string literals (which live in the later files) in
// `find()` scans, mirroring the pre-split file order.
#[cfg(test)]
const APP_SRC_FOR_SCAN: &str = concat!(
    include_str!("app/model.rs"),
    include_str!("app/nav.rs"),
    include_str!("app/workflow_ui.rs"),
    include_str!("app/persist.rs"),
    include_str!("app/draw.rs"),
    include_str!("app.rs"),
    // events/remote/input hold test-mod helpers whose literals would
    // shadow root definitions in find() scans — they stay after app.rs.
    include_str!("app/input.rs"),
    include_str!("app/events.rs"),
    include_str!("app/remote.rs"),
    include_str!("app/lifecycle.rs"),
);

mod dirs {
    use std::path::PathBuf;
    pub fn home_dir() -> Option<PathBuf> {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

/// Width of the Sessions-view sidebar in cells. The terminal panel takes the
/// remaining width minus its own border (see `SIDEBAR_WIDTH + 2` in main.rs
/// when sizing the PTY).
pub const SIDEBAR_WIDTH: u16 = 36;
/// Minimum Wakeups within the window to consider a session actively working.
const WAKEUP_BURST_THRESHOLD: usize = 5;

// ── MRU quick-switch (A-;) ──────────────────────────────────────

// ── Fuzzy-find palette (A-p) ────────────────────────────────────

// ── Task-detail peek (A-i) ──────────────────────────────────────

// 10d-3: `apply_stop_workflow_status` relocated to
// `daemon/src/workflow/run.rs` as the shared canonical mutation
// used by BOTH the TUI A-o flow and the daemon's `stop_workflow`
// handler. Re-exported through `crate::workflow::run` (see
// `tui/src/workflow/mod.rs`'s blanket re-export). The function's
// terminal-state guard semantics are identical — round-9's
// behavior is preserved end-to-end.
pub(crate) use crate::workflow::run::apply_stop_workflow_status;


// `ManifestEntry`, `ManifestWorkspace`, `SessionTombstone`, `Manifest`,
// and `TOMBSTONE_RETENTION_SECS` live in the daemon crate (slice
// 10a-types of doc/persistent-host-daemon.md). Module-level `use`
// brings them into scope so existing bare references inside this
// file (`ManifestEntry { ...     global_perms: false,
// file (`ManifestEntry { ... }`, `SessionTombstone { ... }` etc.)
// resolve unchanged.
//
// Deliberately NOT `pub use` — external modules that need these
// types should import from `cm_daemon::manifest` directly so the
// dependency path is explicit and the eventual slice-10e flip
// (when manifest ownership moves daemon-side) doesn't have to
// chase shim re-exports.
use cm_daemon::manifest::{
    Manifest, ManifestEntry, ManifestWorkspace, SessionTombstone,
    TOMBSTONE_RETENTION_SECS,
};

/// Fire a desktop notification announcing that a session went idle, and play a
/// short sound alongside it. Spawned onto a detached thread so a slow/blocked
/// dbus call (or the ~1s sound playback) can't stall the UI loop. Errors are
/// intentionally swallowed — a missing notification daemon or audio device is
/// not a reason to surface anything to the user.
fn notify_session_idle(label: &str) {
    let label = label.to_string();
    std::thread::spawn(move || {
        let _ = notify_rust::Notification::new()
            .summary("Claude Manager")
            .body(&format!("Session idle: {}", label))
            .show();
        play_notification_sound();
    });
}

/// Fire a desktop notification raised by an agent calling the `notify_user`
/// MCP tool. Same detached-thread, swallow-errors discipline as
/// `notify_session_idle`. `message` is the agent-supplied reason; when empty
/// we fall back to a generic line so the notification still says something
/// useful.
pub(crate) fn notify_user_alert(label: &str, message: &str) {
    let label = label.to_string();
    let body = if message.trim().is_empty() {
        format!("{} needs your attention", label)
    } else {
        format!("{}: {}", label, message)
    };
    std::thread::spawn(move || {
        let _ = notify_rust::Notification::new()
            .summary("Claude Manager")
            .body(&body)
            .show();
        play_notification_sound();
    });
}

/// Best-effort: play a short notification sound. Called from the detached
/// notification thread, so the blocking playback (~1s) never touches the UI
/// loop. We don't rely on the notification daemon honoring sound hints (dunst
/// and others ignore them), so we play the sound ourselves via whichever
/// PipeWire/PulseAudio player is on PATH. Every failure is swallowed.
fn play_notification_sound() {
    use std::process::{Command, Stdio};
    const SOUND: &str = "/usr/share/sounds/freedesktop/stereo/window-attention.oga";
    if !std::path::Path::new(SOUND).exists() {
        return;
    }
    // Try players in turn; `.status()` waits (reaping the child) and returns
    // Err only when the binary is absent, so we fall through to the next.
    for player in ["paplay", "pw-play"] {
        let played = Command::new(player)
            .arg(SOUND)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if played.is_ok() {
            return;
        }
    }
}

// ── Input handler extraction ────────────────────────────────────────
//
// The per-mode arms of `handle_input_event` are implemented as free
// functions (`handle_<mode>`) so each modal can be unit-tested without
// booting an `App`. The functions:
//   - take a `<Mode>Mut<'_>` bag of refs into the `InputMode` variant
//     payload (so they can mutate cursor position, type characters, etc.),
//   - take an `InputCtx<'_>` for the read-only context that's needed
//     across more than one mode (currently just the repo URL list),
//   - return an `InputOutcome` describing the post-condition.
// The dispatcher (`handle_input_event`) translates the outcome into
// app-level state changes (mode swap, side-effect dispatch).

// Per-mode mutable-ref bags. Each handler takes its own bag so the
// dispatcher can split the borrow on `&mut self.input_mode` and pass
// only the variant payload through.

pub struct App {
    pub tasks: Vec<TaskEntry>,
    /// Execution contexts. Sidebar rendering iterates workspaces, not tasks.
    pub workspaces: Vec<Workspace>,
    pub cursor: Cursor,
    /// Which sidebar column the cursor is in (S4). `Main` unless the user has
    /// stepped right (`A-l`) into the continuous column. Forced back to `Main`
    /// whenever the continuous column isn't shown.
    pub cursor_column: SidebarColumn,
    /// The `Main`-column cursor stashed when stepping into the continuous
    /// column, restored on the way back (`A-h`) so a round-trip doesn't lose
    /// the main selection.
    pub saved_main_cursor: Option<Cursor>,
    /// A-; quick-switch: session uids in most-recently-focused-first order
    /// (the CURRENT focus is not in the deque — it's pushed when focus
    /// moves off it). Deduped, capped at [`SESSION_MRU_CAP`]. Fed by the
    /// user-driven nav paths via `note_session_focus_change`; reconcile-
    /// driven cursor shuffles are deliberately NOT recorded (not user
    /// intent). In-memory only.
    pub session_mru: VecDeque<String>,
    /// In-progress A-; walk (frozen ring + position). `None` when no walk
    /// is active; reset by any other key press.
    mru_walk: Option<MruWalk>,
    /// The `Continuous`-column position stashed when stepping back to main,
    /// restored on the way in (`A-l`) so re-entering the column lands on the
    /// row you left off at (not always the first). Stored as the session UID
    /// (NOT a `(ws_idx, sess_idx)` index) so it survives a manifest refresh
    /// reindexing the workspaces/sessions between leaving and returning — the
    /// index-based form silently fell back to the first row after any poll.
    /// Resolved against the live rows on restore — falls back to the first row
    /// if the saved session is gone.
    pub saved_continuous_uid: Option<String>,
    pub sidebar_view: SidebarView,
    pub view_mode: ViewMode,
    pub planning: PlanningView,
    pub should_quit: bool,
    pub last_term_size: (u16, u16),
    /// Throttle for `adopt_untracked_daemon_sessions`; bounds the
    /// `list_sessions` RPC frequency in the main tick.
    pub last_adopt_scan: Option<Instant>,
    pub config: Config,
    pub backend: BackendHandle,
    pub connected: bool,
    pub status_msg: Option<(String, Instant)>,
    pub needs_redraw: bool,
    /// Request a physical screen wipe (`terminal.clear()` → ESC[2J +
    /// reset of ratatui's previous buffer) before the next draw. ratatui's
    /// incremental diff only repaints cells it believes changed, so once
    /// its previous-buffer model desyncs from the physical screen (e.g. a
    /// same-dimension SIGWINCH, or any residual corruption) the artifacts
    /// don't self-heal. The main loop consumes this flag to force a full
    /// repaint. Set on resize and on the `A-r` refresh (the user's manual
    /// "fix my screen" escape hatch).
    pub force_clear: bool,
    input_mode: InputMode,
    start_time: Instant,
    sessions_restored: bool,
    /// Task→workspace bindings loaded from the manifest at startup. Consulted
    /// by reconcile_tasks before auto-provisioning so tasks that were already
    /// bound to a workspace don't spawn orphan duplicates when reconcile runs
    /// before restore_sessions populates self.workspaces.
    manifest_bindings: HashMap<String, String>,
    last_session_id_check: Instant,
    /// Last time `tick_workflows` actually ran. The drain loop calls
    /// it every iteration, but each workflow tick does several transcript
    /// reads per active role — throttling to ~10Hz keeps that work off
    /// the keystroke-to-paint path without delaying transitions noticeably.
    last_workflow_tick: Instant,
    /// Workflow definitions loaded from `workflows/*.toml` at startup.
    pub workflows: HashMap<String, Workflow>,
    /// Files in the workflows directory that failed to parse or validate at
    /// startup. Surfaced in the workflow picker so a typo in a TOML doesn't
    /// silently make a workflow disappear without a hint.
    pub workflow_load_errors: Vec<(PathBuf, String)>,
    /// Active + recent workflow runs (persisted per run at ~/.cm/workflow-runs/).
    pub workflow_runs: Vec<WorkflowRun>,
    /// Tails `~/.claude/history.jsonl` for `/clear` and `/compact` events so
    /// we can detect when a bound workflow session rotates its transcript
    /// file. `None` if the history file couldn't be located at startup.
    history_watcher: Option<workflow::history::HistoryWatcher>,
    /// Rotation-trigger entries we've seen but haven't resolved yet because
    /// the new transcript file hadn't been created when we polled. Retry
    /// each tick until resolved or aged out.
    /// Each: (old_sid, timestamp_ms, first_seen_at).
    pending_rotations: Vec<(String, u64, Instant)>,
    /// Mouse capture state. When false, `DisableMouseCapture` has been sent so
    /// the user can use the terminal's native selection (including block-select
    /// chords). Toggle with Alt+m.
    pub mouse_capture_enabled: bool,
    /// Pending requests from the control socket. Drained each tick by the
    /// main loop and dispatched to method handlers. The server thread
    /// pushes; the main loop pops + replies. See `tui/src/control/`.
    control_queue: crate::control::queue::Queue,
    /// Whether THIS TUI currently owns the control socket (`tui.sock`).
    /// False when another instance held it at bind time. Drives the
    /// degraded-mode banner in the status bar and gates the rebind retry.
    control_bound: bool,
    /// PID of the instance that owns the control socket when we don't
    /// (read from the `<sock>.owner` sidecar; `None` when the holder
    /// predates the sidecar or is gone). Surfaced in the banner so the
    /// user knows exactly which process to kill.
    control_conflict_pid: Option<u32>,
    /// Resolved control-socket path, cached so the rebind retry and the
    /// owner-PID lookup don't re-read the environment each tick.
    control_socket_path: std::path::PathBuf,
    /// Next instant at which `maybe_rebind_control_socket` re-attempts the
    /// bind while degraded. Throttles retries to `CONTROL_REBIND_INTERVAL`.
    control_rebind_at: Instant,
    /// Phase 6 activity feed: ring buffer of agent-initiated mutations
    /// surfaced over the MCP control socket. Read-only methods (list_*,
    /// get_*, ping, read_session_output) are intentionally excluded —
    /// they're high-frequency and uninteresting in a feed. Capped at
    /// `ACTIVITY_LOG_CAP` entries (oldest evicted).
    pub activity_log: VecDeque<ActivityEntry>,
    /// Toggle for the bottom-of-screen activity strip. Off by default;
    /// `Alt-,` flips it.
    pub activity_visible: bool,
    /// Continuous-Tasks Phase 1: when set, all three `visual_items_*`
    /// builders skip the continuous group (its `ContinuousHeader` + the
    /// sessions under it). DEPRECATED + unused — the master-hide concept was
    /// folded into `continuous_column_on` (one toggle: column on = shown in the
    /// column, off = hidden). Kept only so old manifests round-trip without a
    /// schema break; never consulted for rendering.
    pub hide_continuous: bool,
    /// Continuous panel (DESIGN_CONTINUOUS_PANEL.md): the SINGLE continuous
    /// control (toggled by `A-c`). When ON, a dedicated continuous COLUMN is
    /// split off the right of the sidebar (orchestrators with nested subtasks);
    /// when OFF, continuous tasks are hidden entirely. Either way the main
    /// sidebar builders NEVER emit continuous sessions — they render only in the
    /// column, never in both places. Off by default. Persisted in the manifest.
    pub continuous_column_on: bool,
    /// User-assigned accent colors for planning tasks, keyed by task id.
    /// Tasks live in the planning API, so their color rides in the local
    /// manifest as a sidecar (`Manifest::task_colors`). Consumed by the
    /// sidebar `TaskHeader` arm; edited via A-e on a task row.
    pub task_colors: HashMap<String, String>,
    /// Result of the startup memory-cap preflight probe. Cached for
    /// the lifetime of the run; consulted in `spawn_agent_session`
    /// to decide whether to wrap a spawn. See DESIGN_MEMORY_CAP.md
    /// § Components / Preflight.
    pub memory_cap_status: crate::memory_cap::MemoryCapAvailability,
    /// Channel watcher threads use to push `MemoryKillEvent`s back
    /// to the main loop. The receiver is drained each tick by
    /// `drain_memory_kill_events`. The sender is cloned into each
    /// capped session's watcher thread.
    ///
    /// Retained to keep the mpsc channel's sender half alive for the
    /// lifetime of the `App` so `memory_kill_rx` never observes a
    /// spurious `Disconnected`. No current call site clones it (the
    /// local-PTY cap-watcher spawn path that did was removed when
    /// workflow participant spawning moved daemon-side).
    #[allow(dead_code)]
    pub memory_kill_tx: std::sync::mpsc::Sender<crate::session_watch::MemoryKillEvent>,
    pub memory_kill_rx: std::sync::mpsc::Receiver<crate::session_watch::MemoryKillEvent>,
    /// 10e-c: TUI consumer for daemon's `manifest.watch` stream.
    /// `Some` only when daemon mode is opt-in active
    /// (`CM_USE_DAEMON_SOCKET=1`); the consumer thread dials the
    /// daemon socket, subscribes, and forwards
    /// `ManifestEvent`s (snapshot + diffs) through this receiver.
    /// `None` in legacy single-process mode — no consumer
    /// thread spawned.
    ///
    /// 10e-c r1 F1: events were `ManifestDiff` pre-r1 — r1
    /// widened to `ManifestEvent` so the post-disconnect
    /// snapshot reconciliation surfaces in the type system.
    ///
    /// Drained per tick by `drain_manifest_watch_events`. Each
    /// event applies to `TerminalSession.preserved_last_exit`:
    /// Diff(Exited) sets it unconditionally; Snapshot
    /// conservatively only fills it when local is None
    /// (avoids clobbering live broadcasts the TUI processed
    /// pre-disconnect). Unknown uids are silent no-ops (R5).
    pub manifest_watch_rx:
        Option<std::sync::mpsc::Receiver<crate::manifest_watch::ManifestEvent>>,
    /// 10e-c: thread handles for the manifest.watch consumers.
    /// Held for the App's lifetime; threads are reaped by
    /// process exit. 12e-r2 F2: now a `Vec` — one consumer
    /// per host in `hosts.toml`, so multi-host setups stream
    /// manifest events from every daemon in parallel.
    pub _manifest_watch_threads: Vec<std::thread::JoinHandle<()>>,
    /// 11d: receiver from the `events.subscribe` consumer thread.
    /// Drained per tick by [`App::drain_workflow_watch_events`].
    /// Carries either a `Snapshot(WorkflowRun)` (one per active
    /// run on (re)subscribe) or `Event(Event)` (live broadcast).
    pub workflow_watch_rx:
        Option<std::sync::mpsc::Receiver<crate::workflow_watch::WorkflowWatchEvent>>,
    /// 11d: thread handles for the events.subscribe consumers.
    /// Same lifecycle convention as `_manifest_watch_threads`.
    /// 12e-r2 F2: per-host (Vec).
    pub _workflow_watch_threads: Vec<std::thread::JoinHandle<()>>,
    /// Background per-remote-host `list_sessions` poller channel. Drained per
    /// tick into `remote_session_lists`; the adopt scan reads that cache so the
    /// MAIN thread never does a synchronous remote `list_sessions` RPC (that
    /// every-5s round-trip over a slow tunnel was the "TUI freezes" regression).
    pub session_poll_rx: Option<
        std::sync::mpsc::Receiver<(
            cm_daemon::host_id::HostId,
            Vec<crate::client_session::DaemonSessionSummary>,
        )>,
    >,
    /// Thread handles for the per-remote-host session pollers (lifecycle like
    /// `_manifest_watch_threads`).
    pub _session_poll_threads: Vec<std::thread::JoinHandle<()>>,
    /// Latest daemon session list per REMOTE host, fed off-thread by the
    /// session pollers. The adopt scan reads this instead of a synchronous RPC.
    pub remote_session_lists: std::collections::HashMap<
        cm_daemon::host_id::HostId,
        Vec<crate::client_session::DaemonSessionSummary>,
    >,
    /// Background per-host `continuous.dispatch_pending` poller channel
    /// (`spawn_dispatch_pending_pollers`; every host incl. local, 30s).
    /// Drained per tick into `continuous_dispatch_pending`. `None` in tests.
    pub dispatch_pending_rx: Option<
        std::sync::mpsc::Receiver<(
            cm_daemon::host_id::HostId,
            std::collections::HashMap<
                String,
                Vec<cm_daemon::continuous::dispatch_pending::PendingIssue>,
            >,
        )>,
    >,
    /// Thread handles for the dispatch-pending pollers (lifecycle like
    /// `_session_poll_threads`).
    pub _dispatch_pending_threads: Vec<std::thread::JoinHandle<()>>,
    /// Latest dispatch-pending report per host → per continuous task_id:
    /// index issues an operator unblocked (dated OPERATOR directive, cleared
    /// blocked_reason, no ack) that MAY still await orchestrator dispatch.
    /// Replaced wholesale per host on every poller delivery, so cleared
    /// directives drop out. The render path applies the final
    /// planning-liveness filter (`session_dispatch_pending`).
    pub continuous_dispatch_pending: std::collections::HashMap<
        cm_daemon::host_id::HostId,
        std::collections::HashMap<
            String,
            Vec<cm_daemon::continuous::dispatch_pending::PendingIssue>,
        >,
    >,
    /// Off-thread remote-attach worker. `Some` in production; `None` in tests
    /// (which exercise the synchronous inline reattach path). When present, the
    /// deferred-reattach drain DISPATCHES attaches to it instead of blocking the
    /// main thread, and `drain_attach_results` binds the ready sessions.
    pub attach_worker: Option<crate::attach_worker::AttachWorker>,
    /// Remote sessions whose attach is in-flight on the worker (uid → the queued
    /// entry, kept so a failed attach can be re-queued / capped). Prevents
    /// re-dispatching an attach that's already running.
    pub attaching: std::collections::HashMap<String, PendingRemoteReattach>,
    /// 10e-d: per-process de-dup set for cap-kill toasts. A given
    /// session's cap-kill event can reach the TUI through two
    /// side-channels — the attach-stream End frame (immediate,
    /// 10c-e-3b-fix4b) and the manifest.watch diff broadcast
    /// (eventual, 10e-c). Both paths converge on the activity
    /// feed via `try_emit_cap_kill_toast`, which checks this set
    /// first and inserts on emit. The set survives for the
    /// TUI process lifetime; uids generated by `new_session_uid`
    /// are monotonic so production never reuses one. The
    /// `clear_cap_kill_toast_state` helper releases an entry
    /// (defensive: covers test-paths that reuse uids; not
    /// required for production correctness).
    pub cap_kill_toasted: std::collections::HashSet<String>,
    /// 12a (Phase 3): parsed `~/.cm/hosts.toml`. Synthesized
    /// local-default when the file is missing (A1 in the Phase 3
    /// plan). No consumer yet — 12b adds the field to manifest
    /// entries; 12c wires the connection pool; the load happens
    /// here so a malformed config file surfaces at App::new
    /// rather than at first RPC.
    pub hosts: crate::hosts::HostsConfig,
    /// 12c (Phase 3): host_id → ConnectionHandle pool. Built
    /// once from `hosts` at App::new; every RPC call site that
    /// pre-12c dialed `cm_daemon::default_socket_path()`
    /// directly now routes through `host_pool` instead
    /// (`for_host(&ts.host_id)` for per-session calls,
    /// `default_handle()` for TUI-level pushes).
    ///
    /// 12e: wrapped in `Arc` so the watch-consumer threads can
    /// hold a `SocketPathProvider` closure that refreshes the
    /// path on each reconnect (F2 fix).
    pub host_pool: std::sync::Arc<crate::host_pool::HostPool>,
    /// 12e-r7 F1: manifest entries that were SKIPPED at
    /// restore time because their `host_id` failed the
    /// `guard_local_host_only` check. Keyed by workspace id;
    /// each value is the full set of skipped entries for
    /// that workspace, preserved verbatim so `save_session_manifest`
    /// can round-trip them back to disk.
    ///
    /// Without this, round-6's restore guard caused
    /// permanent data loss: the entry was filtered from live
    /// state at restore, and the next save (which serializes
    /// only `ws.sessions`) dropped it from disk forever.
    /// `~/.cm/tui-sessions.json` round-trips a remote-pinned
    /// entry through TUI restarts on a local active_host (or
    /// post-Phase-3 daemon-side reattach support) just like
    /// it would for a local entry.
    ///
    /// Phase 3 staging area: when daemon-side path resolution
    /// lands (slice 12g), these entries become loadable
    /// again. Until then, they ride along on disk untouched.
    pub skipped_manifest_entries:
        HashMap<String, Vec<cm_daemon::manifest::ManifestEntry>>,
    /// Phase 4 startup-freeze fix: remote manifest entries whose host uses a
    /// blocking transport (`ssh-unix`/`tcp-tls`), deferred out of
    /// `restore_sessions`' synchronous loop so a configured-but-slow remote
    /// no longer freezes the first frame for ~1-3s per host. The per-host
    /// `manifest.watch` consumer warms the tunnel off the main thread;
    /// `drain_deferred_remote_reattach` (per tick) reattaches each entry once
    /// its tunnel is connectable. Until then the raw entry also rides in
    /// `skipped_manifest_entries` so a save during the window round-trips it.
    pending_remote_reattach: Vec<PendingRemoteReattach>,
    /// Background fanout worker for daemon RPC pushes. Main
    /// thread builds owned snapshots and fires them at this
    /// worker; the worker coalesces bursts and does the per-host
    /// RPC. Keeps keystroke handling off the network's RTT
    /// budget (~500ms-1s per push to SSH-tunneled `cm-manager`).
    pub push_worker: crate::push_worker::PushWorker,
    /// Last-drawn `view_mode` for the Clear-on-transition gate.
    /// `None` until the first draw — that draw always clears
    /// (initial paint).
    last_drawn_view_mode: Option<ViewMode>,
    /// Discriminant of the last-drawn `input_mode`. Opening or
    /// closing an input dialog flips the discriminant; sub-field
    /// edits (typing inside the dialog) don't. Used as the
    /// second half of the Clear-on-transition gate.
    last_drawn_input_disc: Option<std::mem::Discriminant<InputMode>>,
    /// Pending attention alerts raised by the `notify_user` MCP tool,
    /// keyed by the alerting session's uid → the agent-supplied message.
    /// Transient + in-memory only (never persisted to the manifest): an
    /// alert is the live "this session wants you" signal that drives the
    /// blinking sidebar indicator, and it's cleared the moment the user
    /// selects that session's row. See `tick_alerts` / `reap_and_clear_alerts`.
    alerts: HashMap<String, String>,
    /// Fingerprint of every open-workspace idle session's `(uid, age
    /// bucket)` set at the last `tick_idle_ages` evaluation. Idle ages only
    /// matter at bucket granularity (afterglow → settled → stale), and an
    /// idle session produces no PTY events to ride the normal repaint —
    /// this is how a bucket-boundary crossing forces a redraw. Transient.
    idle_bucket_fingerprint: u64,
    /// Throttle stamp for `tick_idle_ages` (re-evaluated at most ~1/s;
    /// buckets move on minute scales so per-iteration checks are waste).
    last_idle_bucket_check: Option<Instant>,
    /// Last alert animation frame we forced a redraw for. An alerting session
    /// is usually idle (no PTY output → no redraws), so the heartbeat can't
    /// ride the normal event-driven repaint; `tick_alerts` flips `needs_redraw`
    /// whenever this frame index advances so the icon keeps animating.
    last_alert_frame: u64,
    /// Remote sessions whose attach I/O stream died (the SSH tunnel
    /// dropped) but whose daemon-side PTY + workflow keep running.
    /// Keyed by session uid. Transient + in-memory only (never
    /// persisted): set in `drain_pty_events` when a REMOTE session's
    /// `daemon_transport_eof` flag fires on exit, and cleared in
    /// `drain_deferred_remote_reattach` once the PTY rebinds (or the
    /// session is found genuinely gone). Drives the `⟳ reconnecting`
    /// sidebar indicator and keeps the session slot alive so no work
    /// is lost while connectivity is restored. Mirrors the transient
    /// per-uid shape of [`Self::alerts`]; the matching reattach
    /// worklist entry rides in [`Self::pending_remote_reattach`].
    reconnecting_sessions: std::collections::HashSet<String>,
    /// Per-uid record of the host tunnel-generation each ATTACHED remote
    /// session was dialed under (see `HostPool::tunnel_generation`). Written
    /// when a remote attach succeeds (`drain_attach_results` /
    /// `drain_deferred_remote_reattach`); read by the per-tick watchdog
    /// `requeue_stale_generation_remote_sessions`, which re-queues any session
    /// whose recorded generation is now behind the host's current one — i.e.
    /// its tunnel was replaced (died), so its attach stream is dead even if it
    /// never produced a clean EOF (the half-open freeze this closes, S3).
    /// Transient + in-memory only. Cleared when the session is re-queued (and
    /// re-inserted on the next successful attach).
    attached_tunnel_generation: std::collections::HashMap<String, u64>,
}

/// Phase 6 activity-feed entry. Logged from each mutating control-socket
/// method handler via `App::log_activity`. Each entry is one observable
/// mutation (start_session, send_input, kill_session, start/stop_workflow,
/// create_subtask, mark_subtask_done, propose_task).
#[derive(Clone, Debug)]
pub struct ActivityEntry {
    /// Wall-clock timestamp the mutation landed (used for the leading
    /// `HH:MM:SS` column in the rendered strip).
    pub ts: std::time::SystemTime,
    /// Human-friendly caller label. For workflow participants this is
    /// the role name (`worker`/`reviewer`/`manager`); otherwise the
    /// session's sidebar label (e.g. `survey-claude`). Falls back to
    /// the caller's session_uid prefix if neither is available.
    pub caller_label: String,
    /// Compact one-line summary of the mutation, formatted by the
    /// caller. Example: `start_session(refactor-helpers, codex)` or
    /// `mark_subtask_done(b4264d86, close_worktree=true)`.
    pub summary: String,
}

/// How many activity entries to retain. ~50 covers a few minutes of busy
/// orchestration while keeping the buffer cheap. The strip itself only
/// renders the last few; the rest exist for a future scrollable view.
const ACTIVITY_LOG_CAP: usize = 50;

/// 10e-d: unified activity-feed summary for daemon-path
/// cap-kills. Used by both the attach-stream End-frame path and
/// the manifest.watch Exited-diff path so the user sees the same
/// string whether they were attached at kill time or not. (The
/// local-spawn path in `drain_memory_kill_events` keeps its
/// richer PID/comm/RSS format — different concern, different
/// surface.)
pub(crate) const CAP_KILL_TOAST_MESSAGE: &str =
    "killed by memory cap (daemon session)";

impl App {
    pub fn new(config: Config) -> Self {
        let backend = BackendHandle::spawn(&config);
        let manifest = Self::load_manifest();
        let sidebar_view = match manifest.view.as_deref() {
            Some("task") => SidebarView::Task,
            _ => SidebarView::Status,
        };
        let hide_continuous = manifest.hide_continuous;
        let continuous_column_on = manifest.continuous_column_on;
        let task_colors = manifest.task_colors.clone();
        // Only keep bindings whose target workspace still exists in the
        // manifest — otherwise we'd set workspace_id to a dangling id that
        // nothing resolves to.
        let known_ws_ids: HashSet<&String> = manifest.workspaces.keys().collect();
        let manifest_bindings: HashMap<String, String> = manifest
            .bindings
            .iter()
            .filter(|(_, ws_id)| known_ws_ids.contains(ws_id))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let workflows_dir = workflow::toml_schema::workflows_dir();
        let (workflows, load_errs) = workflow::toml_schema::load_all(&workflows_dir);
        let workflow_load_errors = filter_real_workflow_load_errors(&workflows_dir, load_errs);
        for (path, err) in &workflow_load_errors {
            eprintln!("workflow load failed: {}: {}", path.display(), err);
        }
        let workflow_runs = workflow::run::load_all()
            .into_iter()
            .filter(|r| r.is_active())
            .collect();
        // Start the control socket. Failures aren't fatal — the TUI runs
        // fine without it; only MCP-driven control becomes unavailable.
        // When another instance already owns the socket we DON'T steal it
        // (that would clobber a legit second TUI); instead we record the
        // degraded state, surface a banner, and retry the bind each tick
        // (`maybe_rebind_control_socket`) so this instance self-heals the
        // moment the holder exits.
        let control_queue = crate::control::queue::Queue::new();
        let control_socket_path = crate::control::server::default_socket_path();
        let (control_bound, control_conflict_pid) =
            match crate::control::server::start(control_queue.clone()) {
                Ok(path) => {
                    eprintln!("control socket bound at {}", path.display());
                    (true, None)
                }
                Err(e) => {
                    let owner = crate::control::server::read_owner_pid(&control_socket_path);
                    eprintln!(
                        "control socket NOT bound ({}) — running WITHOUT a control plane; \
                         MCP/agent control won't reach this instance{}. Retrying every {}s.",
                        e,
                        owner
                            .map(|p| format!(" (held by PID {p})"))
                            .unwrap_or_default(),
                        CONTROL_REBIND_INTERVAL.as_secs(),
                    );
                    (false, owner)
                }
            };
        // Memory-cap preflight: run once at startup, cache the result.
        // Subsequent `spawn_agent_session` calls consult this synchronously
        // — no per-spawn probing.
        let memory_cap_status = crate::preflight::probe();
        let mut activity_log: VecDeque<ActivityEntry> = VecDeque::new();
        if let crate::memory_cap::MemoryCapAvailability::Unavailable { reason } = &memory_cap_status
        {
            activity_log.push_back(ActivityEntry {
                ts: std::time::SystemTime::now(),
                caller_label: "preflight".into(),
                summary: format!("memory cap disabled: {}", reason),
            });
        }
        let (memory_kill_tx, memory_kill_rx) = std::sync::mpsc::channel();

        // 12a (Phase 3): load `~/.cm/hosts.toml`. Synthesizes the
        // local-default entry when the file is missing (the common
        // case for users who haven't opted into multi-host).
        // Load failures (malformed TOML, validation errors) print
        // to stderr and fall through to the synthesized default so
        // the TUI still launches — the operator can fix the file
        // and restart. No RPC consumer yet (12c wires that).
        //
        // Reviewer-round (12a): the fallback uses
        // `HostsConfig::synthesized_local_default()` directly — a
        // pure constructor with no filesystem touch — rather than
        // re-loading from a sentinel path. The old shape used
        // `/dev/null/hosts.toml-nonexistent` as the "missing"
        // sentinel, but opening a child path of `/dev/null` returns
        // `NotADirectory`, not `NotFound`, so the sentinel hit the
        // I/O-error branch and the `.expect` panicked — exactly
        // the lockout we were trying to prevent.
        let hosts = match crate::hosts::HostsConfig::load(
            &crate::hosts::default_path(),
        ) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!(
                    "cm-tui: hosts.toml load failed: {} — \
                     falling back to synthesized local default",
                    e,
                );
                crate::hosts::HostsConfig::synthesized_local_default()
            }
        };
        // 12c (Phase 3): build the host_id → ConnectionHandle
        // pool from the loaded HostsConfig. Every runtime RPC
        // site below routes through this pool; pre-12c they
        // each called `cm_daemon::default_socket_path()`
        // directly. Local-only behavior is byte-identical to
        // pre-12c because the pool's local-host entry holds
        // exactly that path (verified by
        // `host_pool::tests::synthesized_default_pool_local_path_matches_daemon_default`).
        // 12e-r3 F3: when pool construction fails, the
        // synthesized-local-default pool MUST also replace
        // `hosts` — otherwise `active_host` (derived below
        // from `hosts.default_host()`) points to a host that
        // isn't in the pool, every subsequent
        // `host_pool.for_host(&active_host)` returns
        // `Err(NotFound)`, and the operator's A-H cycles
        // produce "unknown host" errors. Re-bind both `hosts`
        // AND `host_pool` together so the App's view is
        // self-consistent.
        // 12e-r3 F3: when pool construction fails, the
        // synthesized-local-default pool MUST also replace
        // `hosts` — otherwise `active_host` (derived below
        // from `hosts.default_host()`) points to a host that
        // isn't in the pool, every subsequent
        // `host_pool.for_host(&active_host)` returns
        // `Err(NotFound)`, and the operator's A-H cycles
        // produce "unknown host" errors. Re-bind both `hosts`
        // AND `host_pool` together so the App's view is
        // self-consistent.
        let (hosts, host_pool) =
            match crate::host_pool::HostPool::from_config(&hosts) {
                Ok(pool) => (hosts, pool),
                Err(e) => {
                    eprintln!(
                        "cm-tui: HostPool::from_config failed: {} — \
                         falling back to local-only default (the \
                         configured multi-host setup is not \
                         currently usable; check tunnel-dir perms / \
                         XDG_RUNTIME_DIR / hosts.toml)",
                        e,
                    );
                    let local = crate::hosts::HostsConfig::synthesized_local_default();
                    let pool = crate::host_pool::HostPool::from_config(&local)
                        .expect("local-only default is infallible (no ssh hosts)");
                    (local, pool)
                }
            };
        let host_pool = std::sync::Arc::new(host_pool);
        // 12e-r2 F2 (Option A): per-host watch consumers. One
        // manifest.watch + one events.subscribe consumer per
        // entry in `hosts.toml`, each bound to that host's path
        // via the path provider. Single-host setups still get one
        // consumer; multi-host setups stream events from every
        // daemon in parallel — every configured host is always
        // live (which is what makes the always-show-all-hosts,
        // no-global-active-host model work).
        // 10e-c: spawn the manifest.watch consumer. Without
        // daemon there's no manifest to subscribe to; spawning
        // a consumer that tight-loops trying to dial a non-
        // existent socket would be wasted work + log noise.
        let (manifest_watch_rx, _manifest_watch_threads) =
            crate::manifest_watch::spawn_per_host(&host_pool, &hosts);
        // 11d: spawn the events.subscribe consumer alongside
        // manifest_watch. Same reconnect-with-backoff shape;
        // delivers WorkflowEvent broadcasts to the main loop.
        let (workflow_watch_rx, _workflow_watch_threads) =
            crate::workflow_watch::spawn_per_host(&host_pool, &hosts);
        // Per-remote-host session-list pollers: fetch `list_sessions` OFF the
        // main thread so the adopt scan never blocks the UI on a remote RPC.
        let (session_poll_rx, _session_poll_threads) =
            crate::client_session::spawn_session_pollers(&host_pool, &hosts);
        // Per-host dispatch-pending pollers (Continuous panel's ○ indicator).
        // Skipped under cfg(test) — like `attach_worker` — so unit tests never
        // spawn a thread that dials the developer's real local daemon socket.
        let (dispatch_pending_rx, _dispatch_pending_threads) = if cfg!(test) {
            (None, Vec::new())
        } else {
            crate::client_session::spawn_dispatch_pending_pollers(&host_pool, &hosts)
        };

        // Push fanout worker. Owns its own thread; receives
        // owned snapshots from the main thread via mpsc and
        // does the per-host daemon RPC fanout off the keystroke
        // path. See `push_worker` module doc for design.
        let push_worker =
            crate::push_worker::PushWorker::spawn(std::sync::Arc::clone(&host_pool));
        // Off-thread remote-attach worker (production). The deferred-reattach
        // drain dispatches to it so a slow tunnel attach never blocks the UI.
        // Tests use the SYNCHRONOUS inline reattach path (no worker) so the
        // deferred-reattach assertions stay deterministic — no real background
        // attach thread to race.
        let attach_worker = if cfg!(test) {
            None
        } else {
            Some(crate::attach_worker::AttachWorker::spawn(std::sync::Arc::clone(
                &host_pool,
            )))
        };

        App {
            tasks: Vec::new(),
            workspaces: Vec::new(),
            cursor: Cursor::Workspace(0),
            cursor_column: SidebarColumn::Main,
            saved_main_cursor: None,
            session_mru: VecDeque::new(),
            mru_walk: None,
            saved_continuous_uid: None,
            sidebar_view,
            view_mode: ViewMode::Sessions,
            planning: PlanningView::new(),
            should_quit: false,
            last_term_size: (80, 24),
            last_adopt_scan: None,
            config,
            backend,
            connected: false,
            status_msg: None,
            needs_redraw: true,
            force_clear: false,
            input_mode: InputMode::Normal,
            start_time: Instant::now(),
            sessions_restored: false,
            manifest_bindings,
            last_session_id_check: Instant::now(),
            last_workflow_tick: Instant::now(),
            workflows,
            workflow_load_errors,
            workflow_runs,
            history_watcher: workflow::history::HistoryWatcher::new(),
            pending_rotations: Vec::new(),
            mouse_capture_enabled: true,
            control_queue,
            control_bound,
            control_conflict_pid,
            control_socket_path,
            control_rebind_at: Instant::now() + CONTROL_REBIND_INTERVAL,
            activity_log,
            activity_visible: false,
            hide_continuous,
            continuous_column_on,
            task_colors,
            memory_cap_status,
            memory_kill_tx,
            memory_kill_rx,
            manifest_watch_rx,
            _manifest_watch_threads,
            workflow_watch_rx,
            _workflow_watch_threads,
            session_poll_rx,
            _session_poll_threads,
            remote_session_lists: std::collections::HashMap::new(),
            dispatch_pending_rx,
            _dispatch_pending_threads,
            continuous_dispatch_pending: std::collections::HashMap::new(),
            cap_kill_toasted: std::collections::HashSet::new(),
            hosts,
            host_pool,
            skipped_manifest_entries: HashMap::new(),
            pending_remote_reattach: Vec::new(),
            attach_worker,
            attaching: HashMap::new(),
            push_worker,
            last_drawn_view_mode: None,
            last_drawn_input_disc: None,
            alerts: HashMap::new(),
            last_alert_frame: 0,
            idle_bucket_fingerprint: 0,
            last_idle_bucket_check: None,
            reconnecting_sessions: std::collections::HashSet::new(),
            attached_tunnel_generation: std::collections::HashMap::new(),
        }
    }

    /// 10e-d: idempotent cap-kill toast emission. Both the
    /// attach-stream End-frame path (10c-e-3b-fix4b) and the
    /// manifest.watch Exited-diff path (10e-c) converge here,
    /// so a session that's daemon-attached at kill time gets a
    /// single activity-feed entry regardless of which path
    /// arrives first.
    ///
    /// De-dup is per-uid for the TUI process lifetime via
    /// `cap_kill_toasted`. Production uids are monotonic
    /// (`new_session_uid`), so the set entry stays valid for
    /// the session's whole existence. `clear_cap_kill_toast_state`
    /// is the defensive escape hatch for uid reuse (test paths).
    ///
    /// Returns `true` if a toast was actually emitted this call.
    /// Useful for tests that pin the call-vs-suppress contract.
    pub fn try_emit_cap_kill_toast(&mut self, uid: &str) -> bool {
        if self.cap_kill_toasted.contains(uid) {
            return false;
        }
        self.cap_kill_toasted.insert(uid.to_string());
        self.log_activity(uid, CAP_KILL_TOAST_MESSAGE.to_string());
        true
    }

    /// 10e-d: release the cap-kill de-dup entry for a uid so a
    /// re-spawned session under the same uid can toast again.
    /// In production `new_session_uid()` returns fresh monotonic
    /// ids so this is a no-op; the helper exists for test-paths
    /// that simulate the re-spawn case, and as a defensive hook
    /// for spawn sites.
    pub fn clear_cap_kill_toast_state(&mut self, uid: &str) {
        self.cap_kill_toasted.remove(uid);
    }




    // ── Cursor helpers ──────────────────────────────────────────────

    /// Return the workspace index the cursor is currently on.
    fn active_workspace_index(&self) -> Option<usize> {
        if self.workspaces.is_empty() {
            return None;
        }
        let wi = match &self.cursor {
            Cursor::Workspace(wi) => *wi,
            Cursor::Task { ws_idx, .. } => *ws_idx,
            Cursor::Session(wi, _) => *wi,
        };
        (wi < self.workspaces.len()).then_some(wi)
    }

    /// Return the task_id for the cursor's current task scope, if any:
    ///   - Cursor::Task → the task_id on the cursor
    ///   - Cursor::Session → the session's task_id (may be None)
    ///   - Cursor::Workspace → None (not task-scoped, even if one task is bound)
    fn cursor_task_id(&self) -> Option<String> {
        match &self.cursor {
            Cursor::Task { task_id, .. } => Some(task_id.clone()),
            Cursor::Session(wi, si) => self
                .workspaces
                .get(*wi)
                .and_then(|w| w.sessions.get(*si))
                .and_then(|ts| ts.task_id.clone()),
            Cursor::Workspace(_) => None,
        }
    }

    /// Return a reference to the active terminal session (workspace + session).
    fn active_session(&self) -> Option<(&Workspace, &TerminalSession)> {
        match &self.cursor {
            Cursor::Session(wi, si) => {
                let ws = self.workspaces.get(*wi)?;
                let ts = ws.sessions.get(*si)?;
                Some((ws, ts))
            }
            Cursor::Workspace(wi) => {
                let ws = self.workspaces.get(*wi)?;
                if ws.sessions.len() == 1 {
                    Some((ws, &ws.sessions[0]))
                } else {
                    None
                }
            }
            Cursor::Task { ws_idx, task_id } => {
                let ws = self.workspaces.get(*ws_idx)?;
                let matches: Vec<&TerminalSession> = ws
                    .sessions
                    .iter()
                    .filter(|ts| ts.task_id.as_deref() == Some(task_id.as_str()))
                    .collect();
                if matches.len() == 1 {
                    Some((ws, matches[0]))
                } else {
                    None
                }
            }
        }
    }

    /// Return a mutable reference to the active terminal session.
    fn active_session_mut(&mut self) -> Option<&mut TerminalSession> {
        match &self.cursor {
            Cursor::Session(wi, si) => {
                let ws = self.workspaces.get_mut(*wi)?;
                ws.sessions.get_mut(*si)
            }
            Cursor::Workspace(wi) => {
                let ws = self.workspaces.get_mut(*wi)?;
                if ws.sessions.len() == 1 {
                    Some(&mut ws.sessions[0])
                } else {
                    None
                }
            }
            Cursor::Task { ws_idx, task_id } => {
                let task_id = task_id.clone();
                let ws = self.workspaces.get_mut(*ws_idx)?;
                let mut found_idx = None;
                let mut count = 0;
                for (i, ts) in ws.sessions.iter().enumerate() {
                    if ts.task_id.as_deref() == Some(task_id.as_str()) {
                        count += 1;
                        if count == 1 {
                            found_idx = Some(i);
                        } else {
                            return None;
                        }
                    }
                }
                ws.sessions.get_mut(found_idx?)
            }
        }
    }

    // ── Workspace / task lookup helpers ─────────────────────────────

    fn workspace_index_by_id(&self, id: &str) -> Option<usize> {
        self.workspaces.iter().position(|w| w.id == id)
    }
}

/// Resolve a `(workspace_id, session_uid)` pair to current `(ws_index,
/// session_index)`. Returns `None` if either has been removed since the
/// IDs were captured. Free function so it can be unit-tested against a
/// hand-rolled `&[Workspace]` without building a full `App`.
fn resolve_session_by_ids(
    workspaces: &[Workspace],
    workspace_id: &str,
    session_uid: &str,
) -> Option<(usize, usize)> {
    let wi = workspaces.iter().position(|w| w.id == workspace_id)?;
    let si = workspaces[wi]
        .sessions
        .iter()
        .position(|s| s.uid == session_uid)?;
    Some((wi, si))
}

impl App {

    /// First task bound to the given workspace, if any. Used by push/pull
    /// (which need *a* representative task) and the detail panel (shows one
    /// prompt). Multi-task workspaces have no canonical ordering; first-
    /// insertion-wins.
    fn first_task_for_ws(&self, ws_id: &str) -> Option<&TaskEntry> {
        self.tasks
            .iter()
            .find(|t| t.workspace_id.as_deref() == Some(ws_id))
    }

    /// A workspace is "past" if it's been put away but its worktree may still
    /// be on disk. Cloud workspaces never qualify — there's no worktree to
    /// reopen and the VM may be gone. Local workspaces qualify when either:
    ///   - `is_closed = true` (explicit A-W close), or
    ///   - they have no live sessions AND every bound task is done.
    /// An unbound, sessionless, open workspace is NOT past — it's a fresh
    /// workspace waiting for sessions.
    fn is_past_workspace(&self, wi: usize) -> bool {
        let Some(ws) = self.workspaces.get(wi) else {
            return false;
        };
        if ws.is_cloud {
            return false;
        }
        if ws.is_closed {
            return true;
        }
        if !ws.sessions.is_empty() {
            return false;
        }
        let bound: Vec<&TaskEntry> = self
            .tasks
            .iter()
            .filter(|t| t.workspace_id.as_deref() == Some(&ws.id))
            .collect();
        !bound.is_empty()
            && bound
                .iter()
                .all(|t| matches!(t.api_status, TaskStatus::Done))
    }

    /// Compute effective task status: derived from the workspace's sessions
    /// if bound, otherwise falls back to api_status.
    fn task_status(&self, task: &TaskEntry) -> TaskStatus {
        if let Some(ws) = task
            .workspace_id
            .as_deref()
            .and_then(|id| self.workspaces.iter().find(|w| w.id == id))
        {
            if ws.sessions.iter().any(|s| s.status == SessionStatus::Running) {
                return TaskStatus::Running;
            }
            if ws.sessions.iter().any(|s| s.status == SessionStatus::Idle) {
                return TaskStatus::Blocked;
            }
            if ws.worker_vm.as_deref().is_some_and(|s| !s.is_empty()) {
                return task.api_status.clone();
            }
        }
        task.api_status.clone()
    }

    // ── MRU quick-switch (A-;) + palette jump plumbing ──────────────

    // ── Fuzzy-find palette (A-p) ────────────────────────────────────

    // ── Task-detail peek (A-i) ──────────────────────────────────────

    // ── Event processing ────────────────────────────────────────────

    fn set_status_msg(&mut self, msg: &str) {
        self.status_msg = Some((msg.to_string(), Instant::now()));
    }

    // ── Input handling ──────────────────────────────────────────────

    // ── Session management ──────────────────────────────────────────


    // (Retired: `cycle_active_host` / `A-H` — the global active_host is gone;
    // host is a per-workspace attribute. See DESIGN_REMOVE_GLOBAL_HOST.md.)

    /// Handle terminal resize.
    pub fn resize_terminals(&mut self, cols: u16, rows: u16) {
        self.last_term_size = (cols, rows);
        for ws in &mut self.workspaces {
            for ts in &mut ws.sessions {
                ts.session.resize(cols, rows);
            }
        }
    }

    // ── Drawing ──────────────────────────────────────────────────────

}

// ═══════════════════════════════════════════════════════════════════════════
//                            Workflow integration
// ═══════════════════════════════════════════════════════════════════════════

// ═══════════════════════════════════════════════════════════════════════════
//                         Workflow modal rendering
// ═══════════════════════════════════════════════════════════════════════════

// `drop_run_from_in_mem` + `drop_inactive_runs_from_in_mem` (the run-mirror
// lifecycle GC) moved to `crate::workflow::observer` (re-imported at the top of
// this file).

// Append a diagnostic line for a workflow run to its `tick.log`.
//
// Lives in `~/.cm/workflow-runs/<run_id>/tick.log`. Rate-limited to at most
// one distinct message per run per second to avoid spamming the file on every
// tick of the main loop. Best-effort — ignores all I/O errors.
// `log_tick` + its `TICK_LOG_MAX_BYTES` cap moved to
// `crate::workflow::observer` (re-exported below) as the first extraction of
// workflow-observation glue out of this file.
