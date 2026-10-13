//! Throttled-link regression for the reattach loop (2026-10-13).
//!
//! On Owner's slow link every attach replayed the session's whole output
//! ring through the one SSH connection the host shares. Attaches whose reply
//! queued behind other panes' replays hit the 5 s handshake timeout, were
//! abandoned and retried with a fresh replay, so the link never drained: 32
//! attaches a minute, the same uid 3-4 times. This drives the real attach
//! queue and worker against a real daemon through a proxy that behaves like
//! that connection and checks every session attaches exactly once.

use super::*;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};

/// A unix-socket proxy modelling one congested SSH connection. Bytes from
/// the daemon to every client share one FIFO paced at `rate` bytes/s with a
/// bounded in-flight window (back-pressure reaches the daemon, as with an SSH
/// channel window); client-to-daemon bytes pass straight through. It counts
/// `attach.direct` requests per session uid.
struct SlowLink {
    socket: PathBuf,
    attaches: Arc<Mutex<HashMap<String, usize>>>,
    /// Daemon bytes not yet delivered to a client.
    queued: Arc<AtomicU64>,
    /// When the last byte was delivered (ms since `epoch`).
    last_delivery_ms: Arc<AtomicU64>,
    epoch: Instant,
    stop: Arc<AtomicBool>,
}

const CHUNK: usize = 16 * 1024;

impl SlowLink {
    fn start(dir: &std::path::Path, upstream: PathBuf, rate: u64, window: usize) -> Self {
        let socket = dir.join("slow-link.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let attaches = Arc::new(Mutex::new(HashMap::new()));
        let queued = Arc::new(AtomicU64::new(0));
        let last_delivery_ms = Arc::new(AtomicU64::new(0));
        let epoch = Instant::now();
        let stop = Arc::new(AtomicBool::new(false));
        let (link_tx, link_rx) = mpsc::sync_channel::<(UnixStream, Vec<u8>)>(window / CHUNK);

        // The pacer: one FIFO for every connection's daemon bytes.
        {
            let queued = queued.clone();
            let last = last_delivery_ms.clone();
            std::thread::spawn(move || {
                let mut next = Instant::now();
                for (mut client, bytes) in link_rx {
                    let now = Instant::now();
                    if next > now {
                        std::thread::sleep(next - now);
                    }
                    next = next.max(now)
                        + Duration::from_secs_f64(bytes.len() as f64 / rate as f64);
                    let _ = client.write_all(&bytes); // a client that left drops its bytes
                    queued.fetch_sub(bytes.len() as u64, Ordering::SeqCst);
                    last.store(epoch.elapsed().as_millis() as u64, Ordering::SeqCst);
                }
            });
        }
        {
            let attaches = attaches.clone();
            let queued = queued.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let client = match listener.accept() {
                        Ok((client, _)) => client,
                        Err(_) => {
                            std::thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                    };
                    client.set_nonblocking(false).unwrap();
                    let Ok(daemon) = UnixStream::connect(&upstream) else { continue };
                    // Client → daemon, sniffing the request for attach.direct.
                    let (mut from_client, mut to_daemon) =
                        (client.try_clone().unwrap(), daemon.try_clone().unwrap());
                    let attaches = attaches.clone();
                    std::thread::spawn(move || {
                        let mut head = Vec::new();
                        let mut buf = [0u8; CHUNK];
                        loop {
                            let n = match from_client.read(&mut buf) {
                                Ok(0) | Err(_) => break,
                                Ok(n) => n,
                            };
                            if head.len() < 64 * 1024 {
                                head.extend_from_slice(&buf[..n]);
                                if let Some(uid) = attach_uid(&head) {
                                    *attaches.lock().unwrap().entry(uid).or_insert(0) += 1;
                                    head = vec![0; 64 * 1024]; // counted; stop sniffing
                                }
                            }
                            if to_daemon.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                        let _ = to_daemon.shutdown(std::net::Shutdown::Write);
                    });
                    // Daemon → client, through the shared link.
                    let (mut from_daemon, link_tx, queued) =
                        (daemon, link_tx.clone(), queued.clone());
                    std::thread::spawn(move || {
                        let mut buf = vec![0u8; CHUNK];
                        loop {
                            let n = match from_daemon.read(&mut buf) {
                                Ok(0) | Err(_) => break,
                                Ok(n) => n,
                            };
                            queued.fetch_add(n as u64, Ordering::SeqCst);
                            let Ok(out) = client.try_clone() else { break };
                            if link_tx.send((out, buf[..n].to_vec())).is_err() {
                                break;
                            }
                        }
                    });
                }
            });
        }
        SlowLink { socket, attaches, queued, last_delivery_ms, epoch, stop }
    }

    /// True once nothing is queued and nothing moved for `quiet`.
    fn drained(&self, quiet: Duration) -> bool {
        let last = Duration::from_millis(self.last_delivery_ms.load(Ordering::SeqCst));
        self.queued.load(Ordering::SeqCst) == 0 && self.epoch.elapsed() > last + quiet
    }
}

impl Drop for SlowLink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn attach_uid(head: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(head);
    if !text.contains("\"attach.direct\"") {
        return None;
    }
    let start = text.find("\"uid\":\"")? + 7;
    let end = text[start..].find('"')? + start;
    Some(text[start..end].to_string())
}

#[test]
fn slow_link_attaches_each_session_once() {
    let _guard = crate::test_support::home_lock();
    let home = tempfile::tempdir().unwrap();
    let old_home = std::env::var_os("HOME");
    std::env::set_var("HOME", home.path());

    let (daemon_socket, worktree, state, daemon_stop, _daemon) =
        crate::client_session::tests::start_test_daemon("ws-slow");
    // Five panes with ~300 KB of history each: ~2 MB of replay, ~17 s at
    // 120 KB/s. With a 1 MB window, later attach replies wait well past the
    // old 5 s handshake behind earlier replays.
    let uids: Vec<String> = (0..5).map(|i| format!("ts-slow-{i}")).collect();
    for uid in &uids {
        let mut params = cm_daemon::session::SpawnParams::new(uid, uid, "/bin/sh");
        params.workspace_id = "ws-slow".into();
        params.args = vec![
            "-c".into(),
            "head -c 300000 /dev/zero | tr '\\0' x; exec sleep 600".into(),
        ];
        let session = cm_daemon::session::DaemonSession::spawn(params).expect("spawn");
        state.lock().unwrap().sessions.insert(uid.clone(), session);
    }
    std::thread::sleep(Duration::from_secs(2)); // let the history land in each ring

    let link = SlowLink::start(home.path(), daemon_socket.clone(), 120 * 1024, 1024 * 1024);
    let host = cm_daemon::host_id::HostId::new("sessions");
    let mut app = App::new(crate::config::Config {
        api_url: String::new(),
        api_token: String::new(),
        gcp_project: String::new(),
        gcp_zone: String::new(),
        repos: HashMap::new(),
    });
    let hosts = crate::hosts::HostsConfig {
        hosts: vec![
            crate::hosts::HostConfig {
                id: cm_daemon::host_id::HostId::local(),
                transport: crate::hosts::HostTransport::Unix {
                    socket: home.path().join("unused-local.sock"),
                },
                default: false,
                operator_token: None,
                operator_token_file: None,
            },
            crate::hosts::HostConfig {
                id: host.clone(),
                transport: crate::hosts::HostTransport::Unix { socket: link.socket.clone() },
                default: true,
                operator_token: None,
                operator_token_file: None,
            },
        ],
    };
    app.host_pool = Arc::new(crate::host_pool::HostPool::from_config(&hosts).expect("pool"));
    app.attach_worker = Some(crate::attach_worker::AttachWorker::spawn(app.host_pool.clone()));
    app.workspaces = vec![Workspace {
        color: None,
        pinned: false,
        id: "ws-slow".into(),
        name: "slow".into(),
        is_closed: false,
        is_cloud: false,
        repo_url: None,
        worktree_path: Some(worktree),
        main_repo_path: None,
        worker_vm: None,
        worker_zone: None,
        host_id: host.clone(),
        sessions: Vec::new(),
        tombstones: Vec::new(),
    }];
    for uid in &uids {
        let entry: cm_daemon::manifest::ManifestEntry = serde_json::from_value(serde_json::json!({
            "uid": uid, "label": uid, "session_type": "bash",
            "transcript_id": null, "host_id": host.as_str(),
        }))
        .unwrap();
        app.pending_remote_reattach
            .push(PendingRemoteReattach::new("ws-slow".into(), entry));
    }

    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        app.drain_terminal_events();
        app.requeue_stale_generation_remote_sessions();
        app.drain_deferred_remote_reattach();
        app.drain_attach_results();
        let bound = app.workspaces[0]
            .sessions
            .iter()
            .filter(|s| !s.session.exited && !app.reconnecting_sessions.contains(&s.uid))
            .count();
        if bound == uids.len()
            && app.pending_remote_reattach.is_empty()
            && app.attaching.is_empty()
            && link.drained(Duration::from_secs(2))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "replays never settled: {bound}/{} bound, attaches {:?}",
            uids.len(),
            link.attaches.lock().unwrap(),
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let attaches = link.attaches.lock().unwrap().clone();
    for uid in &uids {
        assert_eq!(attaches.get(uid), Some(&1), "{uid} re-attached: {attaches:?}");
    }

    daemon_stop.store(true, Ordering::SeqCst);
    let _ = UnixStream::connect(&daemon_socket);
    drop(app);
    for uid in &uids {
        if let Some(mut s) = state.lock().unwrap().sessions.remove(uid) {
            let _ = s.kill();
        }
    }
    match old_home {
        Some(h) => std::env::set_var("HOME", h),
        None => std::env::remove_var("HOME"),
    }
}

#[test]
fn backoff_grows_for_short_lived_streams_and_resets_after_a_stable_one() {
    let at = |s: u32| reattach_backoff_delay("ts-x", s);
    assert_eq!(at(0), REMOTE_REATTACH_RETRY_INTERVAL);
    assert!(at(1) >= Duration::from_secs(4) && at(1) < at(2) && at(2) < at(3));
    assert!(at(30) >= REMOTE_REATTACH_MAX_BACKOFF);
    assert!(at(30) <= REMOTE_REATTACH_MAX_BACKOFF * 5 / 4, "spread stays within 25%");

    let _guard = crate::test_support::home_lock();
    let mut app = App::new(crate::config::Config {
        api_url: String::new(),
        api_token: String::new(),
        gcp_project: String::new(),
        gcp_zone: String::new(),
        repos: HashMap::new(),
    });
    // A stream that dies right after attaching is a strike, and so is each
    // failed attempt; the wait grows with them.
    app.note_reattached("ts-x");
    app.note_stream_lost("ts-x");
    assert_eq!(app.reattach_backoff["ts-x"].strikes, 1);
    app.note_reattach_failed("ts-x");
    assert_eq!(app.reattach_backoff["ts-x"].strikes, 2);
    assert_eq!(app.reattach_retry_interval("ts-x"), at(2));
    // One that stayed up past the stable window reconnects at once.
    app.note_reattached("ts-x");
    app.reattach_backoff.get_mut("ts-x").unwrap().attached_at =
        Some(Instant::now() - REMOTE_STREAM_STABLE_AFTER);
    app.note_stream_lost("ts-x");
    assert_eq!(app.reattach_backoff["ts-x"].strikes, 0);
    // A stream this session never tracked (attached before) counts as healthy.
    app.note_stream_lost("ts-new");
    assert_eq!(app.reattach_backoff["ts-new"].strikes, 0);
}
