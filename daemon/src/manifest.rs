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
use std::sync::mpsc;

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
#[derive(Debug, Clone, PartialEq)]
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

/// Multi-subscriber broadcaster for [`ManifestDiff`] events.
///
/// Same shape as [`crate::session::PtyByteFanout`] but without a
/// replay buffer — manifest watchers care about live events plus an
/// initial snapshot, which the eventual `manifest.watch` handler
/// composes from a `list_current()` step plus subscription rather
/// than a ring buffer.
pub struct ManifestWatcher {
    subscribers: Mutex<Vec<mpsc::Sender<ManifestDiff>>>,
}

impl Default for ManifestWatcher {
    fn default() -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
        }
    }
}

impl ManifestWatcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new subscriber. The returned receiver yields every
    /// diff broadcast *after* this call returns. (Initial-snapshot
    /// replay is composed by the eventual JSON-RPC handler; this
    /// primitive only handles live events.)
    pub fn subscribe(&self) -> mpsc::Receiver<ManifestDiff> {
        let (tx, rx) = mpsc::channel();
        self.subscribers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(tx);
        rx
    }

    /// Broadcast `diff` to every live subscriber. Dead subscribers
    /// (whose receiver was dropped) are reaped here via `retain` on
    /// the sender list — same lifecycle as `PtyByteFanout::push`.
    pub fn broadcast(&self, diff: ManifestDiff) {
        let mut subs = self
            .subscribers
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        subs.retain(|tx| tx.send(diff.clone()).is_ok());
    }

    /// Test helper: number of sender slots tracked. As with
    /// `PtyByteFanout::subscriber_slot_count`, this counts the
    /// *tracked* slots; dead ones get reaped on the next broadcast.
    #[cfg(test)]
    fn subscriber_slot_count(&self) -> usize {
        self.subscribers
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default)]
    pub notify_on_idle: bool,
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
        let rx = w.subscribe();
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
        let rx1 = w.subscribe();
        let rx2 = w.subscribe();
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
        let rx = w.subscribe();
        assert!(rx.try_recv().is_err(), "no replay; subscribe was after broadcast");
    }

    #[test]
    fn dropped_subscriber_is_reaped_on_next_broadcast() {
        let w = ManifestWatcher::new();
        let _rx_alive = w.subscribe();
        let rx_dropping = w.subscribe();
        drop(rx_dropping);
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
        let rx = w.subscribe();
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
        let rx = w.subscribe();

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
}
