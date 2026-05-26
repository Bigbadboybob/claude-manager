//! Slice 12c: per-host RPC connection pool.
//!
//! Owns the host_id → socket-path lookup that every RPC call site
//! routes through. Replaces the previous `cm_daemon::default_socket_path()`
//! direct reads scattered across the TUI's runtime-dial sites
//! (`tui/src/workflow_watch.rs`, `tui/src/manifest_watch.rs`,
//! various per-session RPC sites in `tui/src/app.rs`).
//!
//! The pool is build-once-at-App::new, read-many. The TUI is
//! single-threaded outside the consumer threads, which receive
//! their host's path at spawn time and don't re-read the pool;
//! no concurrent mutation surface.
//!
//! ## Local-only at this slice
//!
//! Only `HostTransport::Unix` entries are reachable for live dial
//! through 12c. `HostTransport::SshUnix` entries are accepted
//! into the pool (so a user can have a `manager` entry in
//! `hosts.toml` and the TUI doesn't fail to launch), but their
//! socket path points at the SSH-tunnel-local end
//! (`/tmp/cm-host-<name>.sock`) which doesn't exist until 12d
//! spawns the tunnel. Callers that dial an SshUnix entry today
//! get a connect error — that's the right user-facing message
//! ("no such socket") until 12d enables the transport.
//!
//! `HostTransport::TcpTls` entries are unreachable here because
//! `HostsConfig::load` (slice 12a) rejects them at load time.
//!
//! ## Why the abstraction is just a path
//!
//! 12d will add SSH-process lifecycle (spawn, stderr capture,
//! Drop-time unlink). 12h will add TLS handshake + fingerprint
//! pinning. Both will need ConnectionHandle to grow methods
//! beyond `socket_path()`. For 12c, every transport that's
//! actually dial-able is Unix-shaped (raw local socket or
//! ssh-tunnel-mediated local socket), and every consumer of the
//! handle dials a `UnixStream`. Keeping the handle a thin
//! path-wrapper now means 12d/12h can extend it without breaking
//! the 12c call-site refactor.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use cm_daemon::host_id::HostId;

use crate::hosts::{HostConfig, HostTransport, HostsConfig};

/// One pool entry. Today (12c) carries just the socket path the
/// caller dials. 12d will add SSH-tunnel-lifecycle fields; 12h
/// will add a `TlsConfig` variant. Keeping the type small lets
/// the call-site refactor be mechanical.
#[derive(Clone, Debug)]
pub struct ConnectionHandle {
    /// Filesystem path of the Unix socket to dial.
    ///
    /// - `HostTransport::Unix` → the daemon's own socket
    ///   (typically `~/.cm/daemon.sock`).
    /// - `HostTransport::SshUnix` → the LOCAL end of the
    ///   `ssh -L` tunnel (typically `/tmp/cm-host-<name>.sock`).
    ///   The path is deterministic from the host's name even
    ///   though the socket itself doesn't exist until 12d spawns
    ///   the ssh child. Storing the path now means call sites
    ///   that dial it today (and fail with "no such socket"
    ///   until 12d) have a stable path to surface in error
    ///   messages.
    pub socket_path: PathBuf,
}

impl ConnectionHandle {
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

/// `host_id` → `ConnectionHandle` pool. Built once at `App::new`
/// from `HostsConfig`; immutable thereafter.
#[derive(Clone, Debug)]
pub struct HostPool {
    entries: HashMap<HostId, ConnectionHandle>,
    /// The default host's id, captured from the `default = true`
    /// entry in `HostsConfig`. Used by `default_handle()` so
    /// callers that don't care about per-session routing (TUI-
    /// level pushes like `task.update_tree`) get a sensible
    /// target without each site having to look up the default
    /// themselves.
    ///
    /// `HostsConfig::validate` guarantees exactly one default,
    /// so this field is always populated after `from_config`.
    default_host_id: HostId,
}

impl HostPool {
    /// Build the pool from a validated `HostsConfig`. Panics if
    /// no entry is marked `default` — that's a `HostsConfig`
    /// invariant (`validate` rejects), so reaching this from
    /// production is a bug.
    pub fn from_config(cfg: &HostsConfig) -> Self {
        let mut entries: HashMap<HostId, ConnectionHandle> = HashMap::new();
        let mut default_host_id: Option<HostId> = None;
        for host in &cfg.hosts {
            let socket_path = socket_path_for(host);
            entries.insert(
                host.id.clone(),
                ConnectionHandle { socket_path },
            );
            if host.default {
                default_host_id = Some(host.id.clone());
            }
        }
        let default_host_id = default_host_id.expect(
            "HostsConfig::validate guarantees exactly one default — \
             reaching from_config with no default is a 12a invariant bug",
        );
        HostPool {
            entries,
            default_host_id,
        }
    }

    /// Lookup by host_id. `None` for an unknown id — typically
    /// indicates a manifest entry pointing at a host that's no
    /// longer in `hosts.toml` (user removed the entry between
    /// runs). Callers decide whether that's a hard error or a
    /// "fall back to default" prompt.
    pub fn for_host(&self, host_id: &HostId) -> Option<&ConnectionHandle> {
        self.entries.get(host_id)
    }

    /// Lookup the default host's handle. Used by TUI-level
    /// pushes that aren't tied to a specific session
    /// (`task.update_tree`, `workflow.update_definitions`,
    /// `tui.update_sessions_snapshot`). For session-bound calls
    /// (`rpc_kill_session`, `rpc_set_transcript_path`, etc.),
    /// prefer `for_host(&ts.host_id)`.
    pub fn default_handle(&self) -> &ConnectionHandle {
        self.entries
            .get(&self.default_host_id)
            .expect("default_host_id always in entries (from_config invariant)")
    }

    /// Test helper: number of entries in the pool. Used by
    /// `T_g3c_pool_per_host_id` to confirm host-distinct
    /// configurations produce distinct pool sizes.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Resolve a `HostConfig` to the path its `ConnectionHandle`
/// should dial. Split out so the SSH-tunnel path-derivation
/// (12d) and the TLS-addr-derivation (12h) can plug in here.
fn socket_path_for(host: &HostConfig) -> PathBuf {
    match &host.transport {
        HostTransport::Unix { socket } => socket.clone(),
        HostTransport::SshUnix { .. } => {
            // 12d will spawn `ssh -L <local>:<remote_socket>`
            // and bind to this local path. Until then the path
            // is just a placeholder — callers that try to dial
            // get "no such file" which surfaces as a clear
            // pre-12d error.
            PathBuf::from(format!("/tmp/cm-host-{}.sock", host.id.as_str()))
        }
        HostTransport::TcpTls { .. } => {
            // Unreachable: `HostsConfig::load` (12a) rejects
            // TcpTls with `Error::TlsNotImplemented`. Defensive
            // fallback to default_socket_path so a future
            // regression in 12a (silent TcpTls accept) doesn't
            // panic the pool at construction; the resulting
            // entry won't dial successfully, but the TUI stays
            // up.
            cm_daemon::default_socket_path()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hosts::HostsConfig;

    /// T_g3c_pool_per_host_id — the pool builds one
    /// ConnectionHandle per HostsConfig entry, keyed by HostId.
    /// Same host_id returns the same handle (by-value equality
    /// on the underlying path); distinct host_ids return
    /// distinct handles.
    #[test]
    fn t_g3c_pool_per_host_id() {
        // Build a HostsConfig with two distinct hosts.
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let orig_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }
        let path = tmp.path().join("hosts.toml");
        std::fs::write(
            &path,
            r#"
[[host]]
name = "local"
transport = "unix"
socket = "/tmp/local.sock"
default = true

[[host]]
name = "manager"
transport = "ssh-unix"
ssh_host = "cm-manager"
remote_socket = "/home/lucas/.cm/daemon.sock"
"#,
        )
        .expect("write hosts.toml");
        let cfg = HostsConfig::load(&path).expect("load");
        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        let pool = HostPool::from_config(&cfg);
        assert_eq!(pool.len(), 2, "one entry per host");

        // Per-host lookup.
        let local = pool
            .for_host(&HostId::local())
            .expect("local must be in pool");
        let manager = pool
            .for_host(&HostId::new("manager"))
            .expect("manager must be in pool");

        // Local: socket path from the config.
        assert_eq!(
            local.socket_path(),
            std::path::Path::new("/tmp/local.sock"),
        );
        // Manager (ssh-unix): derived tunnel-local path.
        assert_eq!(
            manager.socket_path(),
            std::path::Path::new("/tmp/cm-host-manager.sock"),
        );

        // Distinct host_ids → distinct handles (path-equality
        // is the discriminator — handles are by-value structs).
        assert_ne!(local.socket_path(), manager.socket_path());

        // Same host_id returns equivalent handle on every call.
        let local_again = pool.for_host(&HostId::local()).unwrap();
        assert_eq!(local.socket_path(), local_again.socket_path());

        // Default handle resolves to local (the entry marked
        // `default = true`).
        let default = pool.default_handle();
        assert_eq!(default.socket_path(), local.socket_path());

        // Unknown host_id → None.
        assert!(pool.for_host(&HostId::new("nope")).is_none());
    }

    /// Synthesized-default config builds a single-entry pool
    /// with the local-host path matching
    /// `cm_daemon::default_socket_path()`. Folds in the
    /// "local behavior unchanged" half of T_g3c — the routing
    /// layer (HostPool) defers to the same path resolution the
    /// pre-12c code used directly.
    #[test]
    fn synthesized_default_pool_local_path_matches_daemon_default() {
        let _guard = crate::test_support::home_lock();
        let cfg = HostsConfig::synthesized_local_default();
        let pool = HostPool::from_config(&cfg);
        assert_eq!(pool.len(), 1);
        let handle = pool.default_handle();
        assert_eq!(
            handle.socket_path(),
            &cm_daemon::default_socket_path(),
            "pool's local-host socket path MUST match the canonical \
             cm_daemon::default_socket_path() so refactored call sites \
             dial the same socket they used to dial directly",
        );
    }

    /// T_g3c_local_behavior_byte_stable — daemon's events.jsonl
    /// write path is unchanged by 12c, but we pin the byte-shape
    /// here so a future Phase 3 slice that accidentally touches
    /// `WorkflowEventsWriter` (e.g. via an env-injection refactor)
    /// regresses this test rather than silently changing the wire
    /// format the TUI consumer reads.
    ///
    /// Reviewer-round note from the slice plan: events carry
    /// `ts: f64` from `now_unix_f64()`, so strict byte-compare
    /// diverges on every run. Recommended approach (i) — filter
    /// `ts` before compare. Implemented here: write an event,
    /// read it back, deserialize, blank out `ts`, compare the
    /// remaining fields against a known-good Value.
    ///
    /// The test does NOT spin up agents or workflows — that would
    /// be a true integration-level setup. The spec said "drive a
    /// feedback workflow," but the meaningful invariant is the
    /// daemon's write-side JSON shape, which is set by
    /// `WorkflowEventsWriter::append_event` regardless of how the
    /// event was produced. Calling it directly with a synthesized
    /// `Event` exercises the same code path.
    #[test]
    fn t_g3c_local_behavior_byte_stable() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let orig_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        // Pre-12c routing path: read `cm_daemon::default_socket_path()`
        // directly. Post-12c routing path: build a synthesized-
        // default HostPool, take its default handle's
        // `socket_path()`. Both MUST resolve to the same path
        // under the same $HOME — this is the structural
        // invariant the 12c refactor preserves.
        let pre_12c_path = cm_daemon::default_socket_path();
        let cfg = HostsConfig::synthesized_local_default();
        let pool = HostPool::from_config(&cfg);
        let post_12c_path =
            pool.default_handle().socket_path().to_path_buf();
        assert_eq!(
            pre_12c_path, post_12c_path,
            "12c refactor MUST be socket-path-stable for the local \
             host: pre-12c direct-read and post-12c pool-route \
             return identical paths",
        );

        // Daemon-side events.jsonl write path is unchanged by
        // 12c. Drive the writer directly with a deterministic
        // Event; read the resulting JSON; assert shape.
        let run_id = "wf_g3c_byte_stable";
        let event = cm_daemon::workflow::events::Event {
            id: "evt-12c-byte-stable".to_string(),
            // ts will be ignored in the comparison below;
            // value here is irrelevant.
            ts: 0.0,
            run_id: run_id.to_string(),
            role: "worker".to_string(),
            tool: "workflow_transition".to_string(),
            args: serde_json::json!({"to": "reviewer", "prompt": "p"}),
            source: "daemon".to_string(),
            from_role: Some("worker".to_string()),
            iteration: 2,
        };
        cm_daemon::workflow::events::WorkflowEventsWriter::append_event(
            &event,
        )
        .expect("append_event");

        // Read the resulting JSONL line.
        let events_path =
            cm_daemon::workflow::run::events_path(run_id);
        let raw = std::fs::read_to_string(&events_path)
            .expect("read events.jsonl");
        let line = raw.trim();
        assert!(
            !line.is_empty(),
            "events.jsonl must have a record after append_event",
        );

        // Parse into a generic Value, strip `ts`, compare against
        // a known-good shape. Reviewer-round (i) approach: filter
        // `ts` because `now_unix_f64()` makes it non-deterministic;
        // the operationally-meaningful invariant is every other
        // field staying stable.
        let mut got: serde_json::Value = serde_json::from_str(line)
            .expect("events.jsonl line parses as JSON");
        if let Some(obj) = got.as_object_mut() {
            obj.remove("ts");
        }
        let expected = serde_json::json!({
            "id": "evt-12c-byte-stable",
            "run_id": "wf_g3c_byte_stable",
            "role": "worker",
            "tool": "workflow_transition",
            "args": {"to": "reviewer", "prompt": "p"},
            "source": "daemon",
            "from_role": "worker",
            "iteration": 2
        });
        assert_eq!(
            got, expected,
            "events.jsonl JSON shape (excluding ts) MUST match the \
             pre-12c golden — the host_pool refactor is TUI-side \
             only; the daemon's write path is structurally \
             unchanged, and this test pins that invariant against \
             future Phase 3 slices that might accidentally touch \
             the writer",
        );

        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }
}
