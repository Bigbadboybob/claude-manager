//! Session-manifest types and the diff-broadcasting primitive that
//! backs the `manifest.watch` streaming RPC. Slice 9 of
//! doc/persistent-host-daemon.md.
//!
//! ## What this slice ships
//!
//! - [`LastExit`] — the additive `Option<LastExit>` field landing on
//!   `ManifestEntry` (in `tui/src/app.rs`) per the doc's Phase-1
//!   schema change. `#[serde(default)]` keeps manifests written by
//!   older binaries loading cleanly.
//! - [`ManifestDiff`] — the diff event the daemon emits when an entry
//!   is added / updated / exited / tombstoned. Carries the typed
//!   `LastExit` on the `Exited` variant so detached sessions can
//!   surface the memory-cap-kill toast on a `manifest.watch` stream
//!   (the doc's named acceptance criterion for the detached path).
//! - [`ManifestWatcher`] — broadcaster: subscribers get a receiver
//!   that yields every diff produced after subscribe. Dead
//!   subscribers are reaped lazily on the next broadcast (same
//!   pattern as [`crate::session::PtyByteFanout`]).
//!
//! ## What this slice does NOT ship
//!
//! - Daemon ownership of `~/.cm/tui-sessions.json` itself. The TUI
//!   still reads and writes the file; the daemon's `ManifestWatcher`
//!   has no producer wired to it yet. When slice 10 rewires
//!   `app.rs` to RPC, the daemon takes over the file and starts
//!   producing diffs on every mutation.
//! - The `manifest.watch` JSON-RPC method handler. That joins the
//!   dispatch table once `control/methods.rs` lands daemon-side
//!   (task #17, blocked on slice 9).
//! - The "current-snapshot-on-subscribe" replay. The eventual handler
//!   will send `Added` for every existing session at subscribe time,
//!   then live diffs. That's a thin layer over `ManifestWatcher` and
//!   doesn't change the broadcaster's contract.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Exit metadata recorded on a session's `ManifestEntry` when it
/// transitions to exited. Persisted as part of the manifest JSON
/// (Phase-1 schema addition; loads cleanly via `#[serde(default)]`
/// from older manifests that don't have the field). Also broadcast
/// verbatim via [`ManifestDiff::Exited`] so subscribers can render
/// the cap-kill toast / completion notice without re-reading the file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastExit {
    /// PTY child exit code when known. `None` for signal-terminated
    /// sessions, daemon-side forced kills, etc. Keep this `Option`
    /// rather than defaulting to a sentinel — the TUI's toast logic
    /// needs to distinguish "exited with 0" from "killed and we
    /// don't know what to say."
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    /// True iff the cgroup OOM-killer fired (memory-cap kill). The
    /// TUI's existing `signal 9` toast now sources from this flag
    /// for both the attached-session path (the End-frame variant in
    /// `term_shim::ChildEvent`) and the detached path (this
    /// `ManifestDiff::Exited` field).
    #[serde(default)]
    pub memory_cap_kill: bool,
    /// Byte offset into `~/.cm/memory_kills/<uid>.jsonl` of the
    /// matching record, when one was written. `None` for normal
    /// exits / signals that produced no kill record. Lets the TUI
    /// jump directly to the record when rendering details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kills_file_offset: Option<u64>,
    /// Unix-timestamp seconds when the session exited. Matches the
    /// existing `SessionTombstone.exited_at` so tombstone pruning
    /// uses the same clock.
    pub exited_at: f64,
}

/// One change to the session manifest, broadcast to live
/// `manifest.watch` subscribers.
///
/// `Added`/`Updated` carry the entry as `serde_json::Value` for now
/// rather than a typed struct: the TUI owns `ManifestEntry`'s
/// definition (in `tui/src/app.rs`) through Phase 1, so the daemon
/// can't reference the type without circular crate deps. When slice 10
/// moves `ManifestEntry` daemon-side, these variants take the typed
/// struct directly — the JSON shape stays the same on the wire so
/// existing subscribers don't notice.
/// 10e-b adds `Serialize`/`Deserialize` so diffs travel as JSON
/// payload on `manifest.watch` stream frames. Variant-tagged
/// (`{"Exited": {...}}`) — readable on the wire, matches the
/// natural Rust enum shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ManifestDiff {
    /// A new session was added to the manifest.
    Added { uid: String, entry: Value },
    /// An existing session's fields were updated in place (label
    /// rename, hidden toggle, workflow re-binding, etc.).
    Updated { uid: String, entry: Value },
    /// A session transitioned to exited. The detached-session leg
    /// of the doc's memory-cap-kill notification flows through this
    /// variant — the `last_exit.memory_cap_kill` flag is the toast
    /// trigger.
    Exited { uid: String, last_exit: LastExit },
    /// A session was tombstoned (moved out of the live manifest
    /// into 30-day retention). `exited_at` matches the corresponding
    /// `SessionTombstone.exited_at` field so subscribers can
    /// reconcile against their own state.
    Tombstoned { uid: String, exited_at: f64 },
}

/// Per-subscriber buffer size for the bounded `sync_channel` used by
/// [`ManifestWatcher`]. Sized for ~30 seconds of typical lifecycle
/// activity (a handful of session exits / second is already
/// pathological; normal rates are single-digit per minute). Slow
/// subscribers exceeding this depth get dropped on the next broadcast
/// via `try_send → retain` (10e-b R2 mitigation).
///
/// Tuning: bump if a future deployment surfaces high-rate diffs; the
/// constant is the only knob to change.
pub const MANIFEST_WATCH_BUFFER: usize = 32;

/// Internal shared state for [`ManifestWatcher`] — split out so
/// [`SubscriptionGuard`] can hold an `Arc` independent of any
/// outer `ManifestWatcher` lifecycle. The guard removes its slot
/// on Drop via [`ManifestWatcherInner::unsubscribe`].
///
/// `subscribers` is keyed by a u64 id minted from `next_id` (atomic
/// counter, monotonic). A `HashMap` rather than `Vec` so guards can
/// remove their specific slot in O(1) without scanning. Broadcast
/// iterates the map's senders; dead senders surface via `try_send`
/// errors and are removed from the map (deferred reap kept as
/// defense-in-depth alongside the RAII path).
pub struct ManifestWatcherInner {
    subscribers: Mutex<std::collections::HashMap<u64, mpsc::SyncSender<ManifestDiff>>>,
    next_id: AtomicU64,
}

impl ManifestWatcherInner {
    fn new() -> Self {
        Self {
            subscribers: Mutex::new(std::collections::HashMap::new()),
            next_id: AtomicU64::new(0),
        }
    }

    /// Remove a subscriber slot by id. Called from
    /// [`SubscriptionGuard::drop`] when the holder exits — any
    /// path, including panic. Idempotent: removing a non-present
    /// id (broadcaster already reaped it via try_send error) is
    /// a no-op.
    fn unsubscribe(&self, id: u64) {
        self.subscribers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id);
    }
}

/// RAII guard returned by [`ManifestWatcher::subscribe`]. When
/// dropped, removes its specific subscriber slot from the
/// broadcaster's map — even if the broadcaster's `try_send`
/// reaping path hasn't fired (no broadcast since the disconnect).
///
/// 10e-b r2 fix: pre-r2 a quiet manifest stream that disconnected
/// left an orphan SyncSender in the broadcaster's `Vec<>` until
/// the next broadcast triggered `retain`. Across repeated
/// connect/disconnect cycles in idle daemons, dead slots
/// accumulated. The guard reaps immediately on handler exit.
///
/// Held by [`crate::control::dispatch::ManifestWatchHandle`];
/// dropped at handle scope end (handler return, panic, abort).
pub struct SubscriptionGuard {
    inner: Arc<ManifestWatcherInner>,
    id: u64,
}

impl Drop for SubscriptionGuard {
    fn drop(&mut self) {
        self.inner.unsubscribe(self.id);
    }
}

/// Multi-subscriber broadcaster for [`ManifestDiff`] events.
///
/// Same shape as [`crate::session::PtyByteFanout`] but without a
/// replay buffer — manifest watchers care about live events plus an
/// initial snapshot, which the [`manifest.watch`] RPC handler
/// composes from a `state.workspaces`/`state.bindings` snapshot read
/// plus subscription rather than a ring buffer.
///
/// 10e-b: subscribers use bounded `sync_channel(MANIFEST_WATCH_BUFFER)`
/// + `try_send` in `broadcast`. Full or disconnected senders are
/// reaped via the broadcast's map-remove (defense-in-depth).
///
/// 10e-b r2: [`subscribe`] now returns a [`SubscriptionGuard`] in
/// addition to the receiver. The guard reaps the specific slot
/// immediately on drop, so handler exit doesn't leave orphan
/// slots in idle daemons. Broadcast-time reaping is retained for
/// the "broadcaster found slow/dead sender" case; the two paths
/// are complementary.
pub struct ManifestWatcher {
    inner: Arc<ManifestWatcherInner>,
}

impl Default for ManifestWatcher {
    fn default() -> Self {
        Self {
            inner: Arc::new(ManifestWatcherInner::new()),
        }
    }
}

impl ManifestWatcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new subscriber. Returns:
    /// - a bounded receiver (capacity [`MANIFEST_WATCH_BUFFER`])
    ///   that yields every diff broadcast *after* this call.
    /// - a [`SubscriptionGuard`] that reaps the subscriber's slot
    ///   from the broadcaster's map on drop.
    ///
    /// Holders MUST keep the guard alive for the lifetime of the
    /// receiver — dropping the guard early eagerly unsubscribes
    /// (subsequent broadcasts won't reach the receiver even
    /// though it's still alive). The `manifest.watch` RPC's
    /// `ManifestWatchHandle` ties both together so this can't be
    /// fumbled by accident.
    pub fn subscribe(&self) -> (mpsc::Receiver<ManifestDiff>, SubscriptionGuard) {
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::sync_channel(MANIFEST_WATCH_BUFFER);
        self.inner
            .subscribers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, tx);
        let guard = SubscriptionGuard {
            inner: Arc::clone(&self.inner),
            id,
        };
        (rx, guard)
    }

    /// Broadcast `diff` to every live subscriber. Reaps two
    /// classes of dead sender by collecting fail-ids and
    /// removing:
    /// - `TrySendError::Disconnected` — the subscriber's receiver
    ///   was dropped without the guard running (shouldn't happen
    ///   under the r2 RAII model, but kept as defense-in-depth).
    /// - `TrySendError::Full` — the subscriber is slower than the
    ///   broadcast rate; its `MANIFEST_WATCH_BUFFER`-slot channel
    ///   is full. Per the 10e-b plan §5 R2: drop on full; the
    ///   subscriber must reconnect to resume.
    ///
    /// `try_send` (not `send`) is load-bearing — `send` on a
    /// `SyncSender` BLOCKS when full, which would freeze the
    /// broadcaster (and thus whichever lock the caller is
    /// holding — typically `DaemonState`).
    pub fn broadcast(&self, diff: ManifestDiff) {
        let mut subs = self
            .inner
            .subscribers
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dead: Vec<u64> = subs
            .iter()
            .filter_map(|(id, tx)| match tx.try_send(diff.clone()) {
                Ok(()) => None,
                Err(_) => Some(*id),
            })
            .collect();
        for id in dead {
            subs.remove(&id);
        }
    }

    /// Test helper: number of sender slots tracked. As with
    /// `PtyByteFanout::subscriber_slot_count`, this counts the
    /// *tracked* slots; dead ones get reaped on the next broadcast
    /// OR on guard drop (whichever fires first).
    #[cfg(test)]
    pub(crate) fn subscriber_slot_count(&self) -> usize {
        self.inner
            .subscribers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }
}

// =====================================================================
// Persisted manifest schema (relocated from `tui/src/app.rs` in slice
// 10a-types of doc/persistent-host-daemon.md).
//
// These four structs + the retention constant define the on-disk
// shape of `~/.cm/tui-sessions.json`. They lived in `tui/src/app.rs`
// for as long as the TUI was the sole reader and writer; the daemon
// now needs to *read* the same file at startup so it can populate
// its `DaemonState`. The TUI remains the sole *writer* through
// slice 10e — relocating the schema is the first step of the
// ownership flip, not the flip itself.
//
// Every field is `pub` here because the TUI's existing in-memory
// hydration / save logic reads and writes the fields directly.
// `serde(default)` + `serde(skip_serializing_if = "Option::is_none")`
// disciplines preserve the same forward/backward-compatible wire
// shape that's been in production. The slice-9 `last_exit` and the
// in-flight forward-compat passthrough are retained verbatim.
// =====================================================================

/// Persisted record of one session. Each `ManifestWorkspace.sessions`
/// is `Vec<ManifestEntry>`; load/save go through this type.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ManifestEntry {
    /// Stable per-session UID generated at creation. Persisted (Phase 2a)
    /// so MCP env's `CM_TUI_SESSION_ID` survives TUI restart and the
    /// agent's tool calls keep authorizing. Backfill rule: missing on
    /// load → generate fresh and re-save.
    #[serde(default)]
    pub uid: String,
    /// UID of the agent session that spawned/owns this one. Used by
    /// the descendant-only auth check in Phase 3 and by sidebar
    /// "managed-by" markers later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_by_uid: Option<String>,
    /// Bumps every time `transcript_id` rebinds. Persisted so a
    /// pre-restart cursor against an old transcript correctly mismatches
    /// the post-restart generation and resets to offset 0.
    #[serde(default)]
    pub generation: u64,
    pub label: String,
    pub session_type: String,
    /// Current transcript file UUID. Older manifests stored this as
    /// `session_id`; the alias keeps backfill correct across upgrade.
    #[serde(alias = "session_id")]
    pub transcript_id: Option<String>,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub idle_timeout_secs: u16,
    #[serde(default)]
    pub burst_threshold: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_role: Option<String>,
    /// Continuous-task this session is a tick of, when spawned by
    /// the trigger funnel (DESIGN_CONTINUOUS_TASKS.md §6). `None`
    /// for every ordinary session — the funnel that sets it lands
    /// in Phase 2; this is the Phase-1 wire field only. Mirrors
    /// `workflow_run_id`'s daemon→manifest→TUI threading exactly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuous_task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default)]
    pub notify_on_idle: bool,
    /// User-assigned accent color for this session's sidebar row — a name
    /// from the TUI's `USER_COLORS` palette (e.g. "red"). Display-only:
    /// the daemon round-trips it untouched, exactly like `hidden`. `None`
    /// = inherit the workspace color / default styling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Memory-cap soft limit (bytes) this session was spawned with, when
    /// capped. P0 session durability (S2): persisted in
    /// `daemon-sessions.json` so daemon-side RESTORE can re-apply the
    /// cap when re-spawning — the cap is an argv-level `systemd-run`
    /// wrap, so it must be reconstructed, not inherited. `None` for
    /// uncapped sessions. Inert in the TUI's `tui-sessions.json` (the
    /// TUI never reads these back); `#[serde(default, skip…)]` keeps
    /// the wire shape unchanged for uncapped sessions. See
    /// DESIGN_SESSION_DURABILITY.md and
    /// [`crate::session::DaemonSession::memory_cap_soft_bytes`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_cap_soft_bytes: Option<u64>,
    /// Memory-cap hard limit (bytes). See [`Self::memory_cap_soft_bytes`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_cap_hard_bytes: Option<u64>,
    /// Parent cgroup prefix the cap scope was created under. See
    /// [`Self::memory_cap_soft_bytes`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroup_prefix: Option<PathBuf>,
    /// Global-permissions grant for this session (see
    /// `crate::session::DaemonSession::global_perms`). Persisted so
    /// a privileged orchestrator keeps its grant across TUI/daemon
    /// restart. `#[serde(default)]` keeps pre-existing manifests
    /// (no field) loading as `false` — the safe baseline.
    #[serde(default)]
    pub global_perms: bool,
    /// Name of the agent-memory snapshot this session was cloned from, if
    /// any. Informational provenance only — used to surface "Seeded from:
    /// <name>" in session info. See DESIGN_AGENT_MEMORIES.md.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seeded_from_snapshot: Option<String>,
    /// Phase-1 schema addition (slice 9 of doc/persistent-host-daemon.md).
    /// Populated by the daemon's reaper when the session transitions to
    /// exited; carries the cap-kill flag that lets the TUI render the
    /// "killed by memory cap" toast on reattach to a session that
    /// exited while the TUI was detached. `#[serde(default)]` keeps
    /// pre-Phase-1 manifests (no field present) loading cleanly with
    /// `None`. The TUI's read-modify-write save discipline preserves
    /// this field across saves (slice 12 review fix).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_exit: Option<LastExit>,
    /// Phase 3 schema addition (slice 12b of `daemon/NOTES.md`).
    /// Tags each session with the host its daemon runs on. Pre-12
    /// manifests have no field; `#[serde(default = ...)]` fills in
    /// `HostId::local()` on load, and the next save writes the
    /// upgraded shape back to disk. The TUI's load-modify-save
    /// discipline preserves this across saves the same way the
    /// slice-9 `last_exit` does.
    ///
    /// On a local-daemon manifest the value is always `local` —
    /// the daemon doesn't know its own remote name (the operator's
    /// `~/.cm/hosts.toml` is what assigns names). The field exists
    /// here so the TUI's in-memory model has a uniform type when
    /// it loads sessions from any daemon; remote-loaded sessions
    /// get their `host_id` overridden to the configured name at
    /// TUI load time (12c).
    #[serde(default = "default_local_host_id")]
    pub host_id: crate::host_id::HostId,
}

/// 12b: serde-default constructor for `ManifestEntry::host_id`.
/// `#[serde(default = "<path>")]` requires a fn item, not a
/// closure — this is the smallest one.
fn default_local_host_id() -> crate::host_id::HostId {
    crate::host_id::HostId::local()
}

/// Persisted workspace metadata. Lives in `Manifest::workspaces`
/// keyed by the workspace's stable id.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ManifestWorkspace {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub is_closed: bool,
    #[serde(default)]
    pub is_cloud: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main_repo_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_vm: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_zone: Option<String>,
    /// Host this workspace's worktree lives on (DESIGN_REMOVE_GLOBAL_HOST.md).
    /// Set once at creation; the single source of truth replacing the global
    /// `active_host`. Serde-default `local` so legacy manifests (no field)
    /// load — the TUI then derives the real host from the workspace's first
    /// session at load time.
    #[serde(default = "default_local_host_id")]
    pub host_id: crate::host_id::HostId,
    /// User-assigned accent color for this workspace — a name from the
    /// TUI's `USER_COLORS` palette. Sessions without their own `color`
    /// inherit it in the sidebar. Display-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Pinned workspaces sort to the top of the sidebar, ahead of the
    /// status-ranked rest (stable within the pinned group). Display and
    /// ordering only — no daemon behavior keys off it.
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub sessions: Vec<ManifestEntry>,
    /// Recently-closed sessions kept around so `read_session_output` can
    /// resolve a transcript path even after the session is gone. Pruned
    /// on TUI startup; see `TOMBSTONE_RETENTION_SECS`.
    #[serde(default)]
    pub tombstones: Vec<SessionTombstone>,
}

/// Lightweight record of a session that's been closed. Holds only what
/// the resolver and sidebar need; the live `TerminalSession` (which owns
/// PTY resources) is dropped at close time. Keeping the full struct
/// alive after exit would leak the PTY writer file descriptor.
///
/// **Self-contained**: every field needed to resolve `transcript_path`
/// for an `exited`-state read is on the tombstone itself, not on the
/// workspace. This matters because workspace state mutates after a
/// session closes (e.g. `push_active` clears `worktree_path` when a
/// local workspace gets uploaded to cloud). If resolution depended on
/// the workspace's *current* `worktree_path`, those tombstones would
/// silently stop resolving even though the on-disk transcript file
/// still exists at the path captured at exit time.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SessionTombstone {
    pub uid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_by_uid: Option<String>,
    pub label: String,
    /// "claude" / "codex" / "bash"
    pub session_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Last transcript file UUID this session was bound to. Used by the
    /// resolver to compute a `transcript_path` for `state: "exited"`
    /// reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transcript_id: Option<String>,
    /// Worktree path captured at exit time. Snapshot, not a live
    /// reference — survives subsequent mutations of the workspace's
    /// `worktree_path`. Required to compute Claude Code transcript
    /// paths (Codex's path scheme is per-user-and-date and ignores it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<PathBuf>,
    pub generation: u64,
    /// Unix-timestamp seconds when the session exited. Used for the
    /// retention prune.
    pub exited_at: f64,
}

/// How long tombstones live before the startup prune drops them. 30 days
/// is generous because the data is small and these are exactly the
/// records an agent might want to look at later.
pub const TOMBSTONE_RETENTION_SECS: f64 = 30.0 * 24.0 * 60.0 * 60.0;

/// Top-level on-disk shape of `~/.cm/tui-sessions.json`.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Manifest {
    /// Workspaces keyed by stable workspace id.
    #[serde(default)]
    pub workspaces: HashMap<String, ManifestWorkspace>,
    /// `task_id` → `workspace_id` bindings. A task present here is bound to
    /// the referenced workspace.
    #[serde(default)]
    pub bindings: HashMap<String, String>,
    #[serde(default)]
    pub view: Option<String>,
    /// Continuous-Tasks Phase 1: persisted `A-c` toggle that hides the
    /// continuous section (header + its sessions) from all three sidebar
    /// builders. Serde-default so old manifests without the key load as
    /// `false` (section shown).
    #[serde(default)]
    pub hide_continuous: bool,
    /// Two-column continuous panel (DESIGN_CONTINUOUS_PANEL.md): persisted
    /// `A-C` toggle. When `true` the TUI splits a dedicated continuous COLUMN
    /// off the right of the sidebar (orchestrators + nested subtasks) instead
    /// of the bottom-of-sidebar section. Serde-default `false` so old manifests
    /// load as "column off" (today's single-sidebar layout).
    #[serde(default)]
    pub continuous_column_on: bool,
    /// User-assigned accent colors for planning tasks, keyed by task id.
    /// Tasks live in the planning API rather than this manifest, so their
    /// display color rides here as a TUI-side sidecar (same palette names
    /// as the session/workspace `color` fields).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub task_colors: HashMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_exit(memory_cap_kill: bool) -> LastExit {
        LastExit {
            code: Some(137),
            memory_cap_kill,
            kills_file_offset: Some(1024),
            exited_at: 1_700_000_000.0,
        }
    }

    // --- LastExit serde -----------------------------------------------

    #[test]
    fn color_pinned_task_colors_default_and_round_trip() {
        // A pre-feature manifest (no color/pinned/task_colors keys) loads
        // with inert defaults — the back-compat contract for the fields.
        let legacy = r#"{"workspaces":{"w1":{"id":"w1","name":"n","sessions":[]}}}"#;
        let m: Manifest = serde_json::from_str(legacy).unwrap();
        let w = &m.workspaces["w1"];
        assert_eq!(w.color, None);
        assert!(!w.pinned);
        assert!(m.task_colors.is_empty());

        // Set fields survive a serialize/deserialize cycle, including the
        // per-session color.
        let mut m2 = m.clone();
        {
            let w = m2.workspaces.get_mut("w1").unwrap();
            w.color = Some("cyan".into());
            w.pinned = true;
        }
        m2.task_colors.insert("t1".into(), "red".into());
        let s = serde_json::to_string(&m2).unwrap();
        let back: Manifest = serde_json::from_str(&s).unwrap();
        let w = &back.workspaces["w1"];
        assert_eq!(w.color.as_deref(), Some("cyan"));
        assert!(w.pinned);
        assert_eq!(back.task_colors.get("t1").map(String::as_str), Some("red"));
    }

    #[test]
    fn last_exit_round_trips_through_json() {
        let original = sample_exit(true);
        let s = serde_json::to_string(&original).unwrap();
        let back: LastExit = serde_json::from_str(&s).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn last_exit_omits_none_fields_on_serialize() {
        let minimal = LastExit {
            code: None,
            memory_cap_kill: false,
            kills_file_offset: None,
            exited_at: 1_700_000_000.0,
        };
        let s = serde_json::to_string(&minimal).unwrap();
        assert!(
            !s.contains("\"code\""),
            "code:None must be omitted, not serialized as null: {}",
            s
        );
        assert!(
            !s.contains("\"kills_file_offset\""),
            "kills_file_offset:None must be omitted: {}",
            s
        );
        // memory_cap_kill is a bare bool with #[serde(default)] —
        // serializing emits it. That's fine; it's small.
    }

    #[test]
    fn last_exit_accepts_payload_with_only_required_fields() {
        // A daemon that doesn't have OS exit info yet might emit
        // just `{ "exited_at": ... }`. Defaults should fill in the
        // rest.
        let payload = r#"{ "exited_at": 1.0 }"#;
        let parsed: LastExit = serde_json::from_str(payload).unwrap();
        assert_eq!(parsed.code, None);
        assert_eq!(parsed.memory_cap_kill, false);
        assert_eq!(parsed.kills_file_offset, None);
        assert_eq!(parsed.exited_at, 1.0);
    }

    #[test]
    fn last_exit_carries_memory_cap_kill_flag_round_trip() {
        // The named acceptance criterion: the daemon writes
        // `memory_cap_kill: true` on a cap-killed exit; subscribers
        // see the same boolean after JSON round-trip.
        let exit = sample_exit(true);
        let s = serde_json::to_string(&exit).unwrap();
        assert!(s.contains("\"memory_cap_kill\":true"));
        let back: LastExit = serde_json::from_str(&s).unwrap();
        assert!(back.memory_cap_kill);
    }

    // --- ManifestWatcher ---------------------------------------------

    #[test]
    fn subscribe_then_broadcast_delivers_to_one() {
        let w = ManifestWatcher::new();
        let (rx, _guard) = w.subscribe();
        let diff = ManifestDiff::Added {
            uid: "ts-1".into(),
            entry: serde_json::json!({ "label": "x" }),
        };
        w.broadcast(diff.clone());
        let got = rx.try_recv().expect("delivered");
        assert_eq!(got, diff);
    }

    #[test]
    fn multiple_subscribers_all_receive_same_diff() {
        let w = ManifestWatcher::new();
        let (rx1, _g1) = w.subscribe();
        let (rx2, _g2) = w.subscribe();
        let diff = ManifestDiff::Tombstoned {
            uid: "ts-1".into(),
            exited_at: 1.0,
        };
        w.broadcast(diff.clone());
        assert_eq!(rx1.try_recv().unwrap(), diff);
        assert_eq!(rx2.try_recv().unwrap(), diff);
    }

    #[test]
    fn subscribe_after_broadcast_does_not_replay() {
        // Per the type comment: this primitive doesn't buffer.
        // Subscribers only see diffs broadcast *after* they
        // subscribe. The eventual JSON-RPC handler composes the
        // snapshot separately.
        let w = ManifestWatcher::new();
        let diff = ManifestDiff::Added {
            uid: "ts-1".into(),
            entry: Value::Null,
        };
        w.broadcast(diff.clone());
        let (rx, _guard) = w.subscribe();
        assert!(rx.try_recv().is_err(), "no replay; subscribe was after broadcast");
    }

    #[test]
    fn dropped_subscriber_is_reaped_on_next_broadcast() {
        // 10e-b r2: this test now exercises the broadcaster-side
        // defense-in-depth (try_send → Disconnected → remove). In
        // production the RAII guard would reap first; here we
        // intentionally drop just the receiver while keeping the
        // guard alive, to drive the legacy reap path.
        let w = ManifestWatcher::new();
        let (_rx_alive, _g_alive) = w.subscribe();
        let (rx_dropping, _g_dropping) = w.subscribe();
        drop(rx_dropping);
        // Both slots still present (guards alive).
        assert_eq!(w.subscriber_slot_count(), 2);

        w.broadcast(ManifestDiff::Tombstoned {
            uid: "ts-1".into(),
            exited_at: 0.0,
        });
        assert_eq!(
            w.subscriber_slot_count(),
            1,
            "dead subscriber must be reaped during broadcast"
        );
    }

    #[test]
    fn memory_cap_kill_propagates_through_exited_diff() {
        // End-to-end: the doc's detached-session acceptance criterion
        // is that a memory-cap kill surfaces via the manifest.watch
        // exit diff with `memory_cap_kill: true`. Prove the
        // primitive transports it byte-identically.
        let w = ManifestWatcher::new();
        let (rx, _guard) = w.subscribe();
        let last_exit = LastExit {
            code: Some(137),
            memory_cap_kill: true,
            kills_file_offset: Some(0),
            exited_at: 1_700_000_000.0,
        };
        w.broadcast(ManifestDiff::Exited {
            uid: "ts-cap-killed".into(),
            last_exit: last_exit.clone(),
        });

        match rx.try_recv().expect("delivered") {
            ManifestDiff::Exited { uid, last_exit: got } => {
                assert_eq!(uid, "ts-cap-killed");
                assert!(got.memory_cap_kill);
                assert_eq!(got, last_exit);
            }
            other => panic!("expected Exited, got {:?}", other),
        }
    }

    #[test]
    fn broadcast_order_matches_send_order() {
        let w = ManifestWatcher::new();
        let (rx, _guard) = w.subscribe();

        w.broadcast(ManifestDiff::Added {
            uid: "a".into(),
            entry: Value::Null,
        });
        w.broadcast(ManifestDiff::Updated {
            uid: "a".into(),
            entry: Value::Null,
        });
        w.broadcast(ManifestDiff::Exited {
            uid: "a".into(),
            last_exit: sample_exit(false),
        });

        let order: Vec<_> = (0..3).map(|_| rx.try_recv().unwrap()).collect();
        assert!(matches!(order[0], ManifestDiff::Added { .. }));
        assert!(matches!(order[1], ManifestDiff::Updated { .. }));
        assert!(matches!(order[2], ManifestDiff::Exited { .. }));
    }

    /// T11 (10e-b r2) — RAII guard reaps the subscriber slot
    /// immediately on Drop, NOT on the next broadcast. Pre-r2 a
    /// disconnected handler's slot lived in the broadcaster's
    /// vec until the next broadcast triggered `retain`; T11
    /// asserts the slot is gone the instant the guard drops, no
    /// follow-up broadcast required.
    #[test]
    fn guard_drop_immediately_reaps_subscriber_slot() {
        let w = ManifestWatcher::new();
        let (_rx, guard) = w.subscribe();
        assert_eq!(
            w.subscriber_slot_count(),
            1,
            "subscribe inserts exactly one slot",
        );

        drop(guard);

        // No broadcast between drop and assertion. Pre-r2 the
        // slot would still be present here; post-r2 the guard's
        // Drop already called `unsubscribe`.
        assert_eq!(
            w.subscriber_slot_count(),
            0,
            "guard Drop MUST immediately reap the slot (no \
             follow-up broadcast required)",
        );
    }

    /// T12 (10e-b r2) — repeated connect/disconnect cycles in
    /// an idle daemon must NOT accumulate slots. Pre-r2 a quiet
    /// daemon that saw 100 connect/disconnect pairs would have
    /// 100 dead slots until the next real broadcast; r2's RAII
    /// guard bounds the slot count regardless of broadcast rate.
    #[test]
    fn repeated_connect_disconnect_does_not_accumulate_slots() {
        let w = ManifestWatcher::new();
        for _ in 0..100 {
            // Subscribe → immediately drop both rx and guard.
            // Each iteration's guard reaps its slot before the
            // next iteration's subscribe runs.
            let (_rx, _guard) = w.subscribe();
        }
        assert_eq!(
            w.subscriber_slot_count(),
            0,
            "100 subscribe-drop cycles MUST leave zero slots — \
             RAII guard reaps each one on drop, no broadcast \
             needed to trigger cleanup",
        );
    }

    // ---------------------------------------------------------------
    // Slice 12b (Phase 3): `host_id` on ManifestEntry — serde
    // back-compat for pre-12 manifests, explicit-value round-trip,
    // and byte-stability post-upgrade.
    // ---------------------------------------------------------------

    /// T_g3b_pre_12_manifest_load — a manifest written before
    /// slice 12b had no `host_id` field at all. The
    /// `#[serde(default = "default_local_host_id")]` attr must
    /// fill in `HostId::local()` so the load doesn't error and
    /// the in-memory model has a uniform type.
    ///
    /// Pre-fix (no default): serde would reject the missing
    /// field and the whole manifest load would fail — the TUI
    /// would lock out the operator from every pre-12 session.
    #[test]
    fn t_g3b_pre_12_manifest_load() {
        // Pre-12 wire shape — no host_id field anywhere.
        let pre_12_json = r#"{
            "uid": "ts-pre-12",
            "managed_by_uid": null,
            "generation": 0,
            "label": "old-session",
            "session_type": "claude-code",
            "transcript_id": null,
            "hidden": false,
            "idle_timeout_secs": 0,
            "burst_threshold": 0,
            "task_id": null,
            "notify_on_idle": false
        }"#;
        let entry: ManifestEntry = serde_json::from_str(pre_12_json)
            .expect("pre-12 manifest must load via #[serde(default)]");
        assert_eq!(
            entry.host_id,
            crate::host_id::HostId::local(),
            "missing host_id field must default to local",
        );
        // Sanity: every other pre-12 field round-trips.
        assert_eq!(entry.uid, "ts-pre-12");
        assert_eq!(entry.label, "old-session");
        assert_eq!(entry.session_type, "claude-code");
    }

    /// T_g3b_explicit_host_id — a post-12 manifest with an
    /// explicit non-local `host_id` round-trips byte-for-byte.
    /// Pins the wire shape so future field-addition slices
    /// (e.g. 12h's `tls_fingerprint`) can't silently shift the
    /// encoding.
    #[test]
    fn t_g3b_explicit_host_id() {
        let entry = ManifestEntry {
            color: None,
            memory_cap_soft_bytes: None,
            memory_cap_hard_bytes: None,
            cgroup_prefix: None,
            uid: "ts-12b-mgr".into(),
            managed_by_uid: None,
            generation: 0,
            label: "manager-session".into(),
            session_type: "claude-code".into(),
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
            host_id: crate::host_id::HostId::new("manager"),
            global_perms: false,
        };
        let s = serde_json::to_string(&entry).expect("serialize");
        assert!(
            s.contains(r#""host_id":"manager""#),
            "explicit host_id must serialize as a bare string: {}",
            s,
        );
        let back: ManifestEntry =
            serde_json::from_str(&s).expect("round-trip");
        assert_eq!(back.host_id, crate::host_id::HostId::new("manager"));
        assert_eq!(back.uid, entry.uid);
        assert_eq!(back.label, entry.label);
    }

    /// T_g3b_serde_byte_stable_after_upgrade — load a pre-12
    /// manifest, save it, reload, and assert the reload-then-save
    /// is byte-identical to the first save. Catches the
    /// upgrade-once + idempotent-thereafter contract: the
    /// `#[serde(default)]` fill happens ONCE (on the load that
    /// sees the missing field), the next save writes the field
    /// explicitly, and from then on the encoding is stable.
    ///
    /// Without this, a pre-12 file would get re-upgraded on every
    /// load — sometimes triggering save churn even when nothing
    /// else changed.
    #[test]
    fn t_g3b_serde_byte_stable_after_upgrade() {
        // Pre-12 JSON without host_id.
        let pre_12_json = r#"{"uid":"ts","managed_by_uid":null,"generation":0,"label":"x","session_type":"claude","transcript_id":null,"hidden":false,"idle_timeout_secs":0,"burst_threshold":0,"task_id":null,"notify_on_idle":false}"#;

        // First load: serde fills `host_id = HostId::local()`.
        let loaded: ManifestEntry =
            serde_json::from_str(pre_12_json).expect("load");
        assert_eq!(loaded.host_id, crate::host_id::HostId::local());

        // Save: the entry now writes host_id explicitly.
        let after_first_save =
            serde_json::to_string(&loaded).expect("save");
        assert!(
            after_first_save.contains(r#""host_id":"local""#),
            "first save must persist host_id explicitly so the \
             next load doesn't re-trigger the default fill: {}",
            after_first_save,
        );

        // Reload from the saved bytes.
        let reloaded: ManifestEntry =
            serde_json::from_str(&after_first_save).expect("reload");

        // Save again — must be byte-identical to the first save.
        let after_second_save =
            serde_json::to_string(&reloaded).expect("save 2");
        assert_eq!(
            after_first_save, after_second_save,
            "post-upgrade saves must be byte-stable — the first \
             save baked host_id into the encoding, so the second \
             read+write is a no-op-default-free round-trip",
        );
    }

    /// Continuous-Tasks Phase 1: the `continuous_task_id` wire field
    /// threads through the manifest with the pinned serde shape.
    /// A manifest written before the field existed must load with
    /// `None` (serde default), and a None value must NOT serialize
    /// (skip_serializing_if) so old/new encodings stay compatible.
    /// A continuous-tagged entry round-trips the id intact.
    #[test]
    fn t_continuous_task_id_serde_default_and_roundtrip() {
        // Pre-continuous JSON (no continuous_task_id key) → None.
        let pre_json = r#"{"uid":"ts","managed_by_uid":null,"generation":0,"label":"x","session_type":"claude","transcript_id":null,"hidden":false,"idle_timeout_secs":0,"burst_threshold":0,"task_id":null,"notify_on_idle":false}"#;
        let loaded: ManifestEntry =
            serde_json::from_str(pre_json).expect("load");
        assert_eq!(loaded.continuous_task_id, None);

        // None must not appear in the encoding (skip_serializing_if).
        let none_encoded = serde_json::to_string(&loaded).expect("save");
        assert!(
            !none_encoded.contains("continuous_task_id"),
            "a None continuous_task_id must be omitted: {}",
            none_encoded,
        );

        // A continuous-tagged entry round-trips the id.
        let tagged = ManifestEntry {
            color: None,
            memory_cap_soft_bytes: None,
            memory_cap_hard_bytes: None,
            cgroup_prefix: None,
            uid: "ts-cont".into(),
            managed_by_uid: None,
            generation: 0,
            label: "triage-tick".into(),
            session_type: "claude-code".into(),
            transcript_id: None,
            hidden: false,
            idle_timeout_secs: 0,
            burst_threshold: 0,
            workflow_run_id: None,
            workflow_role: None,
            continuous_task_id: Some("bug-triage".into()),
            task_id: None,
            notify_on_idle: false,
            seeded_from_snapshot: None,
            last_exit: None,
            host_id: crate::host_id::HostId::local(),
            global_perms: false,
        };
        let s = serde_json::to_string(&tagged).expect("serialize");
        assert!(
            s.contains(r#""continuous_task_id":"bug-triage""#),
            "tagged entry must serialize the id: {}",
            s,
        );
        let back: ManifestEntry =
            serde_json::from_str(&s).expect("round-trip");
        assert_eq!(back.continuous_task_id.as_deref(), Some("bug-triage"));
    }

    /// DESIGN_REMOVE_GLOBAL_HOST.md Phase A: `ManifestWorkspace.host_id`
    /// round-trips, and a legacy workspace (no `host_id` key) loads as `local`
    /// (serde default) — the TUI then derives the real host from the
    /// workspace's first session.
    #[test]
    fn manifest_workspace_host_id_round_trips_and_legacy_defaults_local() {
        let mut ws = ManifestWorkspace {
            id: "ws".into(),
            host_id: crate::host_id::HostId("manager".into()),
            ..Default::default()
        };
        ws.name = "n".into();
        let json = serde_json::to_string(&ws).expect("serialize");
        assert!(json.contains(r#""host_id":"manager""#), "host_id serialized: {}", json);
        let back: ManifestWorkspace = serde_json::from_str(&json).expect("round-trip");
        assert_eq!(back.host_id, crate::host_id::HostId("manager".into()));

        // Legacy workspace JSON with no host_id key → defaults to local.
        let legacy: ManifestWorkspace =
            serde_json::from_str(r#"{"id":"ws","name":"n"}"#).expect("legacy load");
        assert_eq!(legacy.host_id, crate::host_id::HostId::local());
    }
}
