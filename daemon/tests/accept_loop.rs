//! Integration test for the daemon's accept loop. Slice 10a-shell.
//!
//! Proves that a real client can dial the bound socket, send a
//! length-prefixed JSON-RPC request, and receive a length-prefixed
//! response — i.e. that the wire path through `run()`'s
//! read-dispatch-write cycle works end-to-end. The dispatcher
//! itself is a placeholder that returns `UnknownMethod` for every
//! method (until slice 10b), so the test asserts on the structured
//! error response rather than on actual method behavior.
//!
//! Lives in `tests/` (integration tests) rather than as a `#[test]`
//! inside `lib.rs` because it spawns a background thread holding
//! the daemon's accept loop and a separate client thread driving
//! the round trip — clearer as a black-box test against the
//! published library API.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use cm_daemon::control::protocol::{Caller, ErrorCode, Request, Response};

/// Crate-local serialization mutex for the integration tests in
/// this binary. `bind_socket` (called inside `start_test_daemon`)
/// transiently flips the process umask to `0o177` during the bind,
/// and Linux's umask is process-global. When the three tests run in
/// parallel (cargo's default), one test's `tempfile::tempdir()` can
/// race the other's `bind_socket` window and land a directory at
/// mode `0o600` — no execute, so `UnixListener::bind` on a path
/// inside it then fails with `EACCES`. Holding this mutex across
/// each test serializes them, eliminating the race.
///
/// Same shape as the daemon crate's internal `env_lock` (which
/// isn't accessible from an integration test binary); see
/// `daemon/src/test_support.rs` for the unit-test side.
fn serialize_tests() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Wait up to ~1 second for the socket file to appear at `path`.
fn wait_for_socket(path: &std::path::Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    false
}

/// Spawn the daemon's accept loop on a background thread bound to a
/// tempdir socket. Returns the socket path and a flag the test can
/// flip to stop the daemon. Not using `cm_daemon::run` directly
/// because it loops forever; tests want a controllable lifecycle.
fn start_test_daemon() -> (std::path::PathBuf, Arc<Mutex<bool>>, thread::JoinHandle<()>) {
    start_test_daemon_with_timeout(Duration::from_secs(2))
}

/// Variant that lets tests pin the per-connection read timeout.
/// Production uses `cm_daemon::CONNECTION_READ_TIMEOUT` (30s); the
/// stalled-client test sets this to a few hundred ms so the test
/// runs fast without artificial sleeps past the production timeout.
fn start_test_daemon_with_timeout(
    read_timeout: Duration,
) -> (std::path::PathBuf, Arc<Mutex<bool>>, thread::JoinHandle<()>) {
    use cm_daemon::{bind_socket, control, state};

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("daemon.sock");
    let listener = bind_socket(&path).expect("bind");
    // Keep the tempdir alive for the lifetime of the daemon thread.
    let path_for_thread = path.clone();
    let stop = Arc::new(Mutex::new(false));
    let stop_for_thread = stop.clone();
    // Hold the TempDir in the thread so it doesn't drop early.
    let attach_addr = path.to_string_lossy().into_owned();
    let handle = thread::spawn(move || {
        let _keep_alive = dir;
        let mut initial = state::DaemonState::new();
        // Same wiring as `run()` so session.attach returns a real
        // attach_addr in test responses.
        initial.attach_addr = attach_addr;
        let state =
            std::sync::Arc::new(std::sync::Mutex::new(initial));
        // Accept loop with a soft stop. Production `run()` doesn't
        // need the stop flag; tests do.
        for incoming in listener.incoming() {
            if *stop_for_thread.lock().unwrap_or_else(|e| e.into_inner()) {
                break;
            }
            match incoming {
                Ok(mut stream) => {
                    let state = state.clone();
                    let read_timeout = read_timeout;
                    thread::spawn(move || {
                        // Mirror what `cm_daemon::run`'s
                        // handle_connection does, kept inline here
                        // because `handle_connection` is private.
                        // Apply the same timeout discipline so the
                        // test exercises the real I/O path.
                        let _ = stream.set_read_timeout(Some(read_timeout));
                        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                        let req = match control::wire::read_request(&mut stream) {
                            Ok(Some(r)) => r,
                            _ => return,
                        };
                        // Slice-10c-c review fix #2: dispatch_request
                        // now takes the Arc so individual arms can
                        // manage their own locking. The reaper-cleanup
                        // callback installed by `start_session` re-locks
                        // the same mutex.
                        let resp = control::dispatch::dispatch_request(&state, &req).into_response();
                        let _ = control::wire::write_response(&mut stream, &resp);
                    });
                }
                Err(_) => break,
            }
        }
        let _ = std::fs::remove_file(&path_for_thread);
    });
    (path, stop, handle)
}

/// Helper: send a single length-prefixed request over a fresh
/// `UnixStream` and read the length-prefixed response back. Mirrors
/// what `mcp_server/control_client.py` does on the Python side.
fn round_trip(socket: &std::path::Path, req: &Request) -> Response {
    let mut stream = UnixStream::connect(socket).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();

    let body = serde_json::to_vec(req).unwrap();
    let len = (body.len() as u32).to_be_bytes();
    stream.write_all(&len).expect("write len");
    stream.write_all(&body).expect("write body");
    stream.flush().expect("flush");

    let mut resp_len = [0u8; 4];
    stream.read_exact(&mut resp_len).expect("read len");
    let resp_len = u32::from_be_bytes(resp_len) as usize;
    let mut resp_body = vec![0u8; resp_len];
    stream.read_exact(&mut resp_body).expect("read body");
    serde_json::from_slice(&resp_body).expect("parse response")
}

#[test]
fn deferred_method_returns_unknown_with_slice_10c_pointer() {
    // Methods whose bodies still live in `tui/src/control/methods.rs`
    // (workflow tools, subtask tools, …) return UnknownMethod from
    // the daemon with an error message pointing at the migration
    // slice. Slice 10d-mcp-surface-1 wired `list_sessions`; sub-2b-2
    // wired `propose_task`; pick a workflow tool (deferred to sub-2c)
    // to exercise the fallback arm.
    let _serialize = serialize_tests();
    let (socket, stop, handle) = start_test_daemon();
    assert!(wait_for_socket(&socket), "socket must appear");

    let req = Request {
        id: "deferred-test".into(),
        caller: Caller::session("ts-x"),
        method: "workflow_transition".into(),
        params: serde_json::Value::Null,
    };
    let resp = round_trip(&socket, &req);

    assert_eq!(resp.id, "deferred-test", "id echoes back");
    assert!(!resp.ok, "deferred methods must return an error");
    let err = resp.error.expect("error body present");
    assert_eq!(err.code, ErrorCode::UnknownMethod);
    assert!(
        err.message.contains("slice 10c"),
        "error must point at the migration slice for the operator's logs: {}",
        err.message
    );

    *stop.lock().unwrap() = true;
    let _ = UnixStream::connect(&socket);
    let _ = handle.join();
}

#[test]
fn ping_round_trip_byte_for_byte_tui_parity() {
    // The named acceptance criterion: "CM_TUI_SESSION_ID scoping
    // behaves identically to today." mcp_server.ping() uses
    // `result["uid"]` as the smoke test for env-injection
    // propagation; the daemon must produce the same shape the TUI
    // does (slice-10b review fix).
    let _serialize = serialize_tests();
    let (socket, stop, handle) = start_test_daemon();
    assert!(wait_for_socket(&socket), "socket must appear");

    let req = Request {
        id: "ping-test".into(),
        caller: Caller::session("ts-end-to-end"),
        method: "ping".into(),
        params: serde_json::Value::Null,
    };
    let resp = round_trip(&socket, &req);
    assert_eq!(resp.id, "ping-test");
    assert!(resp.ok, "ping must succeed for Session caller: {:?}", resp.error);
    let result = resp.result.expect("result body");
    assert_eq!(result["pong"], true);
    assert_eq!(
        result["uid"], "ts-end-to-end",
        "uid must echo the session_uid for parity with the TUI's old dispatcher",
    );
    assert_eq!(result["caller_kind"], "session");

    *stop.lock().unwrap() = true;
    let _ = UnixStream::connect(&socket);
    let _ = handle.join();
}

#[test]
fn session_attach_with_unknown_uid_returns_not_found_end_to_end() {
    // Slice 10c-c routes session.attach through the daemon
    // dispatcher with live-registry validation. The unit tests in
    // `daemon/src/control/dispatch.rs::tests` cover the dispatch
    // function directly; this integration test confirms the wire
    // path (real socket round-trip, real read-dispatch-write) also
    // produces NotFound for a uid that isn't in `state.sessions`.
    //
    // The happy path (mint + consume + stream-transition) requires
    // a spawned `DaemonSession` in the registry — covered by the
    // unit tests in `dispatch.rs` and `control::stream` because
    // the integration `start_test_daemon` helper here doesn't
    // expose a way to inject sessions.
    let _serialize = serialize_tests();
    let (socket, stop, handle) = start_test_daemon();
    assert!(wait_for_socket(&socket), "socket must appear");

    let req = Request {
        id: "attach-unknown-uid".into(),
        caller: Caller::operator("op-default"),
        method: "session.attach".into(),
        params: serde_json::json!({ "uid": "ts-not-in-registry" }),
    };
    let resp = round_trip(&socket, &req);
    assert!(!resp.ok, "unknown uid must be rejected");
    let err = resp.error.expect("error body");
    assert_eq!(
        err.code,
        ErrorCode::NotFound,
        "live-registry validation rejects unknown uids with NotFound (slice 10c-c contract): {}",
        err.message,
    );
    assert!(
        err.message.contains("ts-not-in-registry"),
        "error must name the missing uid for debuggability: {}",
        err.message
    );

    *stop.lock().unwrap() = true;
    let _ = UnixStream::connect(&socket);
    let _ = handle.join();
}

#[test]
fn stalled_client_does_not_park_daemon_thread() {
    // Reviewer-named regression (slice 10a review #2): a connect
    // that sends nothing must not park a per-connection thread
    // forever. After the read timeout fires, the daemon should
    // still serve subsequent connections.
    //
    // Uses a short (400ms) read timeout so the test runs fast; the
    // production const is 30s. Generous margin below to absorb
    // scheduling noise when running alongside the other integration
    // tests in parallel — Linux's SO_RCVTIMEO is honored but
    // thread wake-up plus tempdir setup can add tens of ms.
    let _serialize = serialize_tests();
    let read_timeout = Duration::from_millis(400);
    let (socket, stop, handle) = start_test_daemon_with_timeout(read_timeout);
    assert!(wait_for_socket(&socket), "socket must appear");

    // Open a connection that sends nothing — the daemon's read
    // will block (then time out).
    let stalled = UnixStream::connect(&socket).expect("connect stalled");

    // Wait past the timeout so the daemon's read fires WouldBlock,
    // plus enough headroom that intermittent scheduling doesn't
    // race us to the assertion.
    thread::sleep(read_timeout + Duration::from_millis(300));

    // Daemon must still accept new connections and serve them.
    // Use a Session caller because slice-10b-review-fix flipped
    // ping to require a session-scoped caller for TUI parity.
    let req = Request {
        id: "post-stall".into(),
        caller: Caller::session("ts-post-stall"),
        method: "ping".into(),
        params: serde_json::json!({}),
    };
    let resp = round_trip(&socket, &req);
    assert_eq!(resp.id, "post-stall");
    // A successful ping is the signal that the daemon recovered
    // from the stalled connection.
    assert!(
        resp.ok,
        "daemon must remain responsive after timing out a stalled client: {:?}",
        resp.error,
    );
    assert_eq!(resp.result.unwrap()["pong"], true);

    // Hold the stalled connection until the test ends so we know
    // the daemon's recovery isn't masked by the stalled side
    // closing on its own (which would also unblock the read, but
    // for a different reason).
    drop(stalled);

    *stop.lock().unwrap() = true;
    let _ = UnixStream::connect(&socket);
    let _ = handle.join();
}

#[test]
fn multiple_round_trips_share_one_daemon_instance() {
    // Belt-and-suspenders: the per-connection thread model must
    // handle back-to-back requests on independent connections.
    let _serialize = serialize_tests();
    let (socket, stop, handle) = start_test_daemon();
    assert!(wait_for_socket(&socket), "socket must appear");

    for i in 0..3 {
        let req = Request {
            id: format!("multi-{}", i),
            // Session caller: slice-10b-review-fix flipped ping to
            // match TUI parity, where Operator → Unauthorized.
            caller: Caller::session(&format!("ts-multi-{}", i)),
            method: "ping".into(),
            params: serde_json::json!({}),
        };
        let resp = round_trip(&socket, &req);
        assert_eq!(resp.id, format!("multi-{}", i));
        assert!(resp.ok, "ping must succeed for Session caller");
        let result = resp.result.unwrap();
        assert_eq!(result["pong"], true);
        // uid echoes back per TUI parity — the smoke test
        // mcp_server uses for env-injection validation.
        assert_eq!(result["uid"], format!("ts-multi-{}", i));
    }

    *stop.lock().unwrap() = true;
    let _ = UnixStream::connect(&socket);
    let _ = handle.join();
}
