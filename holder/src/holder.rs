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
    /// An extra fd (the binary's signalfd) added to the poll set;
    /// when readable, the caller's `on_extra` callback runs (see
    /// [`Holder::serve`]). `None` in tests.
    pub extra_fd: Option<RawFd>,
}

/// A point-in-time view for the signal callback (SIGUSR1 status
/// dumps must not borrow the holder while `serve` holds it).
#[derive(Debug, Clone)]
pub struct StatusSnapshot {
    pub sessions: usize,
    pub pending_exit_events: usize,
    pub epoch: u64,
}

/// What the signal callback wants done.
#[derive(Debug, PartialEq)]
pub enum SignalDirective {
    /// Keep serving this brain generation.
    Continue,
    /// Begin the stop-everything sequence ([`ServeOutcome`] returns
    /// `ShutdownRequested`; the binary forwards SIGTERM to the brain,
    /// then runs [`Holder::shutdown_kill_all`]).
    Shutdown,
}

impl Default for HolderConfig {
    fn default() -> Self {
        HolderConfig {
            handshake_timeout: Duration::from_secs(30),
            ping_interval: Some(Duration::from_secs(30)),
            outbound_max_frames: 4096,
            holder_build_id: format!("cm-holder/{}", env!("CARGO_PKG_VERSION")),
            extra_fd: None,
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
    /// The brain is wedged: it stopped draining the channel (S3) or
    /// missed [`WEDGE_MISSED_PONGS`] consecutive pings (§ Supervision
    /// — the watchdog consequence). The caller SIGKILLs + reaps the
    /// brain and counts a breaker strike.
    Wedged(String),
    /// No hello within the handshake timeout.
    HelloTimeout,
    /// Version ranges did not overlap; a `proto_mismatch` error was
    /// sent best-effort.
    HelloRefused,
    /// The signal callback asked for the stop-everything sequence
    /// (SIGTERM/SIGINT via the binary's signalfd).
    ShutdownRequested,
}

/// Consecutive unanswered pings that mean the brain is wedged —
/// pong is lock-free by spec (S9), so a miss is never a state-lock
/// convoy; three misses at the ping cadence is the doc's 90s horizon
/// at the default 30s interval.
pub const WEDGE_MISSED_PONGS: u64 = 3;

/// An armed deploy action (§ Brain deploys, C8's arm-late rule):
/// stored by `restart_brain`/`rollback_brain` and consumed by the
/// supervisor when the brain exits. Auto-disarmed if the brain is
/// still alive [`ARM_AUTO_DISARM`] after arming.
pub enum ArmedDeploy {
    /// Exec this pinned fd as the next brain (`restart_brain`).
    NewPin(OwnedFd),
    /// Exec the previous pin (`rollback_brain`).
    UsePrevious,
}

/// C8's auto-disarm horizon: a brain that armed a deploy and then
/// did NOT exit within this window forfeits the arm — an unrelated
/// later crash must not trigger a stale deploy.
pub const ARM_AUTO_DISARM: Duration = Duration::from_secs(30);

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
    /// Custodied listeners (design § Listeners, O11): the BRAIN
    /// binds; the holder keeps the fds alive across brain
    /// generations so the socket never unbinds. At most one per
    /// kind ("unix"/"tls"); a re-store replaces (the old fd closes).
    listeners: Vec<(ch::ListenerMeta, OwnedFd)>,
    /// An armed deploy (C8's arm-late rule): stored by
    /// `restart_brain`/`rollback_brain`, consumed by the supervisor
    /// when the brain exits, auto-disarmed after [`ARM_AUTO_DISARM`]
    /// if the brain is still alive.
    armed_deploy: Option<(ArmedDeploy, Instant)>,
    /// Supervisor-published facts for `status` replies (the breaker
    /// lives in the binary's supervisor; the holder just reports).
    breaker_label: String,
    previous_pin_available: bool,
    next_incarnation: u64,
    /// Brain-spawn counter (design § The holder); incremented per
    /// `serve` call — each serve is one brain generation.
    epoch: u64,
    ping_seq: u64,
    pings_unanswered: u64,
    /// Whether the CURRENT generation completed hello — the
    /// breaker's stability input (a long-lived brain that never
    /// negotiated is not stable).
    helloed_this_generation: bool,
}

impl Holder {
    pub fn new(cfg: HolderConfig) -> Holder {
        Holder {
            cfg,
            sessions: BTreeMap::new(),
            listeners: Vec::new(),
            armed_deploy: None,
            breaker_label: "running".into(),
            previous_pin_available: false,
            next_incarnation: 1,
            epoch: 0,
            ping_seq: 0,
            pings_unanswered: 0,
            helloed_this_generation: false,
        }
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// The supervisor publishes its state for `status` replies.
    pub fn set_supervisor_status(&mut self, breaker: &str, previous_pin: bool) {
        self.breaker_label = breaker.to_string();
        self.previous_pin_available = previous_pin;
    }

    /// Consume the armed deploy (called by the supervisor when the
    /// brain exits): `Some` means the exit was a DEPLOY, not a crash.
    pub fn take_armed_deploy(&mut self) -> Option<ArmedDeploy> {
        self.armed_deploy.take().map(|(a, _)| a)
    }

    /// Whether the current (most recent) generation completed hello
    /// — the breaker's stability input.
    pub fn generation_helloed(&self) -> bool {
        self.helloed_this_generation
    }

    /// The custodied control socket's path (for shutdown-unlink).
    pub fn custodied_unix_path(&self) -> Option<String> {
        self.listeners
            .iter()
            .find(|(m, _)| m.kind == "unix")
            .map(|(m, _)| m.meta.clone())
    }

    /// The stop-everything executor (§ Supervision, S7): SIGKILL and
    /// reap every held session child via the canonical pidfds —
    /// children that ignore the PTY-teardown HUP must not outlive
    /// the supervisor. Returns how many were signaled.
    pub fn shutdown_kill_all(&mut self) -> usize {
        let mut killed = 0usize;
        for (uid, e) in std::mem::take(&mut self.sessions) {
            if e.exit.is_none() {
                let _ = reap::pidfd_send_signal(&e.pidfd, libc::SIGKILL);
                let _ = reap::consume_exit_status(&e.pidfd, e.pid);
                killed += 1;
                eprintln!("cm-holder: shutdown — killed + reaped session '{uid}'");
            }
        }
        killed
    }

    fn pending_exit_events(&self) -> usize {
        self.sessions
            .values()
            .filter(|e| e.exit.as_ref().is_some_and(|x| x.pending.is_some()))
            .count()
    }

    /// Serve one brain generation over `channel`. Returns when the
    /// channel dies (or a signal asks for shutdown); holder state
    /// (sessions, queued events, custody, armed deploys) is retained
    /// for the next generation. `on_extra` runs when the configured
    /// `extra_fd` (the binary's signalfd) is readable — it must
    /// consume the readability itself.
    pub fn serve(
        &mut self,
        channel: OwnedFd,
        mut on_extra: Option<&mut dyn FnMut(&StatusSnapshot) -> SignalDirective>,
    ) -> ServeOutcome {
        self.epoch += 1;
        self.pings_unanswered = 0;
        self.helloed_this_generation = false;
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
            // Optional signalfd slot (index 1 when present).
            let extra_slot = self.cfg.extra_fd.map(|fd| {
                pfds.push(libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                });
                pfds.len() - 1
            });
            let session_base = pfds.len();
            // uid per session pollfd slot, parallel to
            // pfds[session_base..].
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
            if let Some((_, armed_at)) = &self.armed_deploy {
                let dis = *armed_at + ARM_AUTO_DISARM;
                deadline = Some(match deadline {
                    Some(d) => d.min(dis),
                    None => dis,
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
            // C8's auto-disarm: an armed deploy whose brain did NOT
            // exit within the horizon forfeits the arm — a later
            // unrelated crash must not trigger a stale deploy.
            if let Some((_, armed_at)) = &self.armed_deploy {
                if Instant::now() >= *armed_at + ARM_AUTO_DISARM {
                    eprintln!(
                        "cm-holder: armed deploy expired after {:?} without a \
                         brain exit — auto-disarmed (C8)",
                        ARM_AUTO_DISARM
                    );
                    self.armed_deploy = None;
                }
            }
            if let Some(np) = next_ping {
                if hello_done && Instant::now() >= np {
                    self.ping_seq += 1;
                    self.pings_unanswered += 1;
                    if self.pings_unanswered >= WEDGE_MISSED_PONGS {
                        // The watchdog consequence (§ Supervision):
                        // pong is lock-free by spec (S9), so a miss
                        // is a real wedge, never a state-lock convoy.
                        return ServeOutcome::Wedged(format!(
                            "{} consecutive pings unanswered",
                            self.pings_unanswered
                        ));
                    }
                    push_frame(
                        &mut outbound,
                        Frame::new(verbs::PING, None, 0, ch::PingBody { seq: self.ping_seq }),
                        vec![],
                    );
                    next_ping = Some(Instant::now() + self.cfg.ping_interval.unwrap());
                }
            }

            // ---- signalfd ----
            if let Some(slot) = extra_slot {
                if pfds[slot].revents & libc::POLLIN != 0 {
                    let snapshot = StatusSnapshot {
                        sessions: self.sessions.len(),
                        pending_exit_events: self.pending_exit_events(),
                        epoch: self.epoch,
                    };
                    if let Some(cb) = on_extra.as_deref_mut() {
                        if cb(&snapshot) == SignalDirective::Shutdown {
                            best_effort_flush(&channel, &mut outbound);
                            return ServeOutcome::ShutdownRequested;
                        }
                    }
                }
            }

            // ---- child exits ----
            for (i, uid) in slot_uids.iter().enumerate() {
                let pfd = &pfds[session_base + i];
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
                            match self.dispatch(frame, fds, &mut hello_done, &mut outbound) {
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
                return ServeOutcome::Wedged(format!(
                    "outbound queue exceeded {} frames — the brain stopped draining (S3)",
                    self.cfg.outbound_max_frames
                ));
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
        fds: Vec<OwnedFd>,
        hello_done: &mut bool,
        outbound: &mut VecDeque<OutFrame>,
    ) -> Result<(), ServeOutcome> {
        // The brain→holder fd-bearing verbs; everything else must
        // arrive fd-free (undeclared fds already violated at the
        // frame layer — this guards declared-but-wrong-verb).
        if frame.v != verbs::STORE_LISTENER
            && frame.v != verbs::RESTART_BRAIN
            && !fds.is_empty()
        {
            return Err(ServeOutcome::Protocol(format!(
                "verb '{}' carried {} unexpected fd(s)",
                frame.v,
                fds.len()
            )));
        }
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
            self.helloed_this_generation = true;
            return Ok(());
        }

        match frame.v.as_str() {
            verbs::SPAWN => self.handle_spawn(req_id, &frame, outbound),
            verbs::ARM_REAP => self.handle_arm_reap(req_id, &frame, outbound),
            verbs::ADOPT => self.handle_adopt(req_id, outbound),
            verbs::SIGNAL => self.handle_signal(req_id, &frame, outbound),
            verbs::ABORT_SPAWN => self.handle_abort_spawn(req_id, &frame, outbound),
            verbs::UPDATE_CHECKPOINT => self.handle_update_checkpoint(req_id, &frame, outbound),
            verbs::STORE_LISTENER => self.handle_store_listener(req_id, &frame, fds, outbound),
            verbs::RESTART_BRAIN => {
                // C8's arm-late rule is the BRAIN's obligation (send
                // only after quiesce + checked persistence); the
                // holder's side: store the pin, reply ok, and expect
                // an exit within the auto-disarm horizon.
                if fds.len() != 1 {
                    return Err(ServeOutcome::Protocol(format!(
                        "restart_brain carried {} fds, want 1 (the pinned brain binary)",
                        fds.len()
                    )));
                }
                let pin = fds.into_iter().next().expect("len checked");
                self.armed_deploy = Some((ArmedDeploy::NewPin(pin), Instant::now()));
                eprintln!("cm-holder: restart_brain armed (new pinned brain fd)");
                push_frame(
                    outbound,
                    Frame::new(verbs::OK, Some(req_id), 0, ch::OkBody { detail: None }),
                    vec![],
                );
                Ok(())
            }
            verbs::ROLLBACK_BRAIN => {
                if !self.previous_pin_available {
                    push_frame(
                        outbound,
                        err_frame(
                            req_id,
                            ch::ERR_NOT_FOUND,
                            "no previous brain pin to roll back to".into(),
                        ),
                        vec![],
                    );
                    return Ok(());
                }
                self.armed_deploy = Some((ArmedDeploy::UsePrevious, Instant::now()));
                eprintln!("cm-holder: rollback_brain armed (previous pin)");
                push_frame(
                    outbound,
                    Frame::new(verbs::OK, Some(req_id), 0, ch::OkBody { detail: None }),
                    vec![],
                );
                Ok(())
            }
            verbs::CANCEL_PENDING => {
                let had = self.armed_deploy.take().is_some();
                push_frame(
                    outbound,
                    Frame::new(
                        verbs::OK,
                        Some(req_id),
                        0,
                        ch::OkBody {
                            detail: Some(serde_json::json!({ "disarmed": had })),
                        },
                    ),
                    vec![],
                );
                Ok(())
            }
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
                            breaker_state: self.breaker_label.clone(),
                            holder_build_id: self.cfg.holder_build_id.clone(),
                            pings_unanswered: self.pings_unanswered,
                            previous_pin: self.previous_pin_available,
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
        // OOM posture (S11): the child must not inherit a
        // systemd-protected holder's negative score.
        reap::oom_score_adj_zero(spawned.pid);

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
        // Custodied listeners ride back out as dups, one fd per
        // entry, fds in list order (O11).
        let mut listener_metas = Vec::new();
        let mut listener_dups = Vec::new();
        for (meta, fd) in &self.listeners {
            match reap::dup_cloexec(fd.as_raw_fd()) {
                Ok(dup) => {
                    listener_metas.push(meta.clone());
                    listener_dups.push(dup);
                }
                Err(e) => {
                    push_frame(
                        outbound,
                        err_frame(
                            req_id,
                            ch::ERR_INVALID,
                            format!("listener '{}' dup failed during adopt: {e}", meta.kind),
                        ),
                        vec![],
                    );
                }
            }
        }
        let nfds = listener_dups.len() as u8;
        push_frame(
            outbound,
            Frame::new(
                verbs::ADOPT_LISTENERS,
                Some(req_id),
                nfds,
                ch::AdoptListenersBody {
                    listeners: listener_metas,
                },
            ),
            listener_dups,
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

    fn handle_update_checkpoint(
        &mut self,
        req_id: u64,
        frame: &Frame,
        outbound: &mut VecDeque<OutFrame>,
    ) -> Result<(), ServeOutcome> {
        let body: ch::UpdateCheckpointBody = match frame.parse_body() {
            Ok(b) => b,
            Err(e) => {
                push_frame(outbound, err_frame(req_id, ch::ERR_INVALID, e), vec![]);
                return Ok(());
            }
        };
        let reply = match self.sessions.get_mut(&body.uid) {
            Some(e) if e.incarnation == body.incarnation => {
                // Opaque by contract (frozenness): stored, never
                // interpreted; rides the adopt record back out.
                e.watcher_checkpoint = Some(body.watcher_checkpoint);
                Frame::new(verbs::OK, Some(req_id), 0, ch::OkBody { detail: None })
            }
            _ => err_frame(
                req_id,
                ch::ERR_NOT_FOUND,
                format!("no record for '{}'", body.uid),
            ),
        };
        push_frame(outbound, reply, vec![]);
        Ok(())
    }

    fn handle_store_listener(
        &mut self,
        req_id: u64,
        frame: &Frame,
        mut fds: Vec<OwnedFd>,
        outbound: &mut VecDeque<OutFrame>,
    ) -> Result<(), ServeOutcome> {
        let body: ch::StoreListenerBody = match frame.parse_body() {
            Ok(b) => b,
            Err(e) => {
                push_frame(outbound, err_frame(req_id, ch::ERR_INVALID, e), vec![]);
                return Ok(());
            }
        };
        if fds.len() != 1 {
            return Err(ServeOutcome::Protocol(format!(
                "store_listener carried {} fds, want 1",
                fds.len()
            )));
        }
        let fd = fds.pop().expect("len checked");
        // Replace-per-kind: the old custodied fd (if any) closes on
        // drop — the C12 rebind flow (brain bound a new one because
        // its config changed; the stale listener must not linger).
        self.listeners.retain(|(m, _)| m.kind != body.listener.kind);
        self.listeners.push((body.listener, fd));
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
