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

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::attach::TicketAllocator;
use crate::manifest::{Manifest, ManifestWorkspace};
use crate::session::DaemonSession;

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
}

impl Default for DaemonState {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
            workspaces: HashMap::new(),
            bindings: HashMap::new(),
            tickets: TicketAllocator::new(),
            attach_addr: String::new(),
        }
    }
}

impl DaemonState {
    pub fn new() -> Self {
        Self::default()
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
            task_id: None,
            notify_on_idle: false,
            seeded_from_snapshot: None,
            last_exit: None,
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
