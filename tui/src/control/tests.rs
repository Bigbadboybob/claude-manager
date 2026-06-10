//! Integration tests for the control socket.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use super::protocol::{Caller, Request, Response};
use super::queue::Queue;
use super::server;

/// Spin up the socket server, send a request, and process it on the
/// main loop side. Validates the round-trip wiring.
#[test]
fn ping_round_trip() {
    let _g = crate::test_support::home_lock();
    // Use a unique CM_TUI_SOCKET so this doesn't clobber any real TUI.
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("tui.sock");
    let old = std::env::var_os("CM_TUI_SOCKET");
    unsafe {
        std::env::set_var("CM_TUI_SOCKET", &sock);
    }

    let queue = Queue::new();
    let path = server::start(queue.clone()).expect("server starts");

    // Simulate the main loop: drain and answer in a background thread.
    let q_for_main = queue.clone();
    let main_thread = std::thread::spawn(move || {
        // Poll briefly for a request; reply ok with a pong.
        for _ in 0..50 {
            for pending in q_for_main.drain() {
                let resp = Response::ok(
                    pending.request.id.clone(),
                    serde_json::json!({"pong": true}),
                );
                let _ = pending.reply.send(resp);
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });

    // Client side: connect, send a Request, read a Response.
    let mut stream = UnixStream::connect(&path).expect("connect");
    let req = Request {
        id: "test-1".into(),
        caller: Caller::session("u"),
        method: "ping".into(),
        params: serde_json::json!({}),
    };
    let body = serde_json::to_vec(&req).unwrap();
    let len = (body.len() as u32).to_be_bytes();
    stream.write_all(&len).unwrap();
    stream.write_all(&body).unwrap();
    stream.flush().unwrap();

    let resp_bytes = server::read_frame(&mut stream).unwrap();
    let resp: Response = serde_json::from_slice(&resp_bytes).unwrap();
    assert_eq!(resp.id, "test-1");
    assert!(resp.ok, "response should be ok");
    assert_eq!(
        resp.result.unwrap(),
        serde_json::json!({"pong": true})
    );

    main_thread.join().unwrap();
    unsafe {
        if let Some(o) = old {
            std::env::set_var("CM_TUI_SOCKET", o);
        } else {
            std::env::remove_var("CM_TUI_SOCKET");
        }
    }
}

#[test]
fn second_bind_against_running_server_returns_addr_in_use() {
    // Two TUIs binding the same socket path used to silently let the
    // second clobber the first (the server unconditionally unlinked
    // the existing socket file). Now the second `start` connect-probes
    // first; if a live server answers, we abort with AddrInUse.
    let _g = crate::test_support::home_lock();
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("tui.sock");
    let old = std::env::var_os("CM_TUI_SOCKET");
    unsafe {
        std::env::set_var("CM_TUI_SOCKET", &sock);
    }

    let queue1 = Queue::new();
    let _path1 = server::start(queue1.clone()).expect("first server starts");

    // Second bind on the same path: must fail loudly.
    let queue2 = Queue::new();
    let result = server::start(queue2);
    assert!(result.is_err(), "second bind should be rejected");
    let err = result.err().unwrap();
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::AddrInUse,
        "expected AddrInUse, got {:?}: {}",
        err.kind(),
        err
    );
    assert!(
        err.to_string().contains("another TUI"),
        "error message should explain why: {}",
        err
    );

    unsafe {
        if let Some(o) = old {
            std::env::set_var("CM_TUI_SOCKET", o);
        } else {
            std::env::remove_var("CM_TUI_SOCKET");
        }
    }
}

#[test]
fn second_bind_replaces_stale_socket_file() {
    // The complement: a leftover socket file from a crashed TUI (no
    // listener) should NOT block startup. The connect-probe gets
    // ConnectionRefused, we unlink, bind succeeds.
    let _g = crate::test_support::home_lock();
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("tui.sock");
    // Create a file at the path that's NOT a live socket — simulating
    // a stale-on-disk inode left by a crashed TUI.
    std::fs::write(&sock, b"").unwrap();
    let old = std::env::var_os("CM_TUI_SOCKET");
    unsafe {
        std::env::set_var("CM_TUI_SOCKET", &sock);
    }

    let queue = Queue::new();
    let result = server::start(queue);
    assert!(
        result.is_ok(),
        "stale socket should be cleaned up, got {:?}",
        result.err()
    );

    unsafe {
        if let Some(o) = old {
            std::env::set_var("CM_TUI_SOCKET", o);
        } else {
            std::env::remove_var("CM_TUI_SOCKET");
        }
    }
}

#[test]
fn bind_writes_owner_pid_sidecar_and_cleanup_removes_it() {
    // The owner-PID sidecar lets a second TUI that loses the bind race
    // name the holder in its degraded-mode banner ("kill <pid>"). On a
    // successful bind we stamp our own PID; cleanup removes it.
    let _g = crate::test_support::home_lock();
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("tui.sock");
    let old = std::env::var_os("CM_TUI_SOCKET");
    unsafe {
        std::env::set_var("CM_TUI_SOCKET", &sock);
    }

    // Before binding, no holder is recorded.
    assert_eq!(server::read_owner_pid(&sock), None);

    let queue = Queue::new();
    let path = server::start(queue.clone()).expect("server starts");

    // After binding, the sidecar names this live process.
    assert_eq!(
        server::read_owner_pid(&path),
        Some(std::process::id()),
        "sidecar should name our PID after a successful bind"
    );

    // Cleanup removes the sidecar (and the socket).
    server::cleanup(&path);
    assert_eq!(
        server::read_owner_pid(&path),
        None,
        "cleanup should remove the owner sidecar"
    );

    unsafe {
        if let Some(o) = old {
            std::env::set_var("CM_TUI_SOCKET", o);
        } else {
            std::env::remove_var("CM_TUI_SOCKET");
        }
    }
}

#[test]
fn read_owner_pid_ignores_dead_pid() {
    // A stale sidecar naming a dead/recycled PID must not be surfaced —
    // we'd be telling the user to `kill` a PID that isn't the holder.
    let _g = crate::test_support::home_lock();
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("tui.sock");
    // Write a sidecar by hand for a PID that cannot be alive. PID 0 is
    // never a normal user process and has no /proc/0 entry.
    let sidecar = {
        let mut s = sock.clone().into_os_string();
        s.push(".owner");
        std::path::PathBuf::from(s)
    };
    std::fs::write(&sidecar, "0").unwrap();
    assert_eq!(
        server::read_owner_pid(&sock),
        None,
        "a sidecar naming a non-live PID should read as None"
    );
}

#[test]
fn malformed_frame_returns_invalid_params() {
    let _g = crate::test_support::home_lock();
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("tui.sock");
    let old = std::env::var_os("CM_TUI_SOCKET");
    unsafe {
        std::env::set_var("CM_TUI_SOCKET", &sock);
    }

    let queue = Queue::new();
    let path = server::start(queue.clone()).expect("server starts");

    let mut stream = UnixStream::connect(&path).expect("connect");
    let body = b"not valid json";
    let len = (body.len() as u32).to_be_bytes();
    stream.write_all(&len).unwrap();
    stream.write_all(body).unwrap();
    stream.flush().unwrap();

    let resp_bytes = server::read_frame(&mut stream).unwrap();
    let resp: Response = serde_json::from_slice(&resp_bytes).unwrap();
    assert!(!resp.ok);
    let err = resp.error.unwrap();
    assert_eq!(err.code, super::protocol::ErrorCode::InvalidParams);

    unsafe {
        if let Some(o) = old {
            std::env::set_var("CM_TUI_SOCKET", o);
        } else {
            std::env::remove_var("CM_TUI_SOCKET");
        }
    }
}
