//! The holder state machine + single-threaded serve loop.
//! DESIGN_HOLDER_BRAIN_SPLIT § "The holder" + § "Protocol verbs"
//! (phase 2: hello / spawn / arm_reap / adopt / signal / abort_spawn
//! / exit_event / ack_exit / forget / status / ping — every other
//! verb answers `unsupported_verb` until its phase lands).
//!
//! ## Shape
//!
//! One `poll(2)` loop over: the brain channel (IN always, OUT while
//! the outbound queue is nonempty), one pidfd per un-latched live
//! session child, and time (handshake deadline / ping timer) via the
//! poll timeout. No threads, no locks. Channel IO is strictly
//! NONBLOCKING with a bounded outbound queue; a brain that stops
//! draining is a wedge, surfaced as [`ServeOutcome::Wedged`] (S3 —
//! the loop must never block on a `write()` to a stuck brain,
//! because this same loop is the watchdog that must catch it).
//!
//! ## Reap authorization vs. delivery readiness
//!
//! Two distinct gates (S4/C9):
//!
//! - `reap_armed` (persistent): permission to `waitid` this child at
//!   all. Granted once by the first `arm_reap` and kept across brain
//!   generations — an armed record's uid is durably known to brain
//!   persistence, so consuming its status during brain downtime is
//!   safe (the ack protocol redelivers).
//! - `delivery_ready` (per brain generation): whether THIS brain has
//!   inserted the record and may receive its exit events. Reset on
//!   every [`Holder::serve`]; set by `arm_reap` — which the brain
//!   sends after its registry insert on the spawn path AND after
//!   each adopt-time insert (C9). Events (pending or new) for a
//!   record deliver only while `delivery_ready`.
//!
//! An exit-ready pidfd of an UN-armed record is masked out of the
//! poll set with its readiness latched (V7 — a reaped-ready pidfd is
//! level-triggered and would spin the loop); the child is held as a
//! zombie so its `/proc` stays readable for the brain's post-spawn
//! discovery (the zombie-parking property).

use std::collections::{BTreeMap, VecDeque};
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use cm_holder_proto::channel::{self as ch, verbs, FeedStatus, Frame, FrameReader};
use portable_pty::MasterPty;

use crate::{reap, spawn};

/// Loop tuning. Tests shrink the intervals; the binary uses
/// defaults.
#[derive(Debug, Clone)]
pub struct HolderConfig {
    /// Bound on the brain's first frame (hello) after a channel
    /// comes up. The parked migration brain's LONG wait is on the
    /// BRAIN's side of the handshake (V8) — holder-side this only
    /// bounds a connected-but-silent brain.
    pub handshake_timeout: Duration,
    /// Watchdog ping cadence; `None` disables pings entirely.
    pub ping_interval: Option<Duration>,
    /// Outbound-queue bound in FRAMES; exceeding it means the brain
    /// stopped draining ⇒ [`ServeOutcome::Wedged`] (S3).
    pub outbound_max_frames: usize,
    pub holder_build_id: String,
}

impl Default for HolderConfig {
    fn default() -> Self {
        HolderConfig {
            handshake_timeout: Duration::from_secs(30),
            ping_interval: Some(Duration::from_secs(30)),
            outbound_max_frames: 4096,
            holder_build_id: format!("cm-holder/{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

/// Why [`Holder::serve`] returned. The holder's per-session state
/// survives the return; the caller (the binary's respawn loop, or a
/// test) starts the next brain generation and calls `serve` again.
#[derive(Debug, PartialEq)]
pub enum ServeOutcome {
    /// Orderly channel EOF — the brain exited.
    BrainEof,
    /// The brain broke protocol law; the channel is dead (law item
    /// 3). The caller SIGKILLs + reaps the brain and counts a
    /// failure.
    Protocol(String),
    /// The brain stopped draining the channel (S3) — same
    /// consequence as a missed-pong wedge.
    Wedged,
    /// No hello within the handshake timeout.
    HelloTimeout,
    /// Version ranges did not overlap; a `proto_mismatch` error was
    /// sent best-effort.
    HelloRefused,
}

/// A consumed-but-unforgotten exit.
struct ExitRec {
    exited_at: f64,
    /// The event awaiting `ack_exit`; `None` once acked.
    pending: Option<ch::ExitEventBody>,
}

/// One session child, canonically holder-owned.
struct SessionEntry {
    incarnation: u64,
    generation_meta: u64,
    pid: libc::pid_t,
    child_start_time: u64,
    /// Canonical master — its open file description keeps the PTY
    /// alive across brain generations.
    master: Box<dyn MasterPty + Send>,
    /// Canonical pidfd — reaping + signaling identity.
    pidfd: OwnedFd,
    reap_armed: bool,
    delivery_ready: bool,
    /// Exit-ready observed while un-armed: masked out of the poll
    /// set, status left unconsumed (zombie parked).
    exit_latched: bool,
    cgroup_prefix: Option<String>,
    /// Discovered scope path, delivered on `arm_reap` (V5).
    cgroup_path: Option<String>,
    watcher_checkpoint: Option<serde_json::Value>,
    last_signal_request: Option<ch::LastSignalRequest>,
    exit: Option<ExitRec>,
}

impl SessionEntry {
    fn master_raw_fd(&self) -> Option<RawFd> {
        self.master.as_raw_fd()
    }
}

/// One queued outbound frame (bytes + the fd dups that ride its
/// first byte). The dups are OWNED here and close on drop — after a
/// successful `sendmsg` the receiver holds its own copies.
struct OutFrame {
    bytes: Vec<u8>,
    fds: Vec<OwnedFd>,
    sent: usize,
}

/// The holder: all state that must survive brain restarts.
pub struct Holder {
    cfg: HolderConfig,
    sessions: BTreeMap<String, SessionEntry>,
    next_incarnation: u64,
    /// Brain-spawn counter (design § The holder); incremented per
    /// `serve` call — each serve is one brain generation.
    epoch: u64,
    ping_seq: u64,
    pings_unanswered: u64,
}

impl Holder {
    pub fn new(cfg: HolderConfig) -> Holder {
        Holder {
            cfg,
            sessions: BTreeMap::new(),
            next_incarnation: 1,
            epoch: 0,
            ping_seq: 0,
            pings_unanswered: 0,
        }
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    fn pending_exit_events(&self) -> usize {
        self.sessions
            .values()
            .filter(|e| e.exit.as_ref().is_some_and(|x| x.pending.is_some()))
            .count()
    }

    /// Serve one brain generation over `channel`. Returns when the
    /// channel dies; holder state (sessions, queued events) is
    /// retained for the next generation.
    pub fn serve(&mut self, channel: OwnedFd) -> ServeOutcome {
        self.epoch += 1;
        self.pings_unanswered = 0;
        for e in self.sessions.values_mut() {
            e.delivery_ready = false; // C9: re-armed per generation
        }
        set_nonblocking(&channel);

        let mut reader = FrameReader::new();
        let mut outbound: VecDeque<OutFrame> = VecDeque::new();
        let mut hello_done = false;
        let handshake_deadline = Instant::now() + self.cfg.handshake_timeout;
        let mut next_ping = self.cfg.ping_interval.map(|d| Instant::now() + d);

        loop {
            // ---- assemble the poll set ----
            let mut pfds: Vec<libc::pollfd> = Vec::with_capacity(1 + self.sessions.len());
            let mut events = libc::POLLIN;
            if !outbound.is_empty() {
                events |= libc::POLLOUT;
            }
            pfds.push(libc::pollfd {
                fd: channel.as_raw_fd(),
                events,
                revents: 0,
            });
            // uid per extra pollfd slot, parallel to pfds[1..].
            let mut slot_uids: Vec<String> = Vec::new();
            for (uid, e) in &self.sessions {
                if e.exit.is_none() && !e.exit_latched {
                    pfds.push(libc::pollfd {
                        fd: e.pidfd.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    });
                    slot_uids.push(uid.clone());
                }
            }

            // ---- timeout: handshake / ping, else a coarse tick ----
            let now = Instant::now();
            let mut deadline: Option<Instant> = None;
            if !hello_done {
                deadline = Some(handshake_deadline);
            }
            if let Some(np) = next_ping {
                deadline = Some(match deadline {
                    Some(d) => d.min(np),
                    None => np,
                });
            }
            let timeout_ms: i32 = match deadline {
                Some(d) => d
                    .saturating_duration_since(now)
                    .as_millis()
                    .min(60_000)
                    .max(1) as i32,
                None => 60_000,
            };

            // SAFETY: pfds is a valid array for the call's duration.
            let ret = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as u64, timeout_ms) };
            if ret < 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return ServeOutcome::Protocol(format!("poll: {err}"));
            }

            // ---- timers ----
            if !hello_done && Instant::now() >= handshake_deadline {
                return ServeOutcome::HelloTimeout;
            }
            if let Some(np) = next_ping {
                if hello_done && Instant::now() >= np {
                    self.ping_seq += 1;
                    self.pings_unanswered += 1;
                    push_frame(
                        &mut outbound,
                        Frame::new(verbs::PING, None, 0, ch::PingBody { seq: self.ping_seq }),
                        vec![],
                    );
                    next_ping = Some(Instant::now() + self.cfg.ping_interval.unwrap());
                }
            }

            // ---- child exits ----
            for (i, uid) in slot_uids.iter().enumerate() {
                let pfd = &pfds[1 + i];
                if pfd.revents & libc::POLLIN == 0 {
                    continue;
                }
                let Some(e) = self.sessions.get_mut(uid) else {
                    continue;
                };
                if e.reap_armed {
                    Self::consume_exit(uid, e, &mut outbound);
                } else {
                    // V7: latch + mask; zombie parked for /proc reads.
                    e.exit_latched = true;
                }
            }

            // ---- channel readable ----
            if pfds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                match reader.feed(channel.as_fd()) {
                    Ok(FeedStatus::Eof) => return ServeOutcome::BrainEof,
                    Ok(_) => {}
                    Err(v) => return ServeOutcome::Protocol(v.0),
                }
                loop {
                    match reader.next_frame() {
                        Ok(Some((frame, fds))) => {
                            // Phase-2 verbs carry no brain→holder fds.
                            drop(fds);
                            match self.dispatch(frame, &mut hello_done, &mut outbound) {
                                Ok(()) => {}
                                Err(out) => {
                                    // Best-effort: the refusal frame
                                    // (e.g. proto_mismatch) should
                                    // reach the brain before the
                                    // channel drops.
                                    best_effort_flush(&channel, &mut outbound);
                                    return out;
                                }
                            }
                        }
                        Ok(None) => break,
                        Err(v) => return ServeOutcome::Protocol(v.0),
                    }
                }
            }

            // ---- flush outbound ----
            match flush_outbound(&channel, &mut outbound) {
                Ok(()) => {}
                Err(FlushError::PeerGone) => return ServeOutcome::BrainEof,
                Err(FlushError::Fatal(msg)) => return ServeOutcome::Protocol(msg),
            }
            if outbound.len() > self.cfg.outbound_max_frames {
                return ServeOutcome::Wedged;
            }
        }
    }

    /// Consume a ready exit under arm authorization: `waitid`, stamp
    /// the holder clock, take the `memory.events` carve-out snapshot,
    /// queue the event (delivered now iff `delivery_ready`).
    fn consume_exit(uid: &str, e: &mut SessionEntry, outbound: &mut VecDeque<OutFrame>) {
        let st = reap::consume_exit_status(&e.pidfd, e.pid);
        let exited_at = spawn::unix_now();
        let snapshot = e
            .cgroup_path
            .as_deref()
            .and_then(spawn::read_memory_events_snapshot);
        let event = ch::ExitEventBody {
            uid: uid.to_string(),
            incarnation: e.incarnation,
            code: st.code,
            signal: st.signal,
            exited_at,
            last_signal_request: e.last_signal_request.clone(),
            memory_events_snapshot: snapshot,
        };
        if e.delivery_ready {
            push_frame(
                outbound,
                Frame::new(verbs::EXIT_EVENT, None, 0, event.clone()),
                vec![],
            );
        }
        e.exit = Some(ExitRec {
            exited_at,
            pending: Some(event),
        });
        e.exit_latched = false;
    }

    /// Handle one inbound frame. `Err(outcome)` aborts the serve.
    fn dispatch(
        &mut self,
        frame: Frame,
        hello_done: &mut bool,
        outbound: &mut VecDeque<OutFrame>,
    ) -> Result<(), ServeOutcome> {
        // pong is the one req_id-less brain frame (C7's law).
        if frame.v == verbs::PONG {
            self.pings_unanswered = 0;
            return Ok(());
        }
        let Some(req_id) = frame.req_id else {
            return Err(ServeOutcome::Protocol(format!(
                "brain frame '{}' without req_id",
                frame.v
            )));
        };

        if !*hello_done {
            if frame.v != verbs::HELLO {
                return Err(ServeOutcome::Protocol(format!(
                    "first frame must be hello, got '{}'",
                    frame.v
                )));
            }
            let hello: ch::HelloBody = match frame.parse_body() {
                Ok(b) => b,
                Err(e) => return Err(ServeOutcome::Protocol(format!("hello body: {e}"))),
            };
            let lo = hello.proto_min.max(ch::PROTO_VERSION_MIN);
            let hi = hello.proto_max.min(ch::PROTO_VERSION_MAX);
            if lo > hi {
                push_frame(
                    outbound,
                    err_frame(
                        req_id,
                        ch::ERR_PROTO_MISMATCH,
                        format!(
                            "brain speaks {}..={}, holder {}..={}",
                            hello.proto_min,
                            hello.proto_max,
                            ch::PROTO_VERSION_MIN,
                            ch::PROTO_VERSION_MAX
                        ),
                    ),
                    vec![],
                );
                return Err(ServeOutcome::HelloRefused);
            }
            push_frame(
                outbound,
                Frame::new(
                    verbs::HELLO_REPLY,
                    Some(req_id),
                    0,
                    ch::HelloReplyBody {
                        proto: hi,
                        holder_build_id: self.cfg.holder_build_id.clone(),
                        holder_proto_min: ch::PROTO_VERSION_MIN,
                        holder_proto_max: ch::PROTO_VERSION_MAX,
                        epoch: self.epoch,
                        session_count: self.sessions.len(),
                    },
                ),
                vec![],
            );
            *hello_done = true;
            return Ok(());
        }

        match frame.v.as_str() {
            verbs::SPAWN => self.handle_spawn(req_id, &frame, outbound),
            verbs::ARM_REAP => self.handle_arm_reap(req_id, &frame, outbound),
            verbs::ADOPT => self.handle_adopt(req_id, outbound),
            verbs::SIGNAL => self.handle_signal(req_id, &frame, outbound),
            verbs::ABORT_SPAWN => self.handle_abort_spawn(req_id, &frame, outbound),
            verbs::FORGET => self.handle_forget(req_id, &frame, outbound),
            verbs::ACK_EXIT => self.handle_ack_exit(req_id, &frame, outbound),
            verbs::STATUS => {
                push_frame(
                    outbound,
                    Frame::new(
                        verbs::OK,
                        Some(req_id),
                        0,
                        ch::StatusReplyBody {
                            sessions: self.sessions.len(),
                            pending_exit_events: self.pending_exit_events(),
                            epoch: self.epoch,
                            brain_restarts: self.epoch.saturating_sub(1),
                            breaker_state: "none".into(),
                            holder_build_id: self.cfg.holder_build_id.clone(),
                            pings_unanswered: self.pings_unanswered,
                        },
                    ),
                    vec![],
                );
                Ok(())
            }
            verbs::HELLO => Err(ServeOutcome::Protocol("duplicate hello".into())),
            other => {
                // Additive-tolerance: unknown verbs get a typed
                // error, never a disconnect.
                push_frame(
                    outbound,
                    err_frame(
                        req_id,
                        ch::ERR_UNSUPPORTED_VERB,
                        format!("verb '{other}' is not supported by this holder"),
                    ),
                    vec![],
                );
                Ok(())
            }
        }
    }

    fn handle_spawn(
        &mut self,
        req_id: u64,
        frame: &Frame,
        outbound: &mut VecDeque<OutFrame>,
    ) -> Result<(), ServeOutcome> {
        let spec: ch::SpawnBody = match frame.parse_body() {
            Ok(b) => b,
            Err(e) => {
                push_frame(outbound, err_frame(req_id, ch::ERR_INVALID, e), vec![]);
                return Ok(());
            }
        };
        if let Some(existing) = self.sessions.get(&spec.uid) {
            push_frame(
                outbound,
                err_frame_with(
                    req_id,
                    ch::ERR_UID_EXISTS,
                    format!("uid '{}' already has a record", spec.uid),
                    serde_json::json!({ "incarnation": existing.incarnation }),
                ),
                vec![],
            );
            return Ok(());
        }
        let spawned = match spawn::do_spawn(&spec) {
            Ok(s) => s,
            Err(e) => {
                let code = match &e {
                    spawn::SpawnError::Openpty(_) => ch::ERR_OPENPTY_FAILED,
                    spawn::SpawnError::EmptyArgv => ch::ERR_INVALID,
                    _ => ch::ERR_EXEC_FAILED,
                };
                push_frame(outbound, err_frame(req_id, code, e.to_string()), vec![]);
                return Ok(());
            }
        };
        let incarnation = self.next_incarnation;
        self.next_incarnation += 1;

        let Some(master_raw) = spawned.master.as_raw_fd() else {
            // Cannot mint dups — tear down (the abort discipline).
            let _ = reap::pidfd_send_signal(&spawned.pidfd, libc::SIGKILL);
            let _ = reap::consume_exit_status(&spawned.pidfd, spawned.pid);
            push_frame(
                outbound,
                err_frame(req_id, ch::ERR_EXEC_FAILED, "master exposes no raw fd".into()),
                vec![],
            );
            return Ok(());
        };
        let (mdup, pdup) = match (reap::dup_cloexec(master_raw), reap::dup_cloexec(spawned.pidfd.as_raw_fd())) {
            (Ok(m), Ok(p)) => (m, p),
            _ => {
                let _ = reap::pidfd_send_signal(&spawned.pidfd, libc::SIGKILL);
                let _ = reap::consume_exit_status(&spawned.pidfd, spawned.pid);
                push_frame(
                    outbound,
                    err_frame(req_id, ch::ERR_EXEC_FAILED, "fd dup failed".into()),
                    vec![],
                );
                return Ok(());
            }
        };

        push_frame(
            outbound,
            Frame::new(
                verbs::OK,
                Some(req_id),
                2,
                ch::SpawnOkBody {
                    incarnation,
                    pid: spawned.pid,
                    child_start_time: spawned.child_start_time,
                },
            ),
            vec![mdup, pdup],
        );
        self.sessions.insert(
            spec.uid.clone(),
            SessionEntry {
                incarnation,
                generation_meta: spec.generation_meta,
                pid: spawned.pid,
                child_start_time: spawned.child_start_time,
                master: spawned.master,
                pidfd: spawned.pidfd,
                reap_armed: false,
                delivery_ready: false,
                exit_latched: false,
                cgroup_prefix: spec.cgroup_prefix.clone(),
                cgroup_path: None,
                watcher_checkpoint: None,
                last_signal_request: None,
                exit: None,
            },
        );
        Ok(())
    }

    fn handle_arm_reap(
        &mut self,
        req_id: u64,
        frame: &Frame,
        outbound: &mut VecDeque<OutFrame>,
    ) -> Result<(), ServeOutcome> {
        let body: ch::ArmReapBody = match frame.parse_body() {
            Ok(b) => b,
            Err(e) => {
                push_frame(outbound, err_frame(req_id, ch::ERR_INVALID, e), vec![]);
                return Ok(());
            }
        };
        let uid = body.uid.clone();
        let Some(e) = self.sessions.get_mut(&uid) else {
            push_frame(
                outbound,
                err_frame(req_id, ch::ERR_NOT_FOUND, format!("no record for '{uid}'")),
                vec![],
            );
            return Ok(());
        };
        if e.incarnation != body.incarnation {
            push_frame(
                outbound,
                err_frame(
                    req_id,
                    ch::ERR_NOT_FOUND,
                    format!("incarnation mismatch for '{uid}'"),
                ),
                vec![],
            );
            return Ok(());
        }
        e.reap_armed = true;
        e.delivery_ready = true;
        if body.cgroup_path.is_some() {
            e.cgroup_path = body.cgroup_path.clone();
        }
        push_frame(
            outbound,
            Frame::new(verbs::OK, Some(req_id), 0, ch::OkBody { detail: None }),
            vec![],
        );
        // A latched (exited-while-unarmed) child is consumable now;
        // an already-consumed unacked event redelivers now.
        if e.exit_latched && e.exit.is_none() {
            Self::consume_exit(&uid, e, outbound);
        } else if let Some(rec) = &e.exit {
            if let Some(ev) = &rec.pending {
                push_frame(
                    outbound,
                    Frame::new(verbs::EXIT_EVENT, None, 0, ev.clone()),
                    vec![],
                );
            }
        }
        Ok(())
    }

    fn handle_adopt(
        &mut self,
        req_id: u64,
        outbound: &mut VecDeque<OutFrame>,
    ) -> Result<(), ServeOutcome> {
        for (uid, e) in &self.sessions {
            let (mdup, pdup) = match (
                e.master_raw_fd().and_then(|r| reap::dup_cloexec(r).ok()),
                reap::dup_cloexec(e.pidfd.as_raw_fd()).ok(),
            ) {
                (Some(m), Some(p)) => (m, p),
                _ => {
                    // A record whose fds can't dup is unadoptable;
                    // surface it honestly rather than skipping
                    // silently. (Should be unreachable — the fds are
                    // holder-owned and open.)
                    push_frame(
                        outbound,
                        err_frame(
                            req_id,
                            ch::ERR_INVALID,
                            format!("record '{uid}': fd dup failed during adopt"),
                        ),
                        vec![],
                    );
                    continue;
                }
            };
            push_frame(
                outbound,
                Frame::new(
                    verbs::ADOPT_RECORD,
                    Some(req_id),
                    2,
                    ch::AdoptRecordBody {
                        uid: uid.clone(),
                        incarnation: e.incarnation,
                        generation_meta: e.generation_meta,
                        child_pid: e.pid,
                        child_start_time: e.child_start_time,
                        cgroup_prefix: e.cgroup_prefix.clone(),
                        cgroup_path: e.cgroup_path.clone(),
                        watcher_checkpoint: e.watcher_checkpoint.clone(),
                        last_signal_request: e.last_signal_request.clone(),
                        reap_armed: e.reap_armed,
                        reaped: e.exit.is_some(),
                        exit_event_pending: e
                            .exit
                            .as_ref()
                            .is_some_and(|x| x.pending.is_some()),
                    },
                ),
                vec![mdup, pdup],
            );
        }
        // Phase 4 fills this; the shape ships now (0 listeners, 0 fds).
        push_frame(
            outbound,
            Frame::new(
                verbs::ADOPT_LISTENERS,
                Some(req_id),
                0,
                ch::AdoptListenersBody { listeners: vec![] },
            ),
            vec![],
        );
        push_frame(
            outbound,
            Frame::new(
                verbs::ADOPT_DONE,
                Some(req_id),
                0,
                ch::AdoptDoneBody {
                    exit_events_pending: self.pending_exit_events(),
                },
            ),
            vec![],
        );
        Ok(())
    }

    fn handle_signal(
        &mut self,
        req_id: u64,
        frame: &Frame,
        outbound: &mut VecDeque<OutFrame>,
    ) -> Result<(), ServeOutcome> {
        let body: ch::SignalBody = match frame.parse_body() {
            Ok(b) => b,
            Err(e) => {
                push_frame(outbound, err_frame(req_id, ch::ERR_INVALID, e), vec![]);
                return Ok(());
            }
        };
        let Some(e) = self.sessions.get_mut(&body.uid) else {
            push_frame(
                outbound,
                err_frame(req_id, ch::ERR_NOT_FOUND, format!("no record for '{}'", body.uid)),
                vec![],
            );
            return Ok(());
        };
        if e.incarnation != body.incarnation {
            push_frame(
                outbound,
                err_frame(req_id, ch::ERR_NOT_FOUND, "incarnation mismatch".into()),
                vec![],
            );
            return Ok(());
        }
        // C13 ordering: exit knowledge first (reaped / latched /
        // exit-ready probe — a pidfd signal "succeeds" against a
        // zombie), then signal, then stamp only on live delivery.
        let already_exited = e.exit.is_some() || e.exit_latched || reap::pidfd_exit_ready(&e.pidfd);
        if already_exited {
            push_frame(
                outbound,
                err_frame_with(
                    req_id,
                    ch::ERR_ALREADY_EXITED,
                    format!("'{}' already exited", body.uid),
                    serde_json::json!({
                        "exited_at": e.exit.as_ref().map(|x| x.exited_at)
                    }),
                ),
                vec![],
            );
            return Ok(());
        }
        match reap::pidfd_send_signal(&e.pidfd, body.sig) {
            Ok(true) => {
                e.last_signal_request = Some(ch::LastSignalRequest {
                    sig: body.sig,
                    attribution: body.attribution.clone(),
                    at: spawn::unix_now(),
                });
                push_frame(
                    outbound,
                    Frame::new(verbs::OK, Some(req_id), 0, ch::OkBody { detail: None }),
                    vec![],
                );
            }
            Ok(false) => {
                push_frame(
                    outbound,
                    err_frame(req_id, ch::ERR_ALREADY_EXITED, "ESRCH".into()),
                    vec![],
                );
            }
            Err(err) => {
                push_frame(
                    outbound,
                    err_frame(req_id, ch::ERR_INVALID, format!("pidfd_send_signal: {err}")),
                    vec![],
                );
            }
        }
        Ok(())
    }

    fn handle_abort_spawn(
        &mut self,
        req_id: u64,
        frame: &Frame,
        outbound: &mut VecDeque<OutFrame>,
    ) -> Result<(), ServeOutcome> {
        let body: ch::AbortSpawnBody = match frame.parse_body() {
            Ok(b) => b,
            Err(e) => {
                push_frame(outbound, err_frame(req_id, ch::ERR_INVALID, e), vec![]);
                return Ok(());
            }
        };
        match self.sessions.get(&body.uid) {
            Some(e) if e.incarnation == body.incarnation => {}
            _ => {
                push_frame(
                    outbound,
                    err_frame(req_id, ch::ERR_NOT_FOUND, format!("no record for '{}'", body.uid)),
                    vec![],
                );
                return Ok(());
            }
        }
        let e = self.sessions.remove(&body.uid).expect("checked above");
        // SIGKILL + unconditional waitid (armed or not), NO event —
        // the PendingSession-abort translation (S4/O6).
        let _ = reap::pidfd_send_signal(&e.pidfd, libc::SIGKILL);
        let _ = reap::consume_exit_status(&e.pidfd, e.pid);
        push_frame(
            outbound,
            Frame::new(verbs::OK, Some(req_id), 0, ch::OkBody { detail: None }),
            vec![],
        );
        Ok(())
    }

    fn handle_forget(
        &mut self,
        req_id: u64,
        frame: &Frame,
        outbound: &mut VecDeque<OutFrame>,
    ) -> Result<(), ServeOutcome> {
        let body: ch::ForgetBody = match frame.parse_body() {
            Ok(b) => b,
            Err(e) => {
                push_frame(outbound, err_frame(req_id, ch::ERR_INVALID, e), vec![]);
                return Ok(());
            }
        };
        let reply = match self.sessions.get(&body.uid) {
            None => err_frame(req_id, ch::ERR_NOT_FOUND, format!("no record for '{}'", body.uid)),
            Some(e) if e.incarnation != body.incarnation => {
                err_frame(req_id, ch::ERR_NOT_FOUND, "incarnation mismatch".into())
            }
            Some(e) if e.exit.is_none() => err_frame(
                req_id,
                ch::ERR_NOT_EXITED,
                // The S4/O6 rule: forgetting a live child would leak
                // an unreapable zombie in the one process that never
                // dies.
                format!("'{}' is still alive — forget is post-exit GC only", body.uid),
            ),
            Some(e) if e.exit.as_ref().is_some_and(|x| x.pending.is_some()) => err_frame(
                req_id,
                ch::ERR_UNACKED,
                format!("'{}' has an unacked exit event — ack_exit first", body.uid),
            ),
            Some(_) => {
                self.sessions.remove(&body.uid);
                Frame::new(verbs::OK, Some(req_id), 0, ch::OkBody { detail: None })
            }
        };
        push_frame(outbound, reply, vec![]);
        Ok(())
    }

    fn handle_ack_exit(
        &mut self,
        req_id: u64,
        frame: &Frame,
        outbound: &mut VecDeque<OutFrame>,
    ) -> Result<(), ServeOutcome> {
        let body: ch::AckExitBody = match frame.parse_body() {
            Ok(b) => b,
            Err(e) => {
                push_frame(outbound, err_frame(req_id, ch::ERR_INVALID, e), vec![]);
                return Ok(());
            }
        };
        // Idempotent: an ack for an unknown/already-acked key is Ok
        // (the C4 replay flow re-acks after tombstone-match).
        // `known` = an event was actually pending and this ack
        // consumed it; a replayed ack (the C4 tombstone-match flow)
        // is Ok with known=false.
        let known = match self.sessions.get_mut(&body.uid) {
            Some(e) if e.incarnation == body.incarnation => e
                .exit
                .as_mut()
                .is_some_and(|rec| rec.pending.take().is_some()),
            _ => false,
        };
        push_frame(
            outbound,
            Frame::new(
                verbs::OK,
                Some(req_id),
                0,
                ch::OkBody {
                    detail: Some(serde_json::json!({ "known": known })),
                },
            ),
            vec![],
        );
        Ok(())
    }
}

// ============================================================
// Outbound helpers
// ============================================================

fn push_frame(outbound: &mut VecDeque<OutFrame>, frame: Frame, fds: Vec<OwnedFd>) {
    debug_assert_eq!(frame.nfds as usize, fds.len());
    outbound.push_back(OutFrame {
        bytes: frame.to_wire(),
        fds,
        sent: 0,
    });
}

fn err_frame(req_id: u64, code: &str, message: String) -> Frame {
    Frame::new(
        verbs::ERR,
        Some(req_id),
        0,
        ch::ErrBody {
            code: code.to_string(),
            message,
            detail: None,
        },
    )
}

fn err_frame_with(req_id: u64, code: &str, message: String, detail: serde_json::Value) -> Frame {
    Frame::new(
        verbs::ERR,
        Some(req_id),
        0,
        ch::ErrBody {
            code: code.to_string(),
            message,
            detail: Some(detail),
        },
    )
}

enum FlushError {
    PeerGone,
    Fatal(String),
}

/// Drain as much of the outbound queue as the socket accepts —
/// strictly nonblocking (S3).
fn flush_outbound(
    channel: &OwnedFd,
    outbound: &mut VecDeque<OutFrame>,
) -> Result<(), FlushError> {
    while let Some(front) = outbound.front_mut() {
        let res = if front.sent == 0 {
            let raw: Vec<RawFd> = front.fds.iter().map(|f| f.as_raw_fd()).collect();
            ch::send_first(channel.as_fd(), &front.bytes, &raw)
        } else {
            ch::send_rest(channel.as_fd(), &front.bytes[front.sent..])
        };
        match res {
            Ok(n) => {
                if front.sent == 0 {
                    // The fds traveled with the first segment; the
                    // receiver owns copies now — drop ours.
                    front.fds.clear();
                }
                front.sent += n;
                if front.sent >= front.bytes.len() {
                    outbound.pop_front();
                }
            }
            Err(e) => {
                return match e.raw_os_error() {
                    Some(libc::EAGAIN) | Some(libc::EINTR) => Ok(()),
                    Some(libc::EPIPE) | Some(libc::ECONNRESET) => Err(FlushError::PeerGone),
                    _ => Err(FlushError::Fatal(format!("channel send: {e}"))),
                }
            }
        }
    }
    Ok(())
}

/// Drain the outbound queue for up to ~200ms before an abnormal
/// serve return — refusal/error frames should reach a live brain,
/// but a dead one must not delay the return.
fn best_effort_flush(channel: &OwnedFd, outbound: &mut VecDeque<OutFrame>) {
    let deadline = Instant::now() + Duration::from_millis(200);
    while !outbound.is_empty() && Instant::now() < deadline {
        match flush_outbound(channel, outbound) {
            Ok(()) => {
                if !outbound.is_empty() {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            Err(_) => return,
        }
    }
}

fn set_nonblocking(fd: &OwnedFd) {
    // SAFETY: plain fcntl on an owned fd.
    unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}
