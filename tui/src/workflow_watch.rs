//! Slice 11d: TUI-side consumer for the daemon's `events.subscribe`
//! streaming RPC (landed in 11b). Mirror of `manifest_watch.rs` —
//! the structural shape (outer reconnect loop with exponential
//! backoff, inner stream-drain, channel-disconnect terminates the
//! outer loop) is intentional. Diverges only in wire-method
//! (`events.subscribe`) and event-type (`WorkflowWatchEvent` carries
//! a `WorkflowRun` snapshot OR an `Event` diff).
//!
//! Wire shape (matches `daemon/src/control/dispatch.rs::
//! dispatch_events_subscribe` + `daemon/src/control/stream.rs::
//! handle_events_subscribe_stream`):
//!
//!   1. Dial the daemon socket.
//!   2. Send `events.subscribe` JSON-RPC (Operator caller, empty
//!      params).
//!   3. Read OK response `{ "subscribed": true }`.
//!   4. Streaming mode. Frames:
//!        - `WorkflowEventStateSnapshot` (zero or more, sent FIRST
//!          before any live frames): one `WorkflowRun` JSON per
//!          active run captured at subscribe time. Read from DISK
//!          (load_all) — see 11b notes on why the in-memory cache
//!          can't be used.
//!        - `WorkflowEvent`: one `crate::workflow::events::Event`
//!          per broadcast.
//!        - `Heartbeat`: liveness probe. Silent no-op consumer-side.
//!
//! Lifecycle: same as manifest_watch — spawned at `App::new`,
//! reconnects with backoff, exits outer loop on channel-disconnect.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use cm_daemon::control::protocol::{Caller, CallerOperator, Request, StreamKind};
use cm_daemon::control::wire;
use cm_daemon::workflow::events::Event;
use cm_daemon::workflow::run::WorkflowRun;

/// Reconnect backoff base. Matches `manifest_watch`'s constant for
/// uniform user-observable behavior when both consumers reconnect
/// against the same daemon restart.
const RECONNECT_BACKOFF_BASE: Duration = Duration::from_secs(1);
/// Reconnect backoff cap. Matches `manifest_watch`'s 30s.
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Event surface delivered to the App's main loop. One variant per
/// stream-frame kind we actually process — `Heartbeat` doesn't
/// appear here, it's silently consumed.
#[derive(Debug, Clone)]
pub enum WorkflowWatchEvent {
    /// Initial-snapshot frame: one per active workflow run at the
    /// moment we (re)subscribed. The App's apply path is
    /// conservative-merge — see [`drain_workflow_watch_events`]
    /// for the semantics.
    Snapshot(WorkflowRun),
    /// Live workflow event broadcast: matches the JSON shape
    /// daemon writes to `events.jsonl`.
    Event(Event),
}

/// Handle stashed on `App` for lifecycle management. The consumer
/// thread is a process-lifetime "daemon" thread; not joined on
/// App drop.
pub struct WorkflowWatchConsumer {
    pub event_rx: mpsc::Receiver<WorkflowWatchEvent>,
    pub _thread: JoinHandle<()>,
}

/// Decide whether to spawn the consumer. Post-Phase-1 the daemon
/// is mandatory, so the consumer always spawns. Mirror of
/// `manifest_watch::should_spawn` for symmetry — kept as a helper
/// so tests can stub if needed.
pub fn should_spawn() -> bool {
    true
}

/// App-side helper: spawn the consumer and return the channel +
/// thread handle. Called once from `App::new`.
///
/// 12c: `socket_path` is passed in by the caller (resolved from
/// `App.host_pool.default_handle().socket_path()`) rather than
/// re-read from `cm_daemon::default_socket_path()` here. Routes
/// the consumer's daemon-dial through the same host-pool that
/// every other RPC site uses.
pub fn maybe_spawn_for_app(
    socket_path: PathBuf,
) -> (
    Option<mpsc::Receiver<WorkflowWatchEvent>>,
    Option<JoinHandle<()>>,
) {
    if !should_spawn() {
        return (None, None);
    }
    let consumer = spawn(socket_path);
    (Some(consumer.event_rx), Some(consumer._thread))
}

pub fn spawn(socket_path: PathBuf) -> WorkflowWatchConsumer {
    let (event_tx, event_rx) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("cm-tui-workflow-watch".to_string())
        .spawn(move || run_consumer(&socket_path, event_tx))
        .expect("spawn events.subscribe consumer thread");
    WorkflowWatchConsumer {
        event_rx,
        _thread: thread,
    }
}

pub(crate) fn run_consumer(
    socket_path: &Path,
    event_tx: mpsc::Sender<WorkflowWatchEvent>,
) {
    let mut backoff = RECONNECT_BACKOFF_BASE;
    loop {
        match connect_and_subscribe(socket_path) {
            Ok(stream) => {
                backoff = RECONNECT_BACKOFF_BASE;
                match drive_stream(stream, &event_tx) {
                    DriveOutcome::ChannelDisconnected => return,
                    DriveOutcome::StreamEnded => {}
                }
            }
            Err(e) => {
                eprintln!(
                    "cm-tui: events.subscribe connect failed: {} (retrying in {}s)",
                    e,
                    backoff.as_secs(),
                );
            }
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
    }
}

enum DriveOutcome {
    ChannelDisconnected,
    StreamEnded,
}

fn connect_and_subscribe(socket_path: &Path) -> std::io::Result<UnixStream> {
    let mut stream = UnixStream::connect(socket_path)?;
    let req = Request {
        id: format!(
            "ww-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0),
        ),
        caller: Caller::Operator(CallerOperator {
            token_id: crate::daemon_launch::operator_token().to_string(),
        }),
        method: "events.subscribe".to_string(),
        params: serde_json::json!({}),
    };
    wire::write_request(&mut stream, &req)
        .map_err(|e| std::io::Error::other(format!("write events.subscribe: {}", e)))?;
    let resp = wire::read_response(&mut stream)
        .map_err(|e| std::io::Error::other(format!("read events.subscribe: {}", e)))?
        .ok_or_else(|| {
            std::io::Error::other("daemon closed before events.subscribe response")
        })?;
    if !resp.ok {
        let err = resp.error.as_ref();
        return Err(std::io::Error::other(format!(
            "events.subscribe RPC failed: {} ({:?})",
            err.map(|e| e.message.as_str()).unwrap_or("<no message>"),
            err.map(|e| e.code),
        )));
    }
    Ok(stream)
}

fn drive_stream(
    mut stream: UnixStream,
    event_tx: &mpsc::Sender<WorkflowWatchEvent>,
) -> DriveOutcome {
    loop {
        match wire::read_stream_frame(&mut stream) {
            Ok(Some(frame)) => match frame.kind {
                StreamKind::WorkflowEventStateSnapshot => {
                    match serde_json::from_value::<WorkflowRun>(frame.payload.clone()) {
                        Ok(run) => {
                            if event_tx
                                .send(WorkflowWatchEvent::Snapshot(run))
                                .is_err()
                            {
                                return DriveOutcome::ChannelDisconnected;
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "cm-tui: events.subscribe snapshot \
                                 deserialize failed: {} (payload: {}) \
                                 — dropping frame, keep reading",
                                e, frame.payload,
                            );
                        }
                    }
                }
                StreamKind::WorkflowEvent => {
                    match serde_json::from_value::<Event>(frame.payload.clone()) {
                        Ok(ev) => {
                            if event_tx
                                .send(WorkflowWatchEvent::Event(ev))
                                .is_err()
                            {
                                return DriveOutcome::ChannelDisconnected;
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "cm-tui: events.subscribe event \
                                 deserialize failed: {} (payload: {})",
                                e, frame.payload,
                            );
                        }
                    }
                }
                StreamKind::Heartbeat => {
                    // Silent no-op; daemon uses the write
                    // success/failure to detect TUI disconnect.
                }
                other => {
                    eprintln!(
                        "cm-tui: events.subscribe received unexpected \
                         kind {:?} — dropping frame",
                        other,
                    );
                }
            },
            Ok(None) => return DriveOutcome::StreamEnded,
            Err(e) => {
                eprintln!(
                    "cm-tui: events.subscribe read error: {} — reconnecting",
                    e,
                );
                return DriveOutcome::StreamEnded;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cm_daemon::control::protocol::StreamFrame;
    use cm_daemon::workflow::run::{RoleBinding, WorkflowRun};
    use std::collections::BTreeMap;
    use std::os::unix::net::UnixListener;

    fn spawn_test_listener() -> (PathBuf, UnixListener, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cm-test.sock");
        let listener = UnixListener::bind(&path).expect("bind test socket");
        (path, listener, dir)
    }

    fn accept_and_send_frames(
        listener: &UnixListener,
        frames: Vec<StreamFrame>,
    ) -> Request {
        let (mut stream, _) = listener.accept().expect("accept");
        let req = wire::read_request(&mut stream)
            .expect("read request")
            .expect("request present");
        let resp = cm_daemon::control::protocol::Response::ok(
            req.id.clone(),
            serde_json::json!({ "subscribed": true }),
        );
        wire::write_response(&mut stream, &resp).expect("write response");
        for frame in frames {
            wire::write_stream_frame(&mut stream, &frame).expect("write frame");
        }
        req
    }

    fn make_workflow_run(run_id: &str) -> WorkflowRun {
        let mut roles = BTreeMap::new();
        roles.insert(
            "worker".to_string(),
            RoleBinding {
                session_label: "claude".into(),
                current_session_id: None,
                daemon_session_uid: None,
            },
        );
        WorkflowRun::new(
            run_id.to_string(),
            "feedback".into(),
            "/tmp/repo".into(),
            roles,
            "worker".into(),
            BTreeMap::new(),
            None,
            BTreeMap::new(),
            0,
        )
    }

    fn make_event(id: &str, run_id: &str) -> Event {
        Event {
            id: id.into(),
            ts: 0.0,
            run_id: run_id.into(),
            role: "worker".into(),
            tool: "workflow_transition".into(),
            args: serde_json::json!({"to": "reviewer", "prompt": ""}),
            source: String::new(),
            from_role: None,
            iteration: 0,
        }
    }

    /// T14 — consumer sends a well-formed `events.subscribe`
    /// request: method matches, caller is Operator, params is
    /// `{}`.
    #[test]
    fn consumer_sends_well_formed_events_subscribe_request() {
        let (sock, listener, _dir) = spawn_test_listener();
        let sock_clone = sock.clone();
        let consumer = std::thread::spawn(move || {
            let _ = connect_and_subscribe(&sock_clone);
        });

        let (mut stream, _) = listener.accept().expect("accept");
        let req = wire::read_request(&mut stream)
            .expect("read request")
            .expect("request present");
        let resp = cm_daemon::control::protocol::Response::ok(
            req.id.clone(),
            serde_json::json!({ "subscribed": true }),
        );
        wire::write_response(&mut stream, &resp).expect("write response");
        consumer.join().expect("consumer thread");

        assert_eq!(req.method, "events.subscribe");
        match req.caller {
            Caller::Operator(op) => {
                assert!(!op.token_id.is_empty(), "operator token must be set");
            }
            other => panic!("expected Operator caller, got {:?}", other),
        }
        assert!(req.params.is_object());
        assert_eq!(req.params.as_object().unwrap().len(), 0);
    }

    /// T15 — snapshot then event frames land in the receiver
    /// in order. Heartbeat is silently dropped.
    #[test]
    fn consumer_forwards_snapshot_and_event_frames_in_order() {
        let (sock, listener, _dir) = spawn_test_listener();
        let (event_tx, event_rx) = mpsc::channel();
        let sock_clone = sock.clone();
        let _consumer = std::thread::spawn(move || {
            run_consumer(&sock_clone, event_tx);
        });

        let run = make_workflow_run("wf-t15");
        let ev = make_event("e-t15-1", "wf-t15");
        let frames = vec![
            StreamFrame::workflow_event_state_snapshot(
                "req-t15",
                serde_json::to_value(&run).unwrap(),
            ),
            StreamFrame::heartbeat("req-t15"),
            StreamFrame::workflow_event(
                "req-t15",
                serde_json::to_value(&ev).unwrap(),
            ),
        ];
        let _req = accept_and_send_frames(&listener, frames);

        // First: snapshot.
        let snap_evt = event_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("snapshot");
        match snap_evt {
            WorkflowWatchEvent::Snapshot(r) => {
                assert_eq!(r.run_id, "wf-t15");
                assert_eq!(r.workflow_name, "feedback");
            }
            other => panic!("expected Snapshot, got {:?}", other),
        }
        // Second: event (heartbeat skipped).
        let ev_evt = event_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("event");
        match ev_evt {
            WorkflowWatchEvent::Event(e) => {
                assert_eq!(e.id, "e-t15-1");
                assert_eq!(e.run_id, "wf-t15");
            }
            other => panic!("expected Event, got {:?}", other),
        }
    }

    /// T16 — heartbeat-only stream produces no events on the
    /// channel within a bounded window.
    #[test]
    fn consumer_drops_heartbeat_frames_silently() {
        let (sock, listener, _dir) = spawn_test_listener();
        let (event_tx, event_rx) = mpsc::channel();
        let sock_clone = sock.clone();
        let _consumer = std::thread::spawn(move || {
            run_consumer(&sock_clone, event_tx);
        });
        let frames = (0..3)
            .map(|_| StreamFrame::heartbeat("req-t16"))
            .collect();
        let _req = accept_and_send_frames(&listener, frames);
        match event_rx.recv_timeout(Duration::from_millis(200)) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {}
            Ok(ev) => panic!("expected no events; got {:?}", ev),
        }
    }

    /// T17 — channel-disconnect causes the outer loop to exit
    /// (no tight reconnect spin).
    #[test]
    fn consumer_exits_on_channel_disconnect_no_reconnect_spin() {
        let (sock, listener, _dir) = spawn_test_listener();
        let (event_tx, event_rx) = mpsc::channel();
        drop(event_rx);

        let sock_clone = sock.clone();
        let consumer = std::thread::spawn(move || {
            run_consumer(&sock_clone, event_tx);
        });

        // Send a snapshot frame so drive_stream's send fails on
        // the dropped channel.
        let run = make_workflow_run("wf-t17");
        let frames = vec![StreamFrame::workflow_event_state_snapshot(
            "req-t17",
            serde_json::to_value(&run).unwrap(),
        )];
        let _req = accept_and_send_frames(&listener, frames);

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !consumer.is_finished() {
            if std::time::Instant::now() >= deadline {
                panic!(
                    "consumer thread did NOT exit on channel \
                     disconnect; pre-fix would loop reconnecting",
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = consumer.join();
    }

    /// T18 — reconnect resilience: kill the listener mid-stream,
    /// rebind, consumer reconnects and delivers subsequent
    /// frames.
    #[test]
    fn consumer_reconnects_after_listener_drop_and_rebind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("cm-t18-wf.sock");

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
            let ev = make_event("e-pre", "wf-pre");
            let frame = StreamFrame::workflow_event(
                "req-1",
                serde_json::to_value(&ev).unwrap(),
            );
            wire::write_stream_frame(&mut stream, &frame).expect("frame 1");
            let _first = event_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("first event");
            drop(stream);
            drop(listener1);
        }

        let _ = std::fs::remove_file(&sock);

        let listener2 = UnixListener::bind(&sock).expect("bind 2");
        listener2.set_nonblocking(true).expect("set nonblocking");
        let accept_deadline =
            std::time::Instant::now() + Duration::from_secs(5);
        let mut stream2 = loop {
            match listener2.accept() {
                Ok((s, _)) => {
                    s.set_nonblocking(false).expect("post-accept blocking");
                    break s;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= accept_deadline {
                        panic!(
                            "consumer did NOT reconnect within 5s — \
                             reconnect-loop regression?",
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
        let post_ev = make_event("e-post", "wf-post");
        let frame2 = StreamFrame::workflow_event(
            "req-2",
            serde_json::to_value(&post_ev).unwrap(),
        );
        wire::write_stream_frame(&mut stream2, &frame2).expect("frame 2");

        let post = event_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("post-reconnect event MUST arrive");
        match post {
            WorkflowWatchEvent::Event(e) => {
                assert_eq!(e.id, "e-post");
            }
            other => panic!("expected Event, got {:?}", other),
        }
    }

    /// T19 — malformed event frame: consumer drops it and keeps
    /// reading. A valid follow-up still arrives.
    #[test]
    fn consumer_drops_malformed_event_frame_and_keeps_reading() {
        let (sock, listener, _dir) = spawn_test_listener();
        let (event_tx, event_rx) = mpsc::channel();
        let sock_clone = sock.clone();
        let _consumer = std::thread::spawn(move || {
            run_consumer(&sock_clone, event_tx);
        });
        let ev = make_event("e-valid", "wf-x");
        let frames = vec![
            StreamFrame::workflow_event(
                "req-malformed",
                serde_json::json!({ "missing": "fields" }),
            ),
            StreamFrame::workflow_event(
                "req-malformed",
                serde_json::to_value(&ev).unwrap(),
            ),
        ];
        let _req = accept_and_send_frames(&listener, frames);
        let received = event_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("valid event after malformed");
        match received {
            WorkflowWatchEvent::Event(e) => {
                assert_eq!(e.id, "e-valid");
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    /// T20 — should_spawn is unconditionally true (mirror of
    /// manifest_watch's post-flip behavior).
    #[test]
    fn should_spawn_is_unconditional() {
        assert!(should_spawn());
    }
}
