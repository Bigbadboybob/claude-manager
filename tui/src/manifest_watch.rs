//! Slice 10e-c: TUI-side consumer for the daemon's `manifest.watch`
//! streaming RPC (landed in 10e-b at commit `c6d20d8`).
//!
//! Wire shape (matches `daemon/src/control/dispatch.rs::
//! dispatch_manifest_watch` + `daemon/src/control/stream.rs::
//! handle_manifest_watch_stream`):
//!
//!   1. Dial the daemon socket.
//!   2. Send `manifest.watch` JSON-RPC request (operator caller,
//!      empty params).
//!   3. Read OK response `{ "subscribed": true }`.
//!   4. The same socket transitions to a one-way streaming mode.
//!      Frames are length-prefixed `cm_daemon::control::wire::
//!      StreamFrame` JSON. Kinds we observe:
//!        - `ManifestSnapshot` (sent once, first): payload carries
//!          `{workspaces, bindings}` — ignored in 10e-c (TUI loaded
//!          the same disk state at startup; subsequent diffs are
//!          the novel info). 10e-d may reconcile.
//!        - `ManifestDiff`: payload is a variant-tagged
//!          `cm_daemon::manifest::ManifestDiff` JSON. Deserialized
//!          and forwarded to the main loop via mpsc.
//!        - `Heartbeat`: liveness probe. Silent no-op consumer-side.
//!        - Any other kind: a wire-protocol bug; log + drop the
//!          frame, keep reading.
//!
//! Lifecycle:
//!   - Spawned at `App::new` unconditionally — 10f flipped the
//!     daemon-mode default and the consumer now always runs in
//!     production. [`should_spawn`] is retained as the seam tests
//!     use to skip the consumer in synthetic-App constructions.
//!   - Owns a reconnect loop with exponential backoff (1s → 30s).
//!     Daemon restart, socket-not-yet-bound, transient network
//!     blips: the consumer retries with bounded backoff. On
//!     successful reconnect the daemon's `manifest.watch` returns
//!     a fresh snapshot — the TUI's in-memory state stays
//!     authoritative; missed mutations during the disconnect
//!     window flow through later diffs / the next save's
//!     read-from-disk reconciliation. Each successful
//!     (re)subscription additionally emits
//!     [`ManifestEvent::StreamEstablished`] (before any frame),
//!     which the App uses to invalidate the push worker's
//!     de-dupe cache for that host and re-prime a possibly
//!     re-exec'd daemon (seamless-restart audit gap 3).
//!   - Exits its outer loop (no reconnect) on `SendError`. That
//!     means the main loop's `Receiver` was dropped — the App is
//!     gone, no reason to keep dialing.
//!
//! The consumer thread NEVER touches `App` directly — all
//! communication flows through the `mpsc::Sender<ManifestDiff>`
//! into the main loop's `drain_manifest_watch_events`, which
//! mutates `App` under the single-thread invariant the rest of
//! the TUI relies on.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use cm_daemon::control::protocol::{Caller, CallerOperator, Request, StreamKind};
use cm_daemon::control::wire;
use cm_daemon::host_id::HostId;
use cm_daemon::manifest::{LastExit, ManifestDiff};

/// 10e-c r1 F1: post-disconnect snapshot reconciliation payload.
/// Each entry is one session's uid + that session's
/// `last_exit` (`None` if the daemon doesn't have one for it).
///
/// Why this shape vs. the raw daemon snapshot JSON: the TUI's
/// `apply_manifest_snapshot` consumes per-uid `last_exit` (the F1
/// conservative merge) AND the set of uids the daemon still lists,
/// against which agent-spawned rows are reconciled (pruned when the
/// host no longer holds them — the catch-up for exit diffs missed
/// while the stream was down). The rest of the daemon's snapshot
/// (workspaces, bindings) is loaded from disk at TUI startup and
/// isn't reconciled here. Flattening keeps the App-side apply simple.
#[derive(Debug, Clone, PartialEq)]
pub struct ManifestSnapshotPayload {
    /// Host whose `manifest.watch` stream produced this snapshot. The
    /// App-side reconcile only touches rows tagged with this host.
    pub host: HostId,
    /// `(uid, last_exit)` pairs extracted from the daemon's
    /// snapshot's `workspaces[*].sessions[*]`. Order is
    /// daemon-iteration order (HashMap of workspaces × Vec of
    /// sessions); the App-side apply walks this and updates
    /// `preserved_last_exit` conservatively per the F1 merge
    /// rule (only when local is None).
    pub session_last_exits: Vec<(String, Option<LastExit>)>,
    /// EVERY session uid the snapshot lists (a superset of
    /// `session_last_exits`' uids — an entry whose `last_exit` failed to
    /// parse is skipped there but still counts as "listed" here). The
    /// reconcile in `App::apply_manifest_snapshot` prunes agent-spawned
    /// rows on `host` that are NOT in this set: the daemon no longer
    /// holds them, so every exit diff we missed while the stream was
    /// down (tunnel drop, daemon re-exec, token rejection) is caught up
    /// in one pass.
    pub listed_uids: Vec<String>,
    /// When the consumer thread read the frame. Rows created locally
    /// after (or just before) this instant may be newer than the
    /// daemon's capture and are exempt from the reconcile prune.
    pub received_at: std::time::Instant,
}

/// 10e-c r1 F1: typed event the consumer sends to the main loop.
/// Diff and Snapshot are mutually exclusive at the wire-frame
/// level; one event variant per frame.
///
/// Channel was `Sender<ManifestDiff>` pre-r1; r1 widened to this
/// enum to surface the snapshot reconciliation step in the
/// type system. Pre-r1 the snapshot was silently dropped on the
/// consumer side, which clobbered the daemon's last_exit on
/// reconnect (the F1 finding).
#[derive(Debug, Clone)]
pub enum ManifestEvent {
    /// Seamless-restart audit gap 3: the consumer (re)established
    /// its stream to `host` — sent exactly once per successful
    /// `connect_and_subscribe`, before any Snapshot/Diff frame
    /// from the new stream, and never on a failed dial/retry.
    ///
    /// The App's drain uses it to invalidate the push worker's
    /// payload-hash cache for that host and re-fire a full state
    /// push: a daemon re-exec kills every `manifest.watch` stream
    /// at the exec, and the swapped daemon's TUI-pushed state
    /// (`task_tree` / `tui_sessions` / `workflow_definitions`) is
    /// born empty — without the invalidation, the de-dupe would
    /// keep skipping the unchanged payloads and an idle TUI would
    /// never re-prime the new daemon (scope-sensitive auth
    /// answers stay degraded indefinitely). One event per
    /// successful re-establishment bounds the cost under a
    /// flapping stream to one full push per reconnect, never a
    /// per-push-cycle re-send storm.
    StreamEstablished { host: HostId },
    /// Initial snapshot frame from the daemon after a fresh
    /// subscribe. Sent once per (re)connect, before any diff
    /// frames.
    Snapshot(ManifestSnapshotPayload),
    /// One live broadcast — `ManifestDiff::Exited` carries the
    /// memory_cap_kill flag the named criterion needs.
    ///
    /// Phase 3 (remote-session-execution): `host` is the host whose
    /// `manifest.watch` stream produced this diff (set by the per-host
    /// consumer in [`spawn_per_host`]). The adoption path uses it to tag an
    /// adopted sidebar row with the right `ts.host_id` and to dial the
    /// correct daemon when attaching. Pre-Phase-3 this was a tuple variant
    /// with no host; everything routed to `HostId::local()`.
    Diff { host: HostId, diff: ManifestDiff },
}

// Operator-caller token is resolved PER HOST through a
// `crate::host_pool::TokenProvider` (the local daemon's token for
// `local`; a remote daemon's own token, fetched over ssh, for an
// ssh-unix host — see `HostPool::operator_token_for`). Re-read on every
// (re)connect attempt so the consumer never pins a stale value.

/// Reconnect backoff base (first dial-failure sleep before retry).
const RECONNECT_BACKOFF_BASE: Duration = Duration::from_secs(1);

/// Reconnect backoff cap. Bounds the worst-case retry interval
/// when the daemon is permanently down — prevents a tight spin
/// against a misconfigured/unavailable daemon.
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Handle stashed on `App` for lifecycle management. The consumer
/// thread is a "daemon" (process-lifetime) thread; we don't join
/// on App drop — process exit reaps it. Kept here mostly as
/// documentation that the thread exists.
///
/// 10e-c r1 F1: receiver carries `ManifestEvent` (was
/// `ManifestDiff`). Snapshot frames now surface as
/// `ManifestEvent::Snapshot(...)` so the App's apply path can
/// reconcile post-disconnect.
pub struct ManifestWatchConsumer {
    /// Receiver drained per tick by the App.
    pub event_rx: mpsc::Receiver<ManifestEvent>,
    /// Join handle, retained for tests + future explicit shutdown.
    /// Not joined on App drop.
    pub _thread: JoinHandle<()>,
}

/// Decide whether to spawn the consumer. 10f default-flip: the
/// daemon is mandatory, so the consumer always spawns. Kept as a
/// helper (rather than being inlined) for test-suite clarity and
/// because the field types in `maybe_spawn_for_app`'s return
/// remain `Option` — the helper still answers a meaningful
/// question even if today's answer is constant.
///
/// `daemon_socket_exists` is NOT required at decision time —
/// daemon-launch can race with the consumer's first dial. The
/// reconnect loop handles "socket appears shortly after spawn"
/// gracefully via the backoff retry.
pub fn should_spawn() -> bool {
    true
}

/// App-side helper: spawn the consumer and return the channel +
/// thread handle. 10f: post-default-flip the consumer always
/// runs, so the `Option` is always `Some(...)`. Kept as
/// `Option<...>` to preserve the test seam used by code that
/// constructs an App without a live daemon.
///
/// Called once from `App::new`.
///
/// 12c: `socket_path` was passed in by the caller as a static
/// `PathBuf`. 12e (F2 fix): now takes a
/// [`crate::host_pool::SocketPathProvider`] — a closure that
/// resolves to the *current* socket path on every dial attempt.
/// After an SSH tunnel respawn at a new random path (round 3
/// F1), the next reconnect picks up the new path automatically
/// rather than retrying the stale one forever.
pub fn maybe_spawn_for_app(
    path_provider: crate::host_pool::SocketPathProvider,
) -> (
    Option<mpsc::Receiver<ManifestEvent>>,
    Option<JoinHandle<()>>,
) {
    if !should_spawn() {
        return (None, None);
    }
    let consumer = spawn(path_provider);
    (Some(consumer.event_rx), Some(consumer._thread))
}

/// Spawn the consumer thread + return the handle. Caller wires
/// `diff_rx` into `App.manifest_watch_rx`.
///
/// `path_provider` is invoked at every reconnect attempt to get
/// the current socket path. Production builds the provider from
/// `Arc<HostPool>` + `HostId` via
/// `crate::host_pool::path_provider_for_host`. Tests use
/// [`fixed_path_provider`] for a stable path.
pub fn spawn(
    path_provider: crate::host_pool::SocketPathProvider,
) -> ManifestWatchConsumer {
    let (event_tx, event_rx) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("cm-tui-manifest-watch".to_string())
        .spawn(move || {
            run_consumer_with_provider(
                &path_provider,
                &crate::host_pool::local_token_provider(),
                HostId::local(),
                event_tx,
            )
        })
        .expect("spawn manifest.watch consumer thread");
    ManifestWatchConsumer {
        event_rx,
        _thread: thread,
    }
}

/// Test helper: wrap a static path as a
/// `SocketPathProvider` so existing tests stay simple.
#[cfg(test)]
pub(crate) fn fixed_path_provider(
    path: PathBuf,
) -> crate::host_pool::SocketPathProvider {
    std::sync::Arc::new(move || Some(path.clone()))
}

/// 12e-r2 F2 (Option A): spawn one manifest.watch consumer
/// per host in `hosts`. All consumers share a single
/// `mpsc::Sender` so the App's drain loop reads events from
/// every host through one `Receiver`. Pre-r2 we spawned a
/// single consumer bound to the (now-removed) global host at
/// App::new — sessions on any other host got no live event
/// streaming. Per-host consumers cover every configured host
/// from startup, which is what makes the per-workspace host
/// model work.
///
/// Single-host case: still one consumer, no overhead.
pub fn spawn_per_host(
    host_pool: &std::sync::Arc<crate::host_pool::HostPool>,
    hosts: &crate::hosts::HostsConfig,
) -> (
    Option<mpsc::Receiver<ManifestEvent>>,
    Vec<JoinHandle<()>>,
) {
    if !should_spawn() {
        return (None, Vec::new());
    }
    let (event_tx, event_rx) = mpsc::channel();
    let mut threads = Vec::new();
    for host in &hosts.hosts {
        let provider = crate::host_pool::path_provider_for_host(
            host_pool,
            host.id.clone(),
        );
        // Per-host operator token (a remote daemon validates against ITS
        // token, not the local one — see `HostPool::operator_token_for`).
        let token_provider = crate::host_pool::token_provider_for_host(
            host_pool,
            host.id.clone(),
        );
        let tx = event_tx.clone();
        let host_id = host.id.clone();
        let host_name = host.id.as_str().to_string();
        let thread = std::thread::Builder::new()
            .name(format!("cm-tui-manifest-watch-{}", host_name))
            .spawn(move || {
                run_consumer_with_provider(&provider, &token_provider, host_id, tx)
            })
            .expect("spawn manifest.watch consumer thread");
        threads.push(thread);
    }
    // Drop the original sender so the receiver disconnects
    // when ALL per-host consumers exit (otherwise it would
    // wait indefinitely on this last sender).
    drop(event_tx);
    (Some(event_rx), threads)
}

/// Consumer thread body. Two nested loops:
///   - Outer (reconnect): dial socket; on failure, sleep with
///     exponential backoff (1s → 30s cap); on success, reset
///     backoff and enter inner.
///   - Inner (stream-read): drain `StreamFrame`s; on
///     `ManifestDiff` send to channel; on socket error break to
///     outer for reconnect; on `SendError` (channel disconnect →
///     App gone), return entirely.
///
/// 12e (F2 fix): provider-based consumer loop. Each reconnect
/// attempt calls `path_provider()` to get the *current* socket
/// path so a respawned SSH tunnel's new random path is picked
/// up automatically. Pre-12e the path was static, captured at
/// `App::new` time, and would forever miss the new path after
/// a tunnel respawn.
pub(crate) fn run_consumer_with_provider(
    path_provider: &crate::host_pool::SocketPathProvider,
    token_provider: &crate::host_pool::TokenProvider,
    host: HostId,
    event_tx: mpsc::Sender<ManifestEvent>,
) {
    let mut backoff = RECONNECT_BACKOFF_BASE;
    loop {
        let Some(socket_path) = path_provider() else {
            // No socket path available (e.g. SSH tunnel hasn't
            // spawned yet, or pool lookup failed). Treat as a
            // transient connect failure — backoff + retry.
            eprintln!(
                "cm-tui: manifest.watch path_provider returned None \
                 (retrying in {}s)",
                backoff.as_secs(),
            );
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
            continue;
        };
        // Re-resolved per attempt: a remote token that wasn't fetchable
        // at the previous attempt (host down) is picked up on retry.
        let token = token_provider();
        match connect_and_subscribe(&socket_path, &token) {
            Ok(stream) => {
                // Reset backoff on each successful subscription.
                // A flaky daemon that goes down/up shouldn't
                // accumulate exponential delay across sessions.
                backoff = RECONNECT_BACKOFF_BASE;
                // Gap 3 (seamless-restart audit): the subscribe
                // succeeded — this is the consumer's ground-truth
                // "stream (re)established" moment (same point the
                // backoff resets). Signal the App BEFORE reading
                // any frame so the push-cache invalidation is
                // ordered ahead of everything the new stream
                // delivers. Sent once per successful subscription
                // by construction (never per retry attempt).
                if event_tx
                    .send(ManifestEvent::StreamEstablished { host: host.clone() })
                    .is_err()
                {
                    // App dropped its receiver. Done forever
                    // (mirrors DriveOutcome::ChannelDisconnected).
                    return;
                }
                match drive_stream(stream, &host, &event_tx) {
                    DriveOutcome::ChannelDisconnected => {
                        // App dropped its receiver. Done forever.
                        return;
                    }
                    DriveOutcome::StreamEnded => {
                        // Socket-side error / daemon disconnect.
                        // Fall through to outer reconnect.
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "cm-tui: manifest.watch connect to {} failed: {} \
                     (retrying in {}s)",
                    socket_path.display(),
                    e,
                    backoff.as_secs(),
                );
            }
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
    }
}

/// Backward-compatible test seam: wraps `socket_path` in a
/// fixed-path provider and dispatches to
/// `run_consumer_with_provider`. Production no longer uses this
/// — the path provider goes all the way through. Kept for tests
/// that drive the consumer against a synthetic listener with a
/// fixed path.
pub(crate) fn run_consumer(
    socket_path: &Path,
    event_tx: mpsc::Sender<ManifestEvent>,
) {
    let provider: crate::host_pool::SocketPathProvider = {
        let p = socket_path.to_path_buf();
        std::sync::Arc::new(move || Some(p.clone()))
    };
    run_consumer_with_provider(
        &provider,
        &crate::host_pool::local_token_provider(),
        HostId::local(),
        event_tx,
    );
}

/// Why the inner stream loop ended. Drives the outer loop's
/// reconnect-vs-give-up decision.
enum DriveOutcome {
    /// Channel `Sender::send` returned `Err(SendError)` — the
    /// main loop's `Receiver` was dropped. App is gone; no point
    /// reconnecting. Outer loop returns.
    ChannelDisconnected,
    /// Socket read returned EOF or error — daemon disconnect.
    /// Outer loop should backoff + reconnect.
    StreamEnded,
}

/// Dial + send `manifest.watch` RPC + verify OK response.
/// Returns the live socket on success (now in streaming mode);
/// the caller reads frames off it via [`drive_stream`].
fn connect_and_subscribe(socket_path: &Path, token: &str) -> std::io::Result<UnixStream> {
    let mut stream = UnixStream::connect(socket_path)?;
    let req = Request {
        id: format!(
            "mw-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0),
        ),
        caller: Caller::Operator(CallerOperator {
            token_id: token.to_string(),
        }),
        method: "manifest.watch".to_string(),
        params: serde_json::json!({}),
    };
    wire::write_request(&mut stream, &req)
        .map_err(|e| std::io::Error::other(format!("write manifest.watch request: {}", e)))?;
    let resp = wire::read_response(&mut stream)
        .map_err(|e| std::io::Error::other(format!("read manifest.watch response: {}", e)))?
        .ok_or_else(|| {
            std::io::Error::other("daemon closed connection before manifest.watch response")
        })?;
    if !resp.ok {
        let err = resp.error.as_ref();
        return Err(std::io::Error::other(format!(
            "manifest.watch RPC failed: {} ({:?})",
            err.map(|e| e.message.as_str()).unwrap_or("<no message>"),
            err.map(|e| e.code),
        )));
    }
    Ok(stream)
}

/// Read frames until socket EOF/error OR channel disconnect.
/// Returns the outcome so the outer loop can decide whether to
/// reconnect.
fn drive_stream(
    mut stream: UnixStream,
    host: &HostId,
    event_tx: &mpsc::Sender<ManifestEvent>,
) -> DriveOutcome {
    loop {
        match wire::read_stream_frame(&mut stream) {
            Ok(Some(frame)) => match frame.kind {
                StreamKind::ManifestSnapshot => {
                    // 10e-c r1 F1: parse the snapshot's
                    // workspaces → flatten to (uid, last_exit)
                    // pairs → forward as ManifestEvent::Snapshot.
                    // The App-side conservative-merge applies:
                    // only update local preserved_last_exit when
                    // it's None (daemon's snapshot is
                    // authoritative for sessions whose last_exit
                    // we don't yet know).
                    match parse_snapshot_payload(&frame.payload, host) {
                        Ok(payload) => {
                            if event_tx
                                .send(ManifestEvent::Snapshot(payload))
                                .is_err()
                            {
                                return DriveOutcome::ChannelDisconnected;
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "cm-tui: manifest.watch snapshot parse failed: {} \
                                 — dropping snapshot, keep reading diffs",
                                e,
                            );
                        }
                    }
                }
                StreamKind::ManifestDiff => {
                    match serde_json::from_value::<ManifestDiff>(frame.payload.clone()) {
                        Ok(diff) => {
                            if event_tx
                                .send(ManifestEvent::Diff {
                                    host: host.clone(),
                                    diff,
                                })
                                .is_err()
                            {
                                // R8: main loop's Receiver dropped.
                                // App is gone; outer loop returns
                                // (don't reconnect).
                                return DriveOutcome::ChannelDisconnected;
                            }
                        }
                        Err(e) => {
                            // Wire-protocol bug: log + drop the
                            // frame, keep reading. Future variants
                            // a future daemon might emit could
                            // land here; we don't want to drop
                            // the whole subscription.
                            eprintln!(
                                "cm-tui: manifest.watch diff deserialize failed: {} (payload: {})",
                                e, frame.payload,
                            );
                        }
                    }
                }
                StreamKind::Heartbeat => {
                    // 10e-b r1: liveness probe. Silent no-op
                    // consumer-side; the daemon uses the write
                    // success/failure to detect our disconnect.
                }
                other => {
                    eprintln!(
                        "cm-tui: manifest.watch received unexpected kind {:?} — dropping frame",
                        other,
                    );
                }
            },
            Ok(None) => {
                // Clean EOF from daemon side (shutdown).
                return DriveOutcome::StreamEnded;
            }
            Err(e) => {
                eprintln!(
                    "cm-tui: manifest.watch read error: {} — reconnecting",
                    e,
                );
                return DriveOutcome::StreamEnded;
            }
        }
    }
}

/// 10e-c r1 F1: parse the daemon's snapshot frame payload into
/// the flat per-uid `(uid, Option<LastExit>)` list the App's
/// apply path expects. The daemon's snapshot wire shape is
/// `{"workspaces": HashMap<String, ManifestWorkspace>, "bindings": ...}`
/// (set by `daemon::control::dispatch::dispatch_manifest_watch`).
/// We only consume the `workspaces[*].sessions[*]` walk; the
/// `bindings` map and other workspace metadata stay TUI-loaded-
/// from-disk.
///
/// Tolerant parser: skips workspaces/sessions whose JSON shape
/// is malformed in unexpected ways. The TUI's `ManifestEntry`
/// type is the daemon's `ManifestEntry` (re-exported), so the
/// normal wire shape parses cleanly.
fn parse_snapshot_payload(
    payload: &serde_json::Value,
    host: &HostId,
) -> Result<ManifestSnapshotPayload, String> {
    let workspaces = payload
        .get("workspaces")
        .ok_or_else(|| "snapshot missing 'workspaces' field".to_string())?
        .as_object()
        .ok_or_else(|| "snapshot 'workspaces' is not an object".to_string())?;
    let mut session_last_exits: Vec<(String, Option<LastExit>)> = Vec::new();
    let mut listed_uids: Vec<String> = Vec::new();
    for (_ws_id, ws_value) in workspaces {
        let sessions = match ws_value.get("sessions").and_then(|v| v.as_array()) {
            Some(s) => s,
            None => continue, // tolerant: skip malformed workspace
        };
        for entry in sessions {
            let uid = match entry.get("uid").and_then(|v| v.as_str()) {
                Some(u) => u.to_string(),
                None => continue,
            };
            listed_uids.push(uid.clone());
            // `last_exit` is `#[serde(skip_serializing_if = "Option::is_none")]`
            // on ManifestEntry — so a session with no exit
            // simply omits the field entirely, not Null.
            let last_exit = match entry.get("last_exit") {
                Some(v) if !v.is_null() => {
                    match serde_json::from_value::<LastExit>(v.clone()) {
                        Ok(le) => Some(le),
                        Err(_) => continue, // skip malformed entry
                    }
                }
                _ => None,
            };
            session_last_exits.push((uid, last_exit));
        }
    }
    Ok(ManifestSnapshotPayload {
        host: host.clone(),
        session_last_exits,
        listed_uids,
        received_at: std::time::Instant::now(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::net::UnixListener;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    /// Listen on a tempdir-bound socket. Returns `(socket_path,
    /// listener, tempdir-guard)`. The tempdir guard must stay in
    /// scope so the socket file isn't cleaned up mid-test.
    fn spawn_test_listener() -> (PathBuf, UnixListener, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cm-test.sock");
        let listener = UnixListener::bind(&path).expect("bind test socket");
        (path, listener, dir)
    }

    /// Gap 3: every successful subscription now emits
    /// `StreamEstablished` before any frame. Tests that drive the
    /// full consumer loop drain (and pin) it with this helper so
    /// their frame-level assertions stay focused.
    fn expect_established(
        event_rx: &mpsc::Receiver<ManifestEvent>,
    ) -> HostId {
        match event_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("StreamEstablished must be the first event after subscribe")
        {
            ManifestEvent::StreamEstablished { host } => host,
            other => panic!(
                "expected StreamEstablished before any frame, got {:?}",
                other,
            ),
        }
    }

    /// Accept one client; send the OK response then a sequence of
    /// stream frames; close. Returns the request the client sent.
    fn accept_and_send_frames(
        listener: &UnixListener,
        frames: Vec<cm_daemon::control::protocol::StreamFrame>,
    ) -> Request {
        let (mut stream, _) = listener.accept().expect("accept");
        let req = wire::read_request(&mut stream)
            .expect("read request")
            .expect("request present");
        // Send OK response matching the dispatcher's wire shape.
        let resp = cm_daemon::control::protocol::Response::ok(
            req.id.clone(),
            serde_json::json!({ "subscribed": true }),
        );
        wire::write_response(&mut stream, &resp).expect("write response");
        for frame in frames {
            wire::write_stream_frame(&mut stream, &frame).expect("write stream frame");
        }
        // Don't close — let the test decide when to drop.
        // (Drop happens when caller drops the stream returned via
        // future variant of this helper; for now we hold the
        // stream local and let it drop at function-end.)
        req
    }

    /// T13 — consumer sends a well-formed `manifest.watch` request:
    /// method matches, caller is Operator, params is `{}`.
    #[test]
    fn consumer_sends_well_formed_manifest_watch_request() {
        let (sock, listener, _dir) = spawn_test_listener();
        let sock_clone = sock.clone();
        let consumer = std::thread::spawn(move || {
            // Use connect_and_subscribe only — we don't want to
            // enter drive_stream and block on read.
            let _ = connect_and_subscribe(
                &sock_clone,
                crate::daemon_launch::operator_token(),
            );
        });

        // Accept + read the request.
        let (mut stream, _) = listener.accept().expect("accept");
        let req = wire::read_request(&mut stream)
            .expect("read request")
            .expect("request present");
        // Send OK to unblock the consumer's response read.
        let resp = cm_daemon::control::protocol::Response::ok(
            req.id.clone(),
            serde_json::json!({ "subscribed": true }),
        );
        wire::write_response(&mut stream, &resp).expect("write response");
        consumer.join().expect("consumer thread");

        assert_eq!(req.method, "manifest.watch");
        match req.caller {
            Caller::Operator(op) => {
                // Operator token is whatever load_or_create_operator_token
                // returns in the test process — non-empty is sufficient
                // for this assertion's purpose (testing the wire path).
                assert!(!op.token_id.is_empty(), "operator token must be set");
            }
            other => panic!("expected Operator caller, got {:?}", other),
        }
        // Params is `{}` (an empty object), not Null.
        assert!(req.params.is_object());
        assert_eq!(req.params.as_object().unwrap().len(), 0);
    }

    /// T14 — diff frames after the OK response land in the
    /// receiver. Uses real `wire::write_stream_frame` on the
    /// listener side; consumer's `drive_stream` deserializes via
    /// the production `serde_json::from_value` path.
    #[test]
    fn consumer_forwards_manifest_diff_frames_to_channel() {
        let (sock, listener, _dir) = spawn_test_listener();
        let (event_tx, event_rx) = mpsc::channel();
        let sock_clone = sock.clone();
        let _consumer = std::thread::spawn(move || {
            run_consumer(&sock_clone, event_tx);
        });

        // Build a snapshot frame + 2 diff frames + 1 heartbeat.
        let frames = vec![
            cm_daemon::control::protocol::StreamFrame::manifest_snapshot(
                "req-t14",
                serde_json::json!({ "workspaces": {}, "bindings": {} }),
            ),
            cm_daemon::control::protocol::StreamFrame::manifest_diff(
                "req-t14",
                serde_json::to_value(ManifestDiff::Tombstoned {
                    uid: "ts-t14-a".into(),
                    exited_at: 1.0,
                })
                .unwrap(),
            ),
            cm_daemon::control::protocol::StreamFrame::heartbeat("req-t14"),
            cm_daemon::control::protocol::StreamFrame::manifest_diff(
                "req-t14",
                serde_json::to_value(ManifestDiff::Tombstoned {
                    uid: "ts-t14-b".into(),
                    exited_at: 2.0,
                })
                .unwrap(),
            ),
        ];
        let _req = accept_and_send_frames(&listener, frames);

        // Receiver gets StreamEstablished (gap 3, once per
        // subscribe) + snapshot (forwarded post-r1) + 2 diffs
        // (heartbeat ignored).
        let established_host = expect_established(&event_rx);
        assert_eq!(
            established_host,
            HostId::local(),
            "run_consumer attributes the established stream to local",
        );
        let snap_event =
            event_rx.recv_timeout(Duration::from_secs(2)).expect("snapshot");
        match snap_event {
            ManifestEvent::Snapshot(payload) => {
                // Snapshot frame's workspaces was empty `{}` —
                // no sessions to extract.
                assert!(
                    payload.session_last_exits.is_empty(),
                    "empty workspaces snapshot → no per-uid last_exits",
                );
            }
            other => panic!("expected Snapshot, got {:?}", other),
        }
        let first =
            event_rx.recv_timeout(Duration::from_secs(2)).expect("first diff");
        match first {
            ManifestEvent::Diff { diff: ManifestDiff::Tombstoned { uid, .. }, host } => {
                assert_eq!(uid, "ts-t14-a");
                // run_consumer (the local seam) attributes diffs to the
                // local host; spawn_per_host passes the real host id.
                assert_eq!(host, HostId::local(), "diff host defaults to local via run_consumer");
            }
            other => panic!("expected Diff(Tombstoned ts-t14-a), got {:?}", other),
        }
        let second =
            event_rx.recv_timeout(Duration::from_secs(2)).expect("second diff");
        match second {
            ManifestEvent::Diff { diff: ManifestDiff::Tombstoned { uid, .. }, .. } => {
                assert_eq!(uid, "ts-t14-b");
            }
            other => panic!("expected Diff(Tombstoned ts-t14-b), got {:?}", other),
        }
    }

    /// T17 — Heartbeat frames don't propagate. (Folded into T14's
    /// assertions: the channel sees exactly the 2 diff frames out
    /// of [snapshot, diff, heartbeat, diff] sent. Adding a focused
    /// version too for explicit pinning.)
    #[test]
    fn consumer_drops_heartbeat_frames_silently() {
        let (sock, listener, _dir) = spawn_test_listener();
        let (event_tx, event_rx) = mpsc::channel();
        let sock_clone = sock.clone();
        let _consumer = std::thread::spawn(move || {
            run_consumer(&sock_clone, event_tx);
        });

        // Three heartbeats, no diffs.
        let frames = (0..3)
            .map(|_| {
                cm_daemon::control::protocol::StreamFrame::heartbeat("req-t17")
            })
            .collect();
        let _req = accept_and_send_frames(&listener, frames);

        // Gap 3: the subscribe itself emits StreamEstablished —
        // drain it; the heartbeat frames must contribute nothing
        // beyond that.
        let _ = expect_established(&event_rx);

        // Receiver gets nothing within a bounded window.
        match event_rx.recv_timeout(Duration::from_millis(200)) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // Daemon-end closed; could happen if the listener
                // dropped. Acceptable as long as no diff arrived.
            }
            Ok(diff) => panic!(
                "expected no diffs (heartbeats only); got {:?}",
                diff,
            ),
        }
    }

    /// T19 — consumer exits its outer loop on `SendError`
    /// (channel receiver dropped). NOT reconnects in a tight
    /// loop. Drives the consumer manually with a closed receiver.
    #[test]
    fn consumer_exits_on_channel_disconnect_no_reconnect_spin() {
        let (sock, listener, _dir) = spawn_test_listener();
        let (event_tx, event_rx) = mpsc::channel();
        // Drop the receiver IMMEDIATELY — but keep a "did the
        // consumer thread return?" flag.
        drop(event_rx);

        let sock_clone = sock.clone();
        let consumer = std::thread::spawn(move || {
            run_consumer(&sock_clone, event_tx);
        });

        // Accept the connection, send OK + (best-effort) one diff.
        // Gap 3 moved the consumer's first channel send EARLIER:
        // the post-subscribe `StreamEstablished` send hits the
        // closed channel and returns the consumer before it reads
        // a single frame — so the diff write below may race the
        // consumer's socket close (EPIPE) and must be tolerated,
        // not expected. (Pre-gap-3 the first send was the diff
        // forward inside drive_stream.)
        {
            let (mut stream, _) = listener.accept().expect("accept");
            let req = wire::read_request(&mut stream)
                .expect("read request")
                .expect("request present");
            let resp = cm_daemon::control::protocol::Response::ok(
                req.id.clone(),
                serde_json::json!({ "subscribed": true }),
            );
            wire::write_response(&mut stream, &resp).expect("write response");
            let frame = cm_daemon::control::protocol::StreamFrame::manifest_diff(
                "req-t19",
                serde_json::to_value(ManifestDiff::Tombstoned {
                    uid: "ts-t19".into(),
                    exited_at: 0.0,
                })
                .unwrap(),
            );
            let _ = wire::write_stream_frame(&mut stream, &frame);
        }

        // Consumer thread MUST exit within a bounded window.
        // Without the SendError early-return, it would keep
        // reconnecting to the listener forever (listener accepts
        // a 2nd connection on next loop iteration).
        let deadline =
            std::time::Instant::now() + Duration::from_secs(2);
        while !consumer.is_finished() {
            if std::time::Instant::now() >= deadline {
                panic!(
                    "consumer thread did NOT exit on channel \
                     disconnect; pre-fix it would have looped \
                     forever (tight reconnect-then-SendError spin)",
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = consumer.join();
    }

    /// T18 — reconnect resilience: kill the listener mid-stream,
    /// respawn it, consumer reconnects on its own and delivers
    /// subsequent diffs.
    ///
    /// MUST-LAND per round-3 review. Integration-style via
    /// UnixListener bind/unbind, no real daemon binary needed.
    #[test]
    fn consumer_reconnects_after_listener_drop_and_rebind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("cm-t18.sock");

        // Phase 1: bind, accept, send 1 diff, drop.
        let listener1 = UnixListener::bind(&sock).expect("bind 1");
        let (event_tx, event_rx) = mpsc::channel();
        let sock_clone = sock.clone();
        let _consumer = std::thread::spawn(move || {
            run_consumer(&sock_clone, event_tx);
        });

        {
            let (mut stream, _) = listener1.accept().expect("accept 1");
            let req = wire::read_request(&mut stream)
                .expect("read req 1")
                .expect("req 1 present");
            let resp = cm_daemon::control::protocol::Response::ok(
                req.id.clone(),
                serde_json::json!({ "subscribed": true }),
            );
            wire::write_response(&mut stream, &resp).expect("resp 1");
            let frame = cm_daemon::control::protocol::StreamFrame::manifest_diff(
                "req-1",
                serde_json::to_value(ManifestDiff::Tombstoned {
                    uid: "ts-pre-disconnect".into(),
                    exited_at: 1.0,
                })
                .unwrap(),
            );
            wire::write_stream_frame(&mut stream, &frame).expect("frame 1");
            // 10e-c r1: snapshot is now forwarded — drain it
            // first so we can then assert the diff arrived
            // after the snapshot.
            // Phase 1 we didn't send a snapshot frame, so there
            // isn't one to drain here. Gap 3: the subscribe
            // emits StreamEstablished before any frame — drain
            // it, then receive the diff.
            let _ = expect_established(&event_rx);
            let first_event = event_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("first frame event (diff)");
            match first_event {
                ManifestEvent::Diff {
                    diff: ManifestDiff::Tombstoned { ref uid, .. },
                    ..
                } => assert_eq!(uid, "ts-pre-disconnect"),
                other => panic!(
                    "expected pre-disconnect diff after the \
                     established event, got {:?}",
                    other,
                ),
            }
            // Phase 1 cleanup: drop the listener AND the stream
            // (closing the stream's daemon end → consumer's
            // read_stream_frame returns Ok(None) → consumer
            // enters outer reconnect loop).
            drop(stream);
            drop(listener1);
        }

        // Remove the socket file so rebind doesn't AddrInUse.
        let _ = std::fs::remove_file(&sock);

        // Phase 2: rebind. Consumer's reconnect-with-backoff
        // dials this fresh listener. First retry sleeps 1s, so
        // give the test ~5s window.
        let listener2 = UnixListener::bind(&sock).expect("bind 2");
        // 10e-c r1 F2: NON-blocking accept inside the deadline
        // loop. If reconnect regresses, accept returns WouldBlock
        // each iteration and the deadline check fires within
        // 50ms — clear panic message instead of a hung CI.
        listener2
            .set_nonblocking(true)
            .expect("set nonblocking true");

        let accept_deadline =
            std::time::Instant::now() + Duration::from_secs(5);
        let mut stream2 = loop {
            match listener2.accept() {
                Ok((s, _)) => {
                    // Switch back to blocking for the
                    // request-response phase below; nonblocking
                    // there would force us into a second polling
                    // loop on read.
                    s.set_nonblocking(false)
                        .expect("post-accept set_nonblocking false");
                    break s;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= accept_deadline {
                        panic!(
                            "consumer did NOT reconnect within 5s window — \
                             pre-r1 the consumer's outer reconnect loop \
                             would have re-dialed; this test pinned that \
                             behavior. Regression possible.",
                        );
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => panic!("accept failed: {}", e),
            }
        };

        let req2 = wire::read_request(&mut stream2)
            .expect("read req 2")
            .expect("req 2 present");
        let resp2 = cm_daemon::control::protocol::Response::ok(
            req2.id.clone(),
            serde_json::json!({ "subscribed": true }),
        );
        wire::write_response(&mut stream2, &resp2).expect("resp 2");

        let frame2 = cm_daemon::control::protocol::StreamFrame::manifest_diff(
            "req-2",
            serde_json::to_value(ManifestDiff::Tombstoned {
                uid: "ts-post-reconnect".into(),
                exited_at: 2.0,
            })
            .unwrap(),
        );
        wire::write_stream_frame(&mut stream2, &frame2).expect("frame 2");

        // Gap 3 pin: the RE-establishment fires a second
        // StreamEstablished (exactly the signal the App keys the
        // push-cache invalidation on — one per successful
        // re-subscribe, none for the failed dials in between).
        let _ = expect_established(&event_rx);

        // Receive the post-reconnect diff. Phase-2 didn't send
        // a snapshot frame either, so the next event IS the diff.
        let post = event_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("post-reconnect diff MUST arrive");
        match post {
            ManifestEvent::Diff { diff: ManifestDiff::Tombstoned { uid, .. }, .. } => {
                assert_eq!(uid, "ts-post-reconnect");
            }
            other => panic!(
                "expected Diff(Tombstoned ts-post-reconnect), got {:?}",
                other,
            ),
        }
    }

    /// T-malformed — daemon sends a malformed ManifestDiff
    /// payload (deserialize fails). Consumer drops THE FRAME and
    /// keeps reading. Subsequent valid frames still arrive.
    #[test]
    fn consumer_drops_malformed_diff_frame_and_keeps_reading() {
        let (sock, listener, _dir) = spawn_test_listener();
        let (event_tx, event_rx) = mpsc::channel();
        let sock_clone = sock.clone();
        let _consumer = std::thread::spawn(move || {
            run_consumer(&sock_clone, event_tx);
        });

        let frames = vec![
            // Malformed diff: payload doesn't match ManifestDiff
            // enum shape.
            cm_daemon::control::protocol::StreamFrame::manifest_diff(
                "req-malformed",
                serde_json::json!({ "not_a_variant": "garbage" }),
            ),
            // Valid follow-up diff.
            cm_daemon::control::protocol::StreamFrame::manifest_diff(
                "req-malformed",
                serde_json::to_value(ManifestDiff::Tombstoned {
                    uid: "ts-valid-after-malformed".into(),
                    exited_at: 1.0,
                })
                .unwrap(),
            ),
        ];
        let _req = accept_and_send_frames(&listener, frames);

        // accept_and_send_frames sends an OK + frames. Our
        // helper here built only `manifest_diff` frames (no
        // snapshot), so after the gap-3 established event the
        // first frame event from the receiver IS the valid diff
        // (malformed was logged + dropped).
        let _ = expect_established(&event_rx);
        let event = event_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("valid diff after malformed");
        match event {
            ManifestEvent::Diff { diff: ManifestDiff::Tombstoned { uid, .. }, .. } => {
                assert_eq!(uid, "ts-valid-after-malformed");
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    /// T21 (10e-c r1 F1) — snapshot's per-uid last_exit info is
    /// forwarded to the App via `ManifestEvent::Snapshot`.
    /// Daemon-side wire shape: `{workspaces, bindings}` where each
    /// `workspaces[ws].sessions[*]` carries a `last_exit` field.
    /// Consumer parses + flattens into the per-uid list.
    ///
    /// Mutation-verified locally by dropping the consumer's
    /// snapshot-handling arm; the test then fails because the
    /// receiver never sees `ManifestEvent::Snapshot` and times
    /// out. Documented as a structural defense — the pre-r1
    /// "snapshot ignored" bug would specifically clobber the
    /// daemon's last_exit on reconnect (F1 finding).
    #[test]
    fn consumer_forwards_snapshot_last_exit_on_reconnect() {
        let (sock, listener, _dir) = spawn_test_listener();
        let (event_tx, event_rx) = mpsc::channel();
        let sock_clone = sock.clone();
        let _consumer = std::thread::spawn(move || {
            run_consumer(&sock_clone, event_tx);
        });

        // Snapshot frame with two sessions: one with last_exit
        // populated (memory_cap_kill: true), one without.
        let last_exit = LastExit {
            code: Some(137),
            memory_cap_kill: true,
            kills_file_offset: Some(42),
            exited_at: 1_700_000_000.0,
        };
        let snapshot_payload = serde_json::json!({
            "workspaces": {
                "ws-1": {
                    "id": "ws-1",
                    "name": "test",
                    "sessions": [
                        {
                            "uid": "ts-with-exit",
                            "label": "x",
                            "session_type": "claude-code",
                            "last_exit": {
                                "code": 137,
                                "memory_cap_kill": true,
                                "kills_file_offset": 42,
                                "exited_at": 1_700_000_000.0,
                            },
                        },
                        {
                            "uid": "ts-without-exit",
                            "label": "y",
                            "session_type": "claude-code",
                        },
                    ],
                },
            },
            "bindings": {},
        });
        let frames = vec![
            cm_daemon::control::protocol::StreamFrame::manifest_snapshot(
                "req-t21",
                snapshot_payload,
            ),
        ];
        let _req = accept_and_send_frames(&listener, frames);

        // Receive the snapshot event (preceded by the gap-3
        // established event, which every subscribe emits).
        let _ = expect_established(&event_rx);
        let event = event_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("snapshot MUST arrive (was silently dropped pre-r1)");
        match event {
            ManifestEvent::Snapshot(payload) => {
                assert_eq!(payload.session_last_exits.len(), 2);
                // First entry: ts-with-exit has Some(last_exit).
                let with_exit = payload
                    .session_last_exits
                    .iter()
                    .find(|(uid, _)| uid == "ts-with-exit")
                    .expect("ts-with-exit in snapshot");
                let parsed_exit = with_exit.1.as_ref().expect("last_exit present");
                assert_eq!(parsed_exit, &last_exit);
                // Second entry: ts-without-exit has None.
                let without_exit = payload
                    .session_last_exits
                    .iter()
                    .find(|(uid, _)| uid == "ts-without-exit")
                    .expect("ts-without-exit in snapshot");
                assert!(without_exit.1.is_none());
            }
            other => panic!("expected Snapshot event, got {:?}", other),
        }
    }

    /// Gap 3 (seamless-restart audit) — `StreamEstablished` fires
    /// exactly ONCE per successful subscription, carries the
    /// consumer's real host id (the per-host consumers pass their
    /// own host, so the App can invalidate the RIGHT host's push
    /// cache), and is NOT re-emitted while the stream stays up
    /// (frames don't re-trigger it — the churn guard the App's
    /// invalidate-and-repush hook relies on).
    #[test]
    fn stream_established_fires_once_per_connect_with_host() {
        let (sock, listener, _dir) = spawn_test_listener();
        let (event_tx, event_rx) = mpsc::channel();
        let host = HostId::new("remote-x");
        let provider = fixed_path_provider(sock.clone());
        let host_for_thread = host.clone();
        let _consumer = std::thread::spawn(move || {
            run_consumer_with_provider(
                &provider,
                &crate::host_pool::local_token_provider(),
                host_for_thread,
                event_tx,
            );
        });

        // One subscribe, then a diff and a heartbeat on the SAME
        // stream — neither may produce another established event.
        let frames = vec![
            cm_daemon::control::protocol::StreamFrame::manifest_diff(
                "req-est",
                serde_json::to_value(ManifestDiff::Tombstoned {
                    uid: "ts-est".into(),
                    exited_at: 1.0,
                })
                .unwrap(),
            ),
            cm_daemon::control::protocol::StreamFrame::heartbeat("req-est"),
        ];
        let _req = accept_and_send_frames(&listener, frames);

        let got_host = expect_established(&event_rx);
        assert_eq!(
            got_host, host,
            "the established event must carry the consumer's own host \
             so the App invalidates that host's push cache, not local's",
        );
        // Next event is the diff — NOT a second established.
        match event_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("diff after established")
        {
            ManifestEvent::Diff {
                diff: ManifestDiff::Tombstoned { uid, .. },
                host: diff_host,
            } => {
                assert_eq!(uid, "ts-est");
                assert_eq!(diff_host, host);
            }
            other => panic!(
                "expected the diff (one established per connect, no \
                 re-emit), got {:?}",
                other,
            ),
        }
        // Bounded quiet window: the heartbeat must not manufacture
        // another established event.
        match event_rx.recv_timeout(Duration::from_millis(200)) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // Listener side dropped; fine as long as no extra
                // established event arrived first.
            }
            Ok(ManifestEvent::StreamEstablished { .. }) => panic!(
                "StreamEstablished re-emitted mid-stream — it must fire \
                 only at (re)subscription",
            ),
            Ok(_) => {
                // Some other stray frame event — irrelevant to this
                // pin (no second established observed).
            }
        }
    }

    /// T32 (10f) — `should_spawn` no longer consults the opt-in
    /// env var; the consumer always spawns in production. Pin
    /// the post-flip contract by scrubbing the env var and
    /// asserting `should_spawn() == true`.
    #[test]
    fn should_spawn_is_unconditional_post_flip() {
        // We use an env-var save/restore guard but the actual
        // env state doesn't matter — should_spawn is constant.
        // Asserting both states pins the post-flip invariant.
        let prev = std::env::var_os("CM_USE_DAEMON_SOCKET");
        // SAFETY: process env mutation. Tests in this file don't
        // share the env lock with daemon_launch.rs; should_spawn
        // post-flip is env-independent, so this scope is safe.
        unsafe {
            std::env::remove_var("CM_USE_DAEMON_SOCKET");
        }
        let scrubbed = should_spawn();
        unsafe {
            std::env::set_var("CM_USE_DAEMON_SOCKET", "1");
        }
        let with_optin = should_spawn();
        // Restore.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("CM_USE_DAEMON_SOCKET", v),
                None => std::env::remove_var("CM_USE_DAEMON_SOCKET"),
            }
        }
        assert!(
            scrubbed,
            "should_spawn must return true without opt-in env (post-flip)",
        );
        assert!(
            with_optin,
            "should_spawn must return true with opt-in env (no-op env var)",
        );
    }
}
