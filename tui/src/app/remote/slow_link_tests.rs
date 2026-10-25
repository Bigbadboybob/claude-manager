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
    /// Daemon bytes delivered to clients in total.
    delivered: Arc<AtomicU64>,
    /// Every client connection, so a test can cut them all (a link drop).
    clients: Arc<Mutex<Vec<UnixStream>>>,
    /// Attach requests that asked to resume (`since`).
    resumes: Arc<AtomicU64>,
    /// While set, new connections are closed at once (a link that is down).
    refuse: Arc<AtomicBool>,
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
        let delivered = Arc::new(AtomicU64::new(0));
        let clients: Arc<Mutex<Vec<UnixStream>>> = Arc::new(Mutex::new(Vec::new()));
        let resumes = Arc::new(AtomicU64::new(0));
        let refuse = Arc::new(AtomicBool::new(false));
        let epoch = Instant::now();
        let stop = Arc::new(AtomicBool::new(false));
        let (link_tx, link_rx) = mpsc::sync_channel::<(UnixStream, Vec<u8>)>(window / CHUNK);

        // The pacer: one FIFO for every connection's daemon bytes.
        {
            let queued = queued.clone();
            let last = last_delivery_ms.clone();
            let delivered = delivered.clone();
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
                    delivered.fetch_add(bytes.len() as u64, Ordering::SeqCst);
                    last.store(epoch.elapsed().as_millis() as u64, Ordering::SeqCst);
                }
            });
        }
        {
            let attaches = attaches.clone();
            let queued = queued.clone();
            let stop = stop.clone();
            let clients = clients.clone();
            let resumes = resumes.clone();
            let refuse = refuse.clone();
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
                    if refuse.load(Ordering::SeqCst) {
                        continue; // dropped: the client sees EOF
                    }
                    clients.lock().unwrap().push(client.try_clone().unwrap());
                    let Ok(daemon) = UnixStream::connect(&upstream) else { continue };
                    // Client → daemon, sniffing the request for attach.direct.
                    let (mut from_client, mut to_daemon) =
                        (client.try_clone().unwrap(), daemon.try_clone().unwrap());
                    let attaches = attaches.clone();
                    let resumes = resumes.clone();
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
                                    if String::from_utf8_lossy(&head).contains("\"since\"") {
                                        resumes.fetch_add(1, Ordering::SeqCst);
                                    }
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
        SlowLink { socket, attaches, queued, last_delivery_ms, delivered, clients, resumes, refuse, epoch, stop }
    }

    /// Drop every connection, as a link failure does.
    fn cut(&self) {
        for client in self.clients.lock().unwrap().drain(..) {
            let _ = client.shutdown(std::net::Shutdown::Both);
        }
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

/// A real daemon with five ~300 KB panes on host "sessions", reached through
/// a [`SlowLink`] (120 KB/s, 1 MB window), and an App with their attaches
/// queued. ~2 MB of full replay: ~17 s on this link, so with full replays
/// later attach replies wait well past the old 5 s handshake.
struct Fixture {
    app: App,
    link: SlowLink,
    uids: Vec<String>,
    state: StdArc<Mutex<cm_daemon::state::DaemonState>>,
    daemon_socket: PathBuf,
    daemon_stop: StdArc<AtomicBool>,
    old_home: Option<std::ffi::OsString>,
    _home: tempfile::TempDir,
}

use std::sync::Arc as StdArc;
use alacritty_terminal::grid::Dimensions as _;

impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());
        let (daemon_socket, worktree, state, daemon_stop, _daemon) =
            crate::client_session::tests::start_test_daemon("ws-slow");
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
            org: None,
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
        Fixture { app, link, uids, state, daemon_socket, daemon_stop, old_home, _home: home }
    }

    /// Run the main loop's attach phases until every pane is bound and the
    /// link has been idle for 2 s.
    fn settle(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            let app = &mut self.app;
            app.drain_terminal_events();
            app.requeue_stale_generation_remote_sessions();
            app.drain_deferred_remote_reattach();
            app.drain_attach_results();
            let bound = app.workspaces[0]
                .sessions
                .iter()
                .filter(|s| !s.session.exited && !app.reconnecting_sessions.contains(&s.uid))
                .count();
            if bound == self.uids.len()
                && app.pending_remote_reattach.is_empty()
                && app.attaching.is_empty()
                && self.link.drained(Duration::from_secs(2))
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "replays never settled: {bound}/{} bound, attaches {:?}",
                self.uids.len(),
                self.link.attaches.lock().unwrap(),
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn attaches(&self) -> HashMap<String, usize> {
        self.link.attaches.lock().unwrap().clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.daemon_stop.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.daemon_socket);
        self.app.workspaces.clear();
        for uid in &self.uids {
            if let Some(mut s) = self.state.lock().unwrap().sessions.remove(uid) {
                let _ = s.kill();
            }
        }
        match self.old_home.take() {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
    }
}

#[test]
fn slow_link_attaches_each_session_once() {
    let _guard = crate::test_support::home_lock();
    let mut fx = Fixture::new();
    // Full replays: the congested shape stage 1 had to survive.
    for uid in &fx.uids {
        fx.app.history_fetches.insert(uid.clone(), Default::default());
    }
    fx.settle();
    let attaches = fx.attaches();
    for uid in &fx.uids {
        assert_eq!(attaches.get(uid), Some(&1), "{uid} re-attached: {attaches:?}");
    }
    let delivered = fx.link.delivered.load(Ordering::SeqCst);
    eprintln!("full-ring cold start: {delivered} bytes");
    // 5 x 300 KB rings, sent as raw frames (base64 JSON was 2.0 MB).
    assert!(delivered > 1_400_000, "full rings were replayed");
    assert!(delivered < 1_700_000, "rings went as raw frames, not base64");
}

#[test]
fn remote_panes_attach_screen_first_and_load_history_on_scroll_up() {
    let _guard = crate::test_support::home_lock();
    let mut fx = Fixture::new();
    let started = Instant::now();
    fx.settle();
    let delivered = fx.link.delivered.load(Ordering::SeqCst);
    eprintln!("screen-first cold start: {delivered} bytes, bound in {:?}", started.elapsed());
    assert!(
        delivered < 100_000,
        "5 screen-first attaches sent {delivered} bytes (full rings are ~2 MB)",
    );
    assert!(started.elapsed() < Duration::from_secs(10), "cold start took {:?}", started.elapsed());
    for ts in &fx.app.workspaces[0].sessions {
        assert!(ts.session.history_deferred, "{} has no history yet", ts.uid);
        let term = ts.session.term.lock();
        assert_eq!(term.grid().history_size(), 0);
    }

    // Owner scrolls up in one pane: that pane, and only it, fetches its ring.
    let si = fx.app.workspaces[0].sessions.iter().position(|s| s.uid == "ts-slow-2").unwrap();
    fx.app.cursor = Cursor::Session(0, si);
    fx.app.load_deferred_history();
    fx.app.load_deferred_history(); // a second scroll while loading is a no-op
    assert!(!fx.app.reconnecting_sessions.contains("ts-slow-2"), "a scrollback load is no reconnect (no ⟳)");
    assert!(!fx.app.workspaces[0].sessions[si].session.exited, "the live stream stays until the ring binds");
    assert_eq!(fx.app.status_msg.as_ref().map(|m| m.0.as_str()), Some("Loading scrollback…"));
    fx.settle();
    assert_eq!(fx.app.status_msg.as_ref().map(|m| m.0.as_str()), Some("Scrollback loaded"));
    let attaches = fx.attaches();
    assert_eq!(attaches.get("ts-slow-2"), Some(&2), "{attaches:?}");
    for uid in fx.uids.iter().filter(|u| *u != "ts-slow-2") {
        assert_eq!(attaches.get(uid), Some(&1), "{uid}: {attaches:?}");
    }
    let ts = fx.app.workspaces[0].sessions.iter().find(|s| s.uid == "ts-slow-2").unwrap();
    assert!(!ts.session.history_deferred);
    assert!(!fx.app.history_fetches.contains_key("ts-slow-2"));
    let wait = Instant::now() + Duration::from_secs(5);
    while ts.session.term.lock().grid().history_size() == 0 {
        assert!(Instant::now() < wait, "the ring never reached the pane");
        std::thread::sleep(Duration::from_millis(20));
    }
    // The scroll that asked for it takes effect once the replay is read.
    let wait = Instant::now() + Duration::from_secs(5);
    loop {
        fx.app.apply_scroll_after_load();
        let ts = fx.app.workspaces[0].sessions.iter().find(|s| s.uid == "ts-slow-2").unwrap();
        if ts.session.term.lock().grid().display_offset() > 0 {
            break;
        }
        assert!(Instant::now() < wait, "the pane was never scrolled up");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_failing_scrollback_load_retries_then_gives_up_and_a_new_scroll_starts_over() {
    let _guard = crate::test_support::home_lock();
    let mut fx = Fixture::new();
    fx.settle();
    let si = fx.app.workspaces[0].sessions.iter().position(|s| s.uid == "ts-slow-3").unwrap();
    fx.app.cursor = Cursor::Session(0, si);
    fx.link.refuse.store(true, Ordering::SeqCst);
    fx.app.load_deferred_history();
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    while fx.app.history_fetches.contains_key("ts-slow-3") {
        assert!(Instant::now() < deadline, "never gave up: {seen:?}");
        fx.app.drain_terminal_events();
        fx.app.drain_deferred_remote_reattach();
        fx.app.drain_attach_results();
        if let Some((msg, _)) = &fx.app.status_msg {
            if seen.last() != Some(msg) {
                seen.push(msg.clone());
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(seen, [
        "Loading scrollback…",
        "Scrollback load failed — retrying (1)",
        "Scrollback load failed — retrying (2)",
        "Scrollback load failed — scroll up again or press A-r to retry",
    ]);
    assert!(
        !fx.app.pending_remote_reattach.iter().any(|p| p.entry.uid == "ts-slow-3"),
        "nothing left retrying",
    );
    let ts = &fx.app.workspaces[0].sessions[si];
    assert!(!ts.session.exited && ts.session.history_deferred, "the live pane is untouched");
    assert!(!fx.app.reconnecting_sessions.contains("ts-slow-3"));

    fx.link.refuse.store(false, Ordering::SeqCst);
    fx.app.load_deferred_history();
    fx.settle();
    let ts = fx.app.workspaces[0].sessions.iter().find(|s| s.uid == "ts-slow-3").unwrap();
    assert!(!ts.session.history_deferred, "a new scroll loads it");
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

fn screen_text(ts: &TerminalSession) -> String {
    use alacritty_terminal::index::{Column, Line};
    let term = ts.session.term.lock();
    let grid = term.grid();
    (0..grid.screen_lines())
        .map(|l| {
            let row = &grid[Line(l as i32)];
            (0..grid.columns()).map(|c| row[Column(c)].c).collect::<String>().trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_dropped_link_resumes_each_pane_with_only_the_bytes_it_missed() {
    let _guard = crate::test_support::home_lock();
    let mut fx = Fixture::new();
    fx.settle();
    let screens: Vec<String> = fx.app.workspaces[0].sessions.iter().map(screen_text).collect();
    assert!(screens.iter().all(|s| s.contains("xxxx")), "panes show their output");
    let before = fx.link.delivered.load(Ordering::SeqCst);

    fx.link.cut();
    for (i, uid) in fx.uids.iter().enumerate() {
        let fanout = fx.state.lock().unwrap().sessions[uid].fanout.clone();
        fanout.push(format!("\r\nmissed while away {i}").as_bytes());
    }
    fx.settle();

    let attaches = fx.attaches();
    for uid in &fx.uids {
        assert_eq!(attaches.get(uid), Some(&2), "{uid}: {attaches:?}");
    }
    assert_eq!(fx.link.resumes.load(Ordering::SeqCst), 5, "every reconnect asked to resume");
    let resent = fx.link.delivered.load(Ordering::SeqCst) - before;
    assert!(resent < 2_000, "resuming 5 panes sent {resent} bytes");
    // Panes join the workspace in attach-completion order, not uid order.
    for ts in &fx.app.workspaces[0].sessions {
        let i = fx.uids.iter().position(|u| *u == ts.uid).unwrap();
        assert!(ts.session.resumed, "{} resumed", ts.uid);
        assert!(ts.session.history_deferred, "{} keeps its scrollback state", ts.uid);
        let wait = Instant::now() + Duration::from_secs(5);
        while !screen_text(ts).contains(&format!("missed while away {i}")) {
            assert!(
                Instant::now() < wait,
                "{}: pane position {:?}, daemon cursor {}: {}",
                ts.uid,
                ts.session.stream.as_ref().map(|c| c.position.load(Ordering::SeqCst)),
                fx.state.lock().unwrap().sessions[&ts.uid].fanout.snapshot_since(None).cursor,
                screen_text(ts).lines().last().unwrap_or(""),
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(screen_text(ts).contains("xxxx"), "the earlier screen is still there");
    }
}

/// Investigation (2026-10-16): Owner scrolls up while a reconnect for that
/// pane is already in flight.
#[test]
fn scroll_up_during_an_in_flight_reconnect_still_loads_history() {
    let _guard = crate::test_support::home_lock();
    let mut fx = Fixture::new();
    fx.settle();
    let si = fx.app.workspaces[0].sessions.iter().position(|s| s.uid == "ts-slow-2").unwrap();
    fx.app.cursor = Cursor::Session(0, si);
    // Drop the pane's stream and drive the loop until its reconnect is in
    // flight on the attach worker.
    fx.app.nudge_remote_reconnects();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !fx.app.attaching.contains_key("ts-slow-2") {
        assert!(Instant::now() < deadline, "the reconnect never started");
        fx.app.drain_terminal_events();
        fx.app.drain_deferred_remote_reattach();
        std::thread::sleep(Duration::from_millis(2));
    }
    fx.app.load_deferred_history();
    fx.settle();
    // A later scroll, as Owner would after seeing nothing.
    fx.app.load_deferred_history();
    fx.settle();
    let ts = fx.app.workspaces[0].sessions.iter().find(|s| s.uid == "ts-slow-2").unwrap();
    eprintln!(
        "history_deferred={} fetch pending={} attaches={:?}",
        ts.session.history_deferred,
        fx.app.history_fetches.contains_key("ts-slow-2"),
        fx.attaches(),
    );
    assert!(!ts.session.history_deferred, "scrollback never loaded");
}

/// 2026-10-16 investigation: the link stays down for longer than the
/// give-up window. Every failure is a transport failure, which never counts
/// toward the cap, so no pane gives up; when the link returns every pane
/// reattaches on its own (no A-r) and takes keystrokes again.
#[test]
fn a_long_outage_never_gives_up_and_every_pane_takes_input_when_the_link_returns() {
    let _guard = crate::test_support::home_lock();
    let mut fx = Fixture::new();
    fx.settle();

    fx.link.refuse.store(true, Ordering::SeqCst);
    fx.link.cut();
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let app = &mut fx.app;
        // Hold every pane at the edge of the cap: one counted failure
        // would give it up.
        for p in app.pending_remote_reattach.iter_mut() {
            p.attempts = REMOTE_REATTACH_MAX_ATTEMPTS - 1;
        }
        for p in app.attaching.values_mut() {
            p.attempts = REMOTE_REATTACH_MAX_ATTEMPTS - 1;
        }
        app.drain_terminal_events();
        app.requeue_stale_generation_remote_sessions();
        app.drain_deferred_remote_reattach();
        app.drain_attach_results();
        for ts in &app.workspaces[0].sessions {
            assert!(!ts.session.exited, "{} gave up during the outage", ts.uid);
        }
        let failed_enough = fx.uids.iter().all(|uid| {
            app.reattach_backoff.get(uid).is_some_and(|b| b.strikes >= 3)
        });
        if failed_enough {
            break;
        }
        assert!(Instant::now() < deadline, "panes did not keep retrying: {:?}", app.reattach_backoff.keys());
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(fx.app.workspaces[0].sessions.len(), fx.uids.len(), "every row is still there");

    fx.link.refuse.store(false, Ordering::SeqCst);
    fx.settle();
    for (i, ts) in fx.app.workspaces[0].sessions.iter_mut().enumerate() {
        ts.session.write(format!("typed-after-outage-{i}").as_bytes()).expect("input");
    }
    for (i, ts) in fx.app.workspaces[0].sessions.iter().enumerate() {
        let want = format!("typed-after-outage-{i}");
        let wait = Instant::now() + Duration::from_secs(10);
        // The PTY echoes what reaches it, so the pane shows it.
        while !screen_text(ts).contains(&want) {
            assert!(Instant::now() < wait, "{}: input never reached the PTY", ts.uid);
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
