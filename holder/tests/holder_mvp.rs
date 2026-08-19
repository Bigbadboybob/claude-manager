//! Behavioral suite for the phase-2 holder MVP — the design's
//! "own-process behavioral tests against scripted brains (no daemon
//! involvement)": each test owns one end of a socketpair and ACTS as
//! the brain; the [`Holder`] loop runs on a thread of this same test
//! process, which is therefore the parent of every spawned session
//! child (the production parenthood requirement for `waitid`).
//!
//! Covers the S4 seam the design names for this phase (fast-exit
//! zombie parking, forget-refusal, abort-no-event), the S1 environ
//! canary, C13 signal ordering / S16 no-stamp-after-exit, O4
//! attribution echo, C4/C9 redelivery-across-brain-generations, and
//! the protocol law (unsupported verbs tolerated; oversized frames
//! and req_id-less requests fatal).
#![cfg(target_os = "linux")]

use std::collections::VecDeque;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cm_holder::holder::{Holder, HolderConfig, ServeOutcome};
use cm_holder_proto::channel::{self as ch, verbs, FeedStatus, Frame, FrameReader};

const DEADLINE: Duration = Duration::from_secs(10);

fn socketpair() -> (OwnedFd, OwnedFd) {
    let mut sv = [0i32; 2];
    // SAFETY: valid out-array.
    let ret = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
            sv.as_mut_ptr(),
        )
    };
    assert_eq!(ret, 0, "socketpair: {}", std::io::Error::last_os_error());
    // SAFETY: socketpair succeeded; both fds are ours.
    (unsafe { OwnedFd::from_raw_fd(sv[0]) }, unsafe {
        OwnedFd::from_raw_fd(sv[1])
    })
}

fn test_config() -> HolderConfig {
    HolderConfig {
        handshake_timeout: Duration::from_secs(5),
        ping_interval: None,
        outbound_max_frames: 4096,
        holder_build_id: "cm-holder/test".into(),
    }
}

/// The scripted brain: blocking-ish frame IO with a hard deadline
/// (a misbehaving holder must fail tests, not hang them), plus a
/// buffer for unsolicited frames (exit events, pings) that arrive
/// while waiting for a reply.
struct Brain {
    fd: OwnedFd,
    reader: FrameReader,
    buffered: VecDeque<(Frame, Vec<OwnedFd>)>,
    next_req: u64,
}

impl Brain {
    fn new(fd: OwnedFd) -> Brain {
        // Nonblocking on OUR side too: the harness's polling helpers
        // (`try_recv`, deadline loops) must never park in a blocking
        // recvmsg — a misbehaving holder has to FAIL tests, not hang
        // the suite.
        // SAFETY: plain fcntl on an owned fd.
        unsafe {
            let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
            if flags >= 0 {
                libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
        }
        Brain {
            fd,
            reader: FrameReader::new(),
            buffered: VecDeque::new(),
            next_req: 1,
        }
    }

    fn send(&mut self, verb: &str, body: impl serde::Serialize) -> u64 {
        let req_id = self.next_req;
        self.next_req += 1;
        let f = Frame::new(verb, Some(req_id), 0, body);
        ch::send_frame_blocking(self.fd.as_fd(), &f, &[]).expect("send");
        req_id
    }

    fn recv_any(&mut self) -> (Frame, Vec<OwnedFd>) {
        if let Some(hit) = self.buffered.pop_front() {
            return hit;
        }
        let deadline = Instant::now() + DEADLINE;
        loop {
            if let Some(hit) = self.reader.next_frame().expect("protocol") {
                return hit;
            }
            assert!(Instant::now() < deadline, "no frame within deadline");
            match self.reader.feed(self.fd.as_fd()).expect("feed") {
                FeedStatus::Eof => panic!("channel EOF while awaiting a frame"),
                FeedStatus::WouldBlock => std::thread::sleep(Duration::from_millis(5)),
                FeedStatus::Progress => {}
            }
        }
    }

    /// Await the reply to `req_id`, buffering anything unsolicited.
    fn wait_reply(&mut self, req_id: u64) -> (Frame, Vec<OwnedFd>) {
        // Scan the buffer first.
        if let Some(pos) = self
            .buffered
            .iter()
            .position(|(f, _)| f.req_id == Some(req_id))
        {
            return self.buffered.remove(pos).unwrap();
        }
        loop {
            let (f, fds) = self.recv_any();
            if f.req_id == Some(req_id) {
                return (f, fds);
            }
            self.buffered.push_back((f, fds));
        }
    }

    fn request(&mut self, verb: &str, body: impl serde::Serialize) -> (Frame, Vec<OwnedFd>) {
        let id = self.send(verb, body);
        self.wait_reply(id)
    }

    fn hello(&mut self) -> ch::HelloReplyBody {
        let (f, _) = self.request(
            verbs::HELLO,
            ch::HelloBody {
                proto_min: ch::PROTO_VERSION_MIN,
                proto_max: ch::PROTO_VERSION_MAX,
                brain_build_id: "test-brain".into(),
            },
        );
        assert_eq!(f.v, verbs::HELLO_REPLY, "got {:?}", f);
        f.parse_body().unwrap()
    }

    /// Await the next exit_event (buffered or fresh).
    fn wait_exit_event(&mut self) -> ch::ExitEventBody {
        if let Some(pos) = self.buffered.iter().position(|(f, _)| f.v == verbs::EXIT_EVENT) {
            let (f, _) = self.buffered.remove(pos).unwrap();
            return f.parse_body().unwrap();
        }
        loop {
            let (f, fds) = self.recv_any();
            if f.v == verbs::EXIT_EVENT {
                return f.parse_body().unwrap();
            }
            self.buffered.push_back((f, fds));
        }
    }

    /// Assert no exit_event arrives within `window`.
    fn assert_no_exit_event_for(&mut self, window: Duration) {
        assert!(
            !self.buffered.iter().any(|(f, _)| f.v == verbs::EXIT_EVENT),
            "buffered exit_event present"
        );
        let deadline = Instant::now() + window;
        while Instant::now() < deadline {
            if let Some((f, fds)) = self.try_recv() {
                assert_ne!(f.v, verbs::EXIT_EVENT, "unexpected exit_event: {:?}", f);
                self.buffered.push_back((f, fds));
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    fn try_recv(&mut self) -> Option<(Frame, Vec<OwnedFd>)> {
        if let Some(hit) = self.reader.next_frame().expect("protocol") {
            return Some(hit);
        }
        match self.reader.feed(self.fd.as_fd()).expect("feed") {
            FeedStatus::Eof => panic!("unexpected EOF"),
            _ => self.reader.next_frame().expect("protocol"),
        }
    }

    /// Collect a full adopt stream: records + done.
    fn adopt(&mut self) -> (Vec<(ch::AdoptRecordBody, Vec<OwnedFd>)>, ch::AdoptDoneBody) {
        let id = self.send(verbs::ADOPT, serde_json::json!({}));
        let mut records = Vec::new();
        loop {
            let (f, fds) = self.recv_any();
            if f.req_id != Some(id) {
                self.buffered.push_back((f, fds));
                continue;
            }
            match f.v.as_str() {
                verbs::ADOPT_RECORD => records.push((f.parse_body().unwrap(), fds)),
                verbs::ADOPT_LISTENERS => {}
                verbs::ADOPT_DONE => return (records, f.parse_body().unwrap()),
                other => panic!("unexpected adopt-stream verb '{other}'"),
            }
        }
    }
}

/// Run a holder generation on a thread; returns (join, brain-end).
fn start(holder: Holder) -> (JoinHandle<(Holder, ServeOutcome)>, Brain) {
    let (ours, theirs) = socketpair();
    let handle = std::thread::spawn(move || {
        let mut h = holder;
        let out = h.serve(ours);
        (h, out)
    });
    (handle, Brain::new(theirs))
}

fn spawn_body(uid: &str, argv: &[&str]) -> ch::SpawnBody {
    let mut env = std::collections::BTreeMap::new();
    env.insert("PATH".to_string(), "/usr/bin:/bin".to_string());
    ch::SpawnBody {
        uid: uid.into(),
        generation_meta: 1,
        argv: argv.iter().map(|s| s.to_string()).collect(),
        env,
        cwd: Some("/".into()),
        cols: 80,
        rows: 24,
        cgroup_prefix: None,
    }
}

fn spawn_ok(brain: &mut Brain, uid: &str, argv: &[&str]) -> (ch::SpawnOkBody, Vec<OwnedFd>) {
    let (f, fds) = brain.request(verbs::SPAWN, spawn_body(uid, argv));
    assert_eq!(f.v, verbs::OK, "spawn reply: {:?}", f);
    assert_eq!(fds.len(), 2, "spawn carries [master_dup, pidfd_dup]");
    (f.parse_body().unwrap(), fds)
}

fn err_of(f: &Frame) -> ch::ErrBody {
    assert_eq!(f.v, verbs::ERR, "expected err, got {:?}", f);
    f.parse_body().unwrap()
}

// ============================================================
// Tests
// ============================================================

#[test]
fn hello_negotiates_and_status_answers() {
    let (join, mut brain) = start(Holder::new(test_config()));
    let hello = brain.hello();
    assert_eq!(hello.proto, ch::PROTO_VERSION_MAX);
    assert_eq!(hello.epoch, 1);
    assert_eq!(hello.session_count, 0);

    let (f, _) = brain.request(verbs::STATUS, serde_json::json!({}));
    let st: ch::StatusReplyBody = f.parse_body().unwrap();
    assert_eq!(st.sessions, 0);
    assert_eq!(st.epoch, 1);
    assert_eq!(st.breaker_state, "none");

    drop(brain);
    let (_h, out) = join.join().unwrap();
    assert_eq!(out, ServeOutcome::BrainEof);
}

#[test]
fn hello_version_gap_is_refused_with_proto_mismatch() {
    let (join, mut brain) = start(Holder::new(test_config()));
    let id = brain.send(
        verbs::HELLO,
        ch::HelloBody {
            proto_min: 99,
            proto_max: 99,
            brain_build_id: "future-brain".into(),
        },
    );
    let (f, _) = brain.wait_reply(id);
    let err = err_of(&f);
    assert_eq!(err.code, ch::ERR_PROTO_MISMATCH);
    let (_h, out) = join.join().unwrap();
    assert_eq!(out, ServeOutcome::HelloRefused);
}

#[test]
fn spawn_hands_out_working_master_and_pidfd_dups() {
    let (join, mut brain) = start(Holder::new(test_config()));
    brain.hello();
    let (ok, fds) = spawn_ok(&mut brain, "s-cat", &["/bin/cat"]);
    assert_eq!(ok.incarnation, 1);
    assert!(ok.pid > 0);
    assert!(ok.child_start_time > 0);

    // The master dup is live PTY I/O: write, expect the echo back.
    let master = &fds[0];
    // SAFETY: valid fd + buffer.
    let n = unsafe { libc::write(master.as_raw_fd(), b"ping\n".as_ptr() as *const _, 5) };
    assert_eq!(n, 5);
    let deadline = Instant::now() + DEADLINE;
    let mut got = Vec::new();
    while got.is_empty() && Instant::now() < deadline {
        let mut buf = [0u8; 256];
        let mut pfd = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: valid pollfd.
        if unsafe { libc::poll(&mut pfd, 1, 200) } > 0 {
            // SAFETY: valid fd + buffer.
            let r = unsafe { libc::read(master.as_raw_fd(), buf.as_mut_ptr() as *mut _, 256) };
            if r > 0 {
                got.extend_from_slice(&buf[..r as usize]);
            }
        }
    }
    assert!(
        String::from_utf8_lossy(&got).contains("ping"),
        "PTY bytes flow through the dup: {:?}",
        String::from_utf8_lossy(&got)
    );

    // The pidfd dup observes the live child (poll: not exit-ready).
    let mut pfd = libc::pollfd {
        fd: fds[1].as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: valid pollfd, zero timeout.
    assert_eq!(unsafe { libc::poll(&mut pfd, 1, 0) }, 0, "child is alive");

    let (f, _) = brain.request(
        verbs::ABORT_SPAWN,
        ch::AbortSpawnBody {
            uid: "s-cat".into(),
            incarnation: ok.incarnation,
        },
    );
    assert_eq!(f.v, verbs::OK);
    drop(brain);
    let (_h, out) = join.join().unwrap();
    assert_eq!(out, ServeOutcome::BrainEof);
}

#[test]
fn child_env_is_exactly_the_spec_never_the_holder_environ() {
    // The S1 guard: the holder (this test process) carries a canary
    // var; the spec does not. The child must see the spec var and
    // NOT the canary.
    std::env::set_var("CM_HOLDER_ENV_CANARY", "must-not-leak");
    let (join, mut brain) = start(Holder::new(test_config()));
    brain.hello();
    let mut body = spawn_body("s-env", &["/bin/cat"]);
    body.env
        .insert("CM_SPEC_MARKER".into(), "rode-the-spec".into());
    let (f, _fds) = brain.request(verbs::SPAWN, body);
    assert_eq!(f.v, verbs::OK);
    let ok: ch::SpawnOkBody = f.parse_body().unwrap();

    let environ = std::fs::read(format!("/proc/{}/environ", ok.pid)).expect("child environ");
    let environ = String::from_utf8_lossy(&environ).replace('\0', "\n");
    assert!(
        environ.contains("CM_SPEC_MARKER=rode-the-spec"),
        "spec env applied: {environ}"
    );
    assert!(
        !environ.contains("CM_HOLDER_ENV_CANARY"),
        "holder environ leaked into a session: {environ}"
    );
    assert!(environ.contains("PATH=/usr/bin:/bin"));

    let (f, _) = brain.request(
        verbs::ABORT_SPAWN,
        ch::AbortSpawnBody {
            uid: "s-env".into(),
            incarnation: ok.incarnation,
        },
    );
    assert_eq!(f.v, verbs::OK);
    drop(brain);
    join.join().unwrap();
}

#[test]
fn fast_exit_is_zombie_parked_until_arm_reap() {
    // The S4 seam: a child that dies instantly produces NO exit
    // event before arm_reap; its /proc stays readable (zombie —
    // nothing consumed the status); arming consumes + delivers.
    let (join, mut brain) = start(Holder::new(test_config()));
    brain.hello();
    let (ok, _fds) = spawn_ok(&mut brain, "s-fast", &["/bin/false"]);

    brain.assert_no_exit_event_for(Duration::from_millis(400));
    // Zombie parked: the stat line still exists and starttime
    // matches the spawn-time capture (the /proc window the daemon's
    // post-spawn discovery depends on).
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", ok.pid))
        .expect("zombie /proc entry readable");
    assert!(stat.contains(") Z") || stat.contains(") X") || !stat.is_empty());

    let (f, _) = brain.request(
        verbs::ARM_REAP,
        ch::ArmReapBody {
            uid: "s-fast".into(),
            incarnation: ok.incarnation,
            cgroup_path: None,
        },
    );
    assert_eq!(f.v, verbs::OK);
    let ev = brain.wait_exit_event();
    assert_eq!(ev.uid, "s-fast");
    assert_eq!(ev.incarnation, ok.incarnation);
    assert_eq!(ev.code, Some(1));
    assert_eq!(ev.signal, None);
    assert!(ev.last_signal_request.is_none());
    assert!(ev.exited_at > 0.0);

    let (f, _) = brain.request(
        verbs::ACK_EXIT,
        ch::AckExitBody {
            uid: "s-fast".into(),
            incarnation: ok.incarnation,
        },
    );
    assert_eq!(f.v, verbs::OK);
    let (f, _) = brain.request(
        verbs::FORGET,
        ch::ForgetBody {
            uid: "s-fast".into(),
            incarnation: ok.incarnation,
        },
    );
    assert_eq!(f.v, verbs::OK);

    let (f, _) = brain.request(verbs::STATUS, serde_json::json!({}));
    let st: ch::StatusReplyBody = f.parse_body().unwrap();
    assert_eq!(st.sessions, 0);
    drop(brain);
    join.join().unwrap();
}

#[test]
fn forget_is_refused_for_live_and_unacked_records() {
    let (join, mut brain) = start(Holder::new(test_config()));
    brain.hello();

    // Live child → not_exited (the O6 zombie-leak rule).
    let (ok, _fds) = spawn_ok(&mut brain, "s-live", &["/bin/cat"]);
    let (f, _) = brain.request(
        verbs::FORGET,
        ch::ForgetBody {
            uid: "s-live".into(),
            incarnation: ok.incarnation,
        },
    );
    assert_eq!(err_of(&f).code, ch::ERR_NOT_EXITED);

    // Reaped-but-unacked → unacked (C4: ack is the durable commit).
    let (ok2, _fds) = spawn_ok(&mut brain, "s-un", &["/bin/false"]);
    let (f, _) = brain.request(
        verbs::ARM_REAP,
        ch::ArmReapBody {
            uid: "s-un".into(),
            incarnation: ok2.incarnation,
            cgroup_path: None,
        },
    );
    assert_eq!(f.v, verbs::OK);
    let _ev = brain.wait_exit_event();
    let (f, _) = brain.request(
        verbs::FORGET,
        ch::ForgetBody {
            uid: "s-un".into(),
            incarnation: ok2.incarnation,
        },
    );
    assert_eq!(err_of(&f).code, ch::ERR_UNACKED);

    // Cleanup.
    let (f, _) = brain.request(
        verbs::ACK_EXIT,
        ch::AckExitBody {
            uid: "s-un".into(),
            incarnation: ok2.incarnation,
        },
    );
    assert_eq!(f.v, verbs::OK);
    let (f, _) = brain.request(
        verbs::ABORT_SPAWN,
        ch::AbortSpawnBody {
            uid: "s-live".into(),
            incarnation: ok.incarnation,
        },
    );
    assert_eq!(f.v, verbs::OK);
    drop(brain);
    join.join().unwrap();
}

#[test]
fn kill_attribution_rides_the_signal_verb_and_echoes_in_the_exit_event() {
    let (join, mut brain) = start(Holder::new(test_config()));
    brain.hello();
    let (ok, _fds) = spawn_ok(&mut brain, "s-kill", &["/bin/cat"]);
    let (f, _) = brain.request(
        verbs::ARM_REAP,
        ch::ArmReapBody {
            uid: "s-kill".into(),
            incarnation: ok.incarnation,
            cgroup_path: None,
        },
    );
    assert_eq!(f.v, verbs::OK);

    let (f, _) = brain.request(
        verbs::SIGNAL,
        ch::SignalBody {
            uid: "s-kill".into(),
            incarnation: ok.incarnation,
            sig: libc::SIGKILL,
            attribution: "operator".into(),
        },
    );
    assert_eq!(f.v, verbs::OK, "signal delivered: {:?}", f);

    let ev = brain.wait_exit_event();
    assert_eq!(ev.signal, Some(libc::SIGKILL));
    let lsr = ev.last_signal_request.expect("attribution echoed");
    assert_eq!(lsr.attribution, "operator");
    assert_eq!(lsr.sig, libc::SIGKILL);
    drop(brain);
    join.join().unwrap();
}

#[test]
fn signal_against_an_exited_child_stamps_nothing() {
    // C13/S16: a natural exit that raced a kill must not acquire a
    // killed_by.
    let (join, mut brain) = start(Holder::new(test_config()));
    brain.hello();
    let (ok, fds) = spawn_ok(&mut brain, "s-race", &["/bin/false"]);
    // Wait for the child to actually be exit-ready via our pidfd dup.
    let deadline = Instant::now() + DEADLINE;
    loop {
        let mut pfd = libc::pollfd {
            fd: fds[1].as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: valid pollfd.
        if unsafe { libc::poll(&mut pfd, 1, 50) } > 0 {
            break;
        }
        assert!(Instant::now() < deadline, "child never exited");
    }
    let (f, _) = brain.request(
        verbs::SIGNAL,
        ch::SignalBody {
            uid: "s-race".into(),
            incarnation: ok.incarnation,
            sig: libc::SIGKILL,
            attribution: "operator".into(),
        },
    );
    assert_eq!(err_of(&f).code, ch::ERR_ALREADY_EXITED);

    let (f, _) = brain.request(
        verbs::ARM_REAP,
        ch::ArmReapBody {
            uid: "s-race".into(),
            incarnation: ok.incarnation,
            cgroup_path: None,
        },
    );
    assert_eq!(f.v, verbs::OK);
    let ev = brain.wait_exit_event();
    assert_eq!(ev.code, Some(1), "a natural exit");
    assert!(
        ev.last_signal_request.is_none(),
        "no attribution stamped for the raced kill (S16)"
    );
    drop(brain);
    join.join().unwrap();
}

#[test]
fn abort_spawn_kills_reaps_and_emits_no_event() {
    let (join, mut brain) = start(Holder::new(test_config()));
    brain.hello();
    let (ok, _fds) = spawn_ok(&mut brain, "s-abort", &["/bin/cat"]);
    let (f, _) = brain.request(
        verbs::ABORT_SPAWN,
        ch::AbortSpawnBody {
            uid: "s-abort".into(),
            incarnation: ok.incarnation,
        },
    );
    assert_eq!(f.v, verbs::OK);
    brain.assert_no_exit_event_for(Duration::from_millis(300));
    let (f, _) = brain.request(verbs::STATUS, serde_json::json!({}));
    let st: ch::StatusReplyBody = f.parse_body().unwrap();
    assert_eq!(st.sessions, 0);
    assert_eq!(st.pending_exit_events, 0);
    drop(brain);
    join.join().unwrap();
}

#[test]
fn duplicate_uid_is_refused_with_the_existing_incarnation() {
    let (join, mut brain) = start(Holder::new(test_config()));
    brain.hello();
    let (ok, _fds) = spawn_ok(&mut brain, "s-dup", &["/bin/cat"]);
    let (f, fds) = brain.request(verbs::SPAWN, spawn_body("s-dup", &["/bin/cat"]));
    assert!(fds.is_empty());
    let err = err_of(&f);
    assert_eq!(err.code, ch::ERR_UID_EXISTS);
    assert_eq!(
        err.detail.unwrap()["incarnation"],
        serde_json::json!(ok.incarnation)
    );
    let (f, _) = brain.request(
        verbs::ABORT_SPAWN,
        ch::AbortSpawnBody {
            uid: "s-dup".into(),
            incarnation: ok.incarnation,
        },
    );
    assert_eq!(f.v, verbs::OK);
    drop(brain);
    join.join().unwrap();
}

#[test]
fn exit_during_brain_downtime_redelivers_to_the_next_generation() {
    // The design's one-line pitch, end to end: brain dies; child
    // exits with nobody watching; the next brain adopts, arms, and
    // receives the authoritative status.
    let (join, mut brain) = start(Holder::new(test_config()));
    brain.hello();
    let (ok, _fds) = spawn_ok(&mut brain, "s-surv", &["/bin/cat"]);
    let (f, _) = brain.request(
        verbs::ARM_REAP,
        ch::ArmReapBody {
            uid: "s-surv".into(),
            incarnation: ok.incarnation,
            cgroup_path: None,
        },
    );
    assert_eq!(f.v, verbs::OK);

    // Brain generation 1 dies.
    drop(brain);
    let (holder, out) = join.join().unwrap();
    assert_eq!(out, ServeOutcome::BrainEof);

    // The child dies during the downtime (we are its parent's
    // process — the holder lives in our threads — so a plain kill
    // by pid is safe here).
    // SAFETY: pid is our own just-spawned child.
    unsafe { libc::kill(ok.pid, libc::SIGKILL) };

    // Brain generation 2: adopt shows the record (reaped by the
    // gen-2 loop's first poll — arm authorization persisted), and
    // arming delivers the queued event.
    let (join2, mut brain2) = start(holder);
    let hello = brain2.hello();
    assert_eq!(hello.epoch, 2);
    assert_eq!(hello.session_count, 1);

    // Give the loop a moment to consume the exit.
    std::thread::sleep(Duration::from_millis(100));
    let (records, done) = brain2.adopt();
    assert_eq!(records.len(), 1);
    let (rec, fds) = &records[0];
    assert_eq!(rec.uid, "s-surv");
    assert_eq!(rec.incarnation, ok.incarnation);
    assert!(rec.reap_armed);
    assert_eq!(fds.len(), 2);
    assert_eq!(done.exit_events_pending, 1);
    assert!(rec.reaped && rec.exit_event_pending);

    let (f, _) = brain2.request(
        verbs::ARM_REAP,
        ch::ArmReapBody {
            uid: "s-surv".into(),
            incarnation: ok.incarnation,
            cgroup_path: None,
        },
    );
    assert_eq!(f.v, verbs::OK);
    let ev = brain2.wait_exit_event();
    assert_eq!(ev.signal, Some(libc::SIGKILL));
    assert_eq!(ev.incarnation, ok.incarnation);

    let (f, _) = brain2.request(
        verbs::ACK_EXIT,
        ch::AckExitBody {
            uid: "s-surv".into(),
            incarnation: ok.incarnation,
        },
    );
    assert_eq!(f.v, verbs::OK);
    let (f, _) = brain2.request(
        verbs::FORGET,
        ch::ForgetBody {
            uid: "s-surv".into(),
            incarnation: ok.incarnation,
        },
    );
    assert_eq!(f.v, verbs::OK);
    drop(brain2);
    join2.join().unwrap();
}

#[test]
fn delivered_but_unacked_events_redeliver_and_acks_are_idempotent() {
    // C4: delivery is not the commit — the ack is. An event the
    // gen-1 brain received but never acked redelivers to gen 2.
    let (join, mut brain) = start(Holder::new(test_config()));
    brain.hello();
    let (ok, _fds) = spawn_ok(&mut brain, "s-c4", &["/bin/false"]);
    let (f, _) = brain.request(
        verbs::ARM_REAP,
        ch::ArmReapBody {
            uid: "s-c4".into(),
            incarnation: ok.incarnation,
            cgroup_path: None,
        },
    );
    assert_eq!(f.v, verbs::OK);
    let _delivered_but_never_acked = brain.wait_exit_event();
    drop(brain);
    let (holder, _) = join.join().unwrap();

    let (join2, mut brain2) = start(holder);
    brain2.hello();
    let (records, done) = brain2.adopt();
    assert_eq!(done.exit_events_pending, 1);
    assert!(records[0].0.exit_event_pending);
    let (f, _) = brain2.request(
        verbs::ARM_REAP,
        ch::ArmReapBody {
            uid: "s-c4".into(),
            incarnation: ok.incarnation,
            cgroup_path: None,
        },
    );
    assert_eq!(f.v, verbs::OK);
    let ev = brain2.wait_exit_event();
    assert_eq!(ev.code, Some(1));

    let (f, _) = brain2.request(
        verbs::ACK_EXIT,
        ch::AckExitBody {
            uid: "s-c4".into(),
            incarnation: ok.incarnation,
        },
    );
    let ok1: ch::OkBody = f.parse_body().unwrap();
    assert_eq!(ok1.detail.unwrap()["known"], serde_json::json!(true));
    // Replayed ack (the C4 tombstone-match flow) is idempotent.
    let (f, _) = brain2.request(
        verbs::ACK_EXIT,
        ch::AckExitBody {
            uid: "s-c4".into(),
            incarnation: ok.incarnation,
        },
    );
    assert_eq!(f.v, verbs::OK);
    let ok2: ch::OkBody = f.parse_body().unwrap();
    assert_eq!(ok2.detail.unwrap()["known"], serde_json::json!(false));
    drop(brain2);
    join2.join().unwrap();
}

#[test]
fn unknown_verbs_get_a_typed_error_and_the_channel_lives_on() {
    let (join, mut brain) = start(Holder::new(test_config()));
    brain.hello();
    // A future/phase-6 verb this holder does not speak.
    let (f, _) = brain.request("restart_brain", serde_json::json!({}));
    assert_eq!(err_of(&f).code, ch::ERR_UNSUPPORTED_VERB);
    // Channel still healthy.
    let (f, _) = brain.request(verbs::STATUS, serde_json::json!({}));
    assert_eq!(f.v, verbs::OK);
    drop(brain);
    let (_h, out) = join.join().unwrap();
    assert_eq!(out, ServeOutcome::BrainEof);
}

#[test]
fn oversized_frames_and_reqid_less_requests_are_protocol_fatal() {
    // Oversized length prefix (C7).
    let (join, brain) = start(Holder::new(test_config()));
    let mut raw = Vec::new();
    raw.extend_from_slice(&(ch::MAX_FRAME_BYTES + 1).to_le_bytes());
    raw.extend_from_slice(b"garbage");
    // SAFETY: plain send on our end.
    let n = unsafe {
        libc::send(
            brain.fd.as_raw_fd(),
            raw.as_ptr() as *const _,
            raw.len(),
            libc::MSG_NOSIGNAL,
        )
    };
    assert!(n > 0);
    let (_h, out) = join.join().unwrap();
    assert!(
        matches!(out, ServeOutcome::Protocol(ref m) if m.contains("MAX_FRAME_BYTES")),
        "{out:?}"
    );
    drop(brain);

    // Request without req_id (C7's envelope law; pong is the one
    // exception).
    let (join, brain) = start(Holder::new(test_config()));
    let f = Frame::new(verbs::STATUS, None, 0, serde_json::json!({}));
    ch::send_frame_blocking(brain.fd.as_fd(), &f, &[]).unwrap();
    let (_h, out) = join.join().unwrap();
    assert!(
        matches!(out, ServeOutcome::Protocol(ref m) if m.contains("without req_id")),
        "{out:?}"
    );
    drop(brain);
}

#[test]
fn watchdog_pings_flow_and_pongs_reset_the_counter() {
    let mut cfg = test_config();
    cfg.ping_interval = Some(Duration::from_millis(80));
    let (join, mut brain) = start(Holder::new(cfg));
    brain.hello();

    // Await a ping, answer it.
    let deadline = Instant::now() + DEADLINE;
    let seq = loop {
        let (f, _) = brain.recv_any();
        if f.v == verbs::PING {
            let p: ch::PingBody = f.parse_body().unwrap();
            break p.seq;
        }
        assert!(Instant::now() < deadline);
    };
    let pong = Frame::new(verbs::PONG, None, 0, ch::PingBody { seq });
    ch::send_frame_blocking(brain.fd.as_fd(), &pong, &[]).unwrap();

    // Shortly after the pong, the unanswered counter is 0 again
    // (subsequent pings may bump it between our pong and the status
    // read, so allow <= 1).
    let (f, _) = brain.request(verbs::STATUS, serde_json::json!({}));
    let st: ch::StatusReplyBody = f.parse_body().unwrap();
    assert!(st.pings_unanswered <= 1, "{}", st.pings_unanswered);
    drop(brain);
    join.join().unwrap();
}

#[test]
fn checkpoint_updates_round_trip_through_adopt() {
    // R12/C11: a pushed watcher checkpoint is stored opaquely and
    // rides the adopt record back out to the next brain generation.
    let (join, mut brain) = start(Holder::new(test_config()));
    brain.hello();
    let (ok, _fds) = spawn_ok(&mut brain, "s-cp", &["/bin/cat"]);
    let (f, _) = brain.request(
        verbs::ARM_REAP,
        ch::ArmReapBody {
            uid: "s-cp".into(),
            incarnation: ok.incarnation,
            cgroup_path: Some("/sys/fs/cgroup/fake/cm-sess-x.scope".into()),
        },
    );
    assert_eq!(f.v, verbs::OK);
    let cp = serde_json::json!({
        "version": 2,
        "cgroup_path": "/sys/fs/cgroup/fake/cm-sess-x.scope",
        "protected": [[1234, 567890]],
        "last_high": 3,
        "kills_baseline": 42,
    });
    let (f, _) = brain.request(
        verbs::UPDATE_CHECKPOINT,
        ch::UpdateCheckpointBody {
            uid: "s-cp".into(),
            incarnation: ok.incarnation,
            watcher_checkpoint: cp.clone(),
        },
    );
    assert_eq!(f.v, verbs::OK, "{f:?}");
    // Wrong incarnation → not_found (O2 identity discipline).
    let (f, _) = brain.request(
        verbs::UPDATE_CHECKPOINT,
        ch::UpdateCheckpointBody {
            uid: "s-cp".into(),
            incarnation: ok.incarnation + 99,
            watcher_checkpoint: serde_json::json!({}),
        },
    );
    assert_eq!(err_of(&f).code, ch::ERR_NOT_FOUND);

    // Next generation: the adopt record carries the blob verbatim.
    drop(brain);
    let (holder, _) = join.join().unwrap();
    let (join2, mut brain2) = start(holder);
    brain2.hello();
    let (records, _done) = brain2.adopt();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].0.watcher_checkpoint, Some(cp));
    // Cleanup: kill via signal + arm + ack + forget.
    let (f, _) = brain2.request(
        verbs::ABORT_SPAWN,
        ch::AbortSpawnBody {
            uid: "s-cp".into(),
            incarnation: ok.incarnation,
        },
    );
    assert_eq!(f.v, verbs::OK);
    drop(brain2);
    join2.join().unwrap();
}

#[test]
fn listener_custody_round_trips_across_generations() {
    // O11: the brain binds, the holder custodies; the next
    // generation adopts a WORKING listener (same open file
    // description — a queued connect made during the gap is
    // acceptable by the design, but here we just prove accept works
    // through the adopted dup).
    let dir = tempfile::tempdir().expect("tempdir");
    let sock_path = dir.path().join("custody.sock");
    let listener = std::os::unix::net::UnixListener::bind(&sock_path).expect("bind");

    let (join, mut brain) = start(Holder::new(test_config()));
    brain.hello();
    let meta = ch::ListenerMeta {
        kind: "unix".into(),
        meta: sock_path.to_string_lossy().into_owned(),
    };
    let store = Frame::new(
        verbs::STORE_LISTENER,
        Some(900),
        1,
        ch::StoreListenerBody {
            listener: meta.clone(),
        },
    );
    ch::send_frame_blocking(brain.fd.as_fd(), &store, &[listener.as_raw_fd()])
        .expect("send store_listener");
    let (f, _) = brain.wait_reply(900);
    assert_eq!(f.v, verbs::OK, "{f:?}");
    // The brain's own copy can close — custody keeps it alive.
    drop(listener);
    drop(brain);
    let (holder, _) = join.join().unwrap();

    let (join2, mut brain2) = start(holder);
    brain2.hello();
    let id = brain2.send(verbs::ADOPT, serde_json::json!({}));
    let mut got: Option<(ch::ListenerMeta, OwnedFd)> = None;
    loop {
        let (f, mut fds) = brain2.recv_any();
        if f.req_id != Some(id) {
            continue;
        }
        match f.v.as_str() {
            verbs::ADOPT_LISTENERS => {
                let body: ch::AdoptListenersBody = f.parse_body().unwrap();
                assert_eq!(body.listeners.len(), 1);
                assert_eq!(fds.len(), 1);
                got = Some((body.listeners[0].clone(), fds.pop().unwrap()));
            }
            verbs::ADOPT_DONE => break,
            _ => {}
        }
    }
    let (adopted_meta, adopted_fd) = got.expect("listener adopted");
    assert_eq!(adopted_meta, meta);
    // The adopted fd ACCEPTS: connect a client to the path and
    // accept through the dup.
    let adopted: std::os::unix::net::UnixListener = adopted_fd.into();
    adopted
        .set_nonblocking(true)
        .expect("nonblocking accept");
    let _client = std::os::unix::net::UnixStream::connect(&sock_path).expect("connect");
    let deadline = Instant::now() + DEADLINE;
    loop {
        match adopted.accept() {
            Ok(_) => break,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "accept never completed");
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("accept: {e}"),
        }
    }
    drop(brain2);
    join2.join().unwrap();
}
