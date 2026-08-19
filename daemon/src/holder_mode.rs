//! Holder mode — the brain half of the holder/brain split.
//! DESIGN_HOLDER_BRAIN_SPLIT phase 3: the `HolderClient` (the
//! daemon's speaker of the `cm-holder-proto` channel), the
//! holder-backed spawn path, and the adopt-at-boot sequence.
//!
//! ## Activation
//!
//! The daemon runs in holder mode exactly when it was spawned by
//! `cm-holder` — detected via [`ENV_CHANNEL_FD`] at startup
//! ([`init_from_env`], called from `run()` before anything spawns).
//! No config flag: the channel's existence IS the mode (the fd is
//! unforgeable — an inherited socketpair end, never a named socket).
//!
//! ## Threading
//!
//! One reader thread owns the inbound direction: it routes replies
//! to waiting callers by `req_id`, dispatches `exit_event`s to
//! per-session subscriptions, and answers `ping` with `pong`
//! IMMEDIATELY on this thread, touching no daemon lock (the S9
//! rule — a state-lock convoy must never look like a wedged brain).
//! Writers share a send mutex; requests pipeline (C7's envelope).
//!
//! ## Channel death is fatal (C5)
//!
//! EOF or a protocol violation on the channel means the holder is
//! gone or the law was broken: the brain EXITS. The holder (our
//! parent, supervising us) respawns a fresh generation that
//! re-adopts; limping on without a holder would strand every
//! session's exit pipeline. Paired with `PR_SET_PDEATHSIG(SIGKILL)`
//! at init (plus the getppid re-check for the parent-died-first
//! race), this keeps "holder crash ⇒ everything dies" true instead
//! of leaking an orphaned brain under a freshly launched holder.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use cm_holder_proto::channel::{
    self as ch, verbs, FeedStatus, Frame, FrameReader, ENV_CHANNEL_FD,
};

use crate::session::{
    AdoptedSessionMeta, DaemonExitStatus, DaemonSession, ExitAuthority, HolderAttribution,
    HolderExit, SpawnParams,
};
use crate::state::DaemonState;

/// Reply deadline for ordinary verbs. The holder answers in
/// microseconds; a miss means the holder itself is gone/wedged,
/// which the caller surfaces as an internal error (and channel
/// death, the usual cause, exits the brain first).
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

static GLOBAL: OnceLock<Arc<HolderClient>> = OnceLock::new();

/// The process-wide holder client, `Some` exactly in holder mode.
pub fn global() -> Option<&'static Arc<HolderClient>> {
    GLOBAL.get()
}

/// State handle for the exit paths (channel EOF / protocol
/// violation): the dying brain persists its durable registry +
/// tombstones best-effort before `exit(70)` — the holder's stop
/// sequence closes the channel BEFORE its SIGTERM can land, so the
/// EOF path is the one that actually runs at shutdown (S7's persist
/// belongs to whichever path fires).
static STATE_FOR_EXIT: OnceLock<std::sync::Weak<Mutex<DaemonState>>> = OnceLock::new();

/// Wire the exit-path persist. Called once from `run()` after the
/// state Arc exists.
pub fn set_state_for_exit(state: &Arc<Mutex<DaemonState>>) {
    let _ = STATE_FOR_EXIT.set(Arc::downgrade(state));
}

fn persist_before_exit(reason: &str) {
    if let Some(state) = STATE_FOR_EXIT.get().and_then(|w| w.upgrade()) {
        eprintln!("cm-daemon: {reason} — persisting durable state before exit");
        let st = state.lock().unwrap_or_else(|p| p.into_inner());
        st.persist_sessions_best_effort();
        if let Err(e) =
            st.save_daemon_tombstones_checked(&crate::state::default_daemon_tombstones_path())
        {
            eprintln!("cm-daemon: exit-path tombstone persist: {e}");
        }
    }
}

/// A typed holder-verb failure.
#[derive(Debug, Clone)]
pub struct HolderError {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for HolderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "holder: {} ({})", self.message, self.code)
    }
}
impl std::error::Error for HolderError {}

fn internal(msg: impl Into<String>) -> HolderError {
    HolderError {
        code: "channel".into(),
        message: msg.into(),
    }
}

/// Unix seconds (f64) — the tombstone/marker timebase.
fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// What `signal` did.
#[derive(Debug, PartialEq)]
pub enum SignalOutcome {
    Delivered,
    AlreadyExited,
}

pub struct HolderClient {
    /// The channel fd (shared open file description for both
    /// directions; the reader thread and senders use it
    /// concurrently — reads and writes are independent halves).
    fd: OwnedFd,
    send_lock: Mutex<()>,
    next_req: std::sync::atomic::AtomicU64,
    pending: Mutex<HashMap<u64, mpsc::Sender<(Frame, Vec<OwnedFd>)>>>,
    exit_subs: Mutex<HashMap<(String, u64), mpsc::Sender<HolderExit>>>,
    /// Events that arrived before their subscription (or for uids
    /// this brain never registered — the protocol-anomaly park, S4:
    /// never ack-and-drop).
    parked_exits: Mutex<Vec<ch::ExitEventBody>>,
    /// From the hello reply — surfaced on `daemon.health`.
    pub holder_build_id: String,
    pub epoch: u64,
}

impl HolderClient {
    /// Perform the hello handshake and start the reader thread.
    fn connect(fd: OwnedFd) -> Result<Arc<HolderClient>, HolderError> {
        // Blocking handshake inline (the reader thread takes over
        // after): send hello, await hello_reply.
        let hello = Frame::new(
            verbs::HELLO,
            Some(1),
            0,
            ch::HelloBody {
                proto_min: ch::PROTO_VERSION_MIN,
                proto_max: ch::PROTO_VERSION_MAX,
                brain_build_id: format!("cm-daemon/{}", env!("CARGO_PKG_VERSION")),
            },
        );
        ch::send_frame_blocking(fd.as_fd(), &hello, &[])
            .map_err(|e| internal(format!("hello send: {e}")))?;
        let mut reader = FrameReader::new();
        let reply = loop {
            if let Some((f, _fds)) = reader
                .next_frame()
                .map_err(|v| internal(v.0))?
            {
                break f;
            }
            match reader.feed(fd.as_fd()).map_err(|v| internal(v.0))? {
                FeedStatus::Eof => return Err(internal("channel EOF during hello")),
                _ => {}
            }
        };
        if reply.v == verbs::ERR {
            let e: ch::ErrBody = reply.parse_body().map_err(internal)?;
            return Err(HolderError {
                code: e.code,
                message: e.message,
            });
        }
        if reply.v != verbs::HELLO_REPLY {
            return Err(internal(format!("expected hello_reply, got '{}'", reply.v)));
        }
        let hr: ch::HelloReplyBody = reply.parse_body().map_err(internal)?;

        let client = Arc::new(HolderClient {
            fd,
            send_lock: Mutex::new(()),
            next_req: std::sync::atomic::AtomicU64::new(2),
            pending: Mutex::new(HashMap::new()),
            exit_subs: Mutex::new(HashMap::new()),
            parked_exits: Mutex::new(Vec::new()),
            holder_build_id: hr.holder_build_id,
            epoch: hr.epoch,
        });
        let for_reader = Arc::clone(&client);
        std::thread::Builder::new()
            .name("cm-holder-client-reader".into())
            .spawn(move || for_reader.reader_loop(reader))
            .map_err(|e| internal(format!("spawn client reader: {e}")))?;
        Ok(client)
    }

    /// The inbound dispatcher. Channel death is fatal for the whole
    /// brain (C5) — the holder respawns us.
    fn reader_loop(self: Arc<HolderClient>, mut reader: FrameReader) {
        loop {
            let fed = match reader.feed(self.fd.as_fd()) {
                Ok(s) => s,
                Err(v) => {
                    eprintln!("cm-daemon: holder channel protocol violation: {} — exiting (holder respawns us)", v.0);
                    std::process::exit(70);
                }
            };
            if fed == FeedStatus::Eof {
                persist_before_exit("holder channel EOF (the holder is gone or replacing us)");
                std::process::exit(70);
            }
            loop {
                match reader.next_frame() {
                    Ok(Some((frame, fds))) => self.dispatch(frame, fds),
                    Ok(None) => break,
                    Err(v) => {
                        eprintln!("cm-daemon: holder channel protocol violation: {} — exiting", v.0);
                        std::process::exit(70);
                    }
                }
            }
        }
    }

    fn dispatch(&self, frame: Frame, fds: Vec<OwnedFd>) {
        // Watchdog: answer on THIS thread, no daemon lock (S9).
        if frame.v == verbs::PING {
            if let Ok(p) = frame.parse_body::<ch::PingBody>() {
                let pong = Frame::new(verbs::PONG, None, 0, ch::PingBody { seq: p.seq });
                let _ = self.send_raw(&pong, &[]);
            }
            return;
        }
        if frame.v == verbs::EXIT_EVENT {
            if let Ok(ev) = frame.parse_body::<ch::ExitEventBody>() {
                self.route_exit(ev);
            }
            return;
        }
        if let Some(req_id) = frame.req_id {
            let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(tx) = pending.get(&req_id) {
                if tx.send((frame, fds)).is_err() {
                    pending.remove(&req_id);
                }
            } else {
                eprintln!(
                    "cm-daemon: holder reply for unknown req_id {req_id} (verb '{}') — dropped",
                    frame.v
                );
            }
            return;
        }
        eprintln!(
            "cm-daemon: unsolicited holder frame '{}' — ignored (additive tolerance)",
            frame.v
        );
    }

    fn route_exit(&self, ev: ch::ExitEventBody) {
        let key = (ev.uid.clone(), ev.incarnation);
        let mut subs = self.exit_subs.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(tx) = subs.get(&key) {
            let msg = holder_exit_from_event(&ev);
            if tx.send(msg).is_ok() {
                return;
            }
            subs.remove(&key);
        }
        drop(subs);
        // No live subscription: park, never ack-and-drop (S4). A
        // redelivery after `subscribe_exit` registers will route; a
        // genuinely unknown uid stays parked and logged.
        eprintln!(
            "cm-daemon: parking exit_event for '{}' inc {} (no subscription yet)",
            ev.uid, ev.incarnation
        );
        self.parked_exits
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(ev);
    }

    fn send_raw(&self, frame: &Frame, fds: &[RawFd]) -> Result<(), HolderError> {
        let _g = self.send_lock.lock().unwrap_or_else(|p| p.into_inner());
        ch::send_frame_blocking(self.fd.as_fd(), frame, fds)
            .map_err(|e| internal(format!("channel send: {e}")))
    }

    /// One request → one reply.
    fn request(
        &self,
        verb: &str,
        body: impl serde::Serialize,
    ) -> Result<(Frame, Vec<OwnedFd>), HolderError> {
        self.request_with_fds(verb, body, &[])
    }

    /// One request (carrying `fds` per the SCM_RIGHTS law) → one
    /// reply.
    fn request_with_fds(
        &self,
        verb: &str,
        body: impl serde::Serialize,
        fds: &[RawFd],
    ) -> Result<(Frame, Vec<OwnedFd>), HolderError> {
        let req_id = self
            .next_req
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(req_id, tx);
        let cleanup = |me: &Self| {
            me.pending
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&req_id);
        };
        let frame = Frame::new(verb, Some(req_id), fds.len() as u8, body);
        if let Err(e) = self.send_raw(&frame, fds) {
            cleanup(self);
            return Err(e);
        }
        match rx.recv_timeout(REPLY_TIMEOUT) {
            Ok(hit) => {
                cleanup(self);
                Ok(hit)
            }
            Err(_) => {
                cleanup(self);
                Err(internal(format!("no reply to '{verb}' within {REPLY_TIMEOUT:?}")))
            }
        }
    }

    fn expect_ok(&self, verb: &str, body: impl serde::Serialize) -> Result<(), HolderError> {
        let (f, _) = self.request(verb, body)?;
        if f.v == verbs::OK {
            return Ok(());
        }
        let e: ch::ErrBody = f.parse_body().map_err(internal)?;
        Err(HolderError {
            code: e.code,
            message: e.message,
        })
    }

    /// Spawn a session child in the holder. Returns the holder's
    /// reply plus the two fd dups `[master, pidfd]`.
    pub fn spawn(
        &self,
        spec: ch::SpawnBody,
    ) -> Result<(ch::SpawnOkBody, OwnedFd, OwnedFd), HolderError> {
        let (f, mut fds) = self.request(verbs::SPAWN, spec)?;
        if f.v != verbs::OK {
            let e: ch::ErrBody = f.parse_body().map_err(internal)?;
            return Err(HolderError {
                code: e.code,
                message: e.message,
            });
        }
        if fds.len() != 2 {
            return Err(internal(format!("spawn reply carried {} fds, want 2", fds.len())));
        }
        let ok: ch::SpawnOkBody = f.parse_body().map_err(internal)?;
        let pidfd = fds.pop().expect("len checked");
        let master = fds.pop().expect("len checked");
        Ok((ok, master, pidfd))
    }

    pub fn arm_reap(
        &self,
        uid: &str,
        incarnation: u64,
        cgroup_path: Option<String>,
    ) -> Result<(), HolderError> {
        self.expect_ok(
            verbs::ARM_REAP,
            ch::ArmReapBody {
                uid: uid.into(),
                incarnation,
                cgroup_path,
            },
        )
    }

    pub fn signal(
        &self,
        uid: &str,
        incarnation: u64,
        sig: i32,
        attribution: &str,
    ) -> Result<SignalOutcome, HolderError> {
        let (f, _) = self.request(
            verbs::SIGNAL,
            ch::SignalBody {
                uid: uid.into(),
                incarnation,
                sig,
                attribution: attribution.into(),
            },
        )?;
        if f.v == verbs::OK {
            return Ok(SignalOutcome::Delivered);
        }
        let e: ch::ErrBody = f.parse_body().map_err(internal)?;
        if e.code == ch::ERR_ALREADY_EXITED {
            return Ok(SignalOutcome::AlreadyExited);
        }
        Err(HolderError {
            code: e.code,
            message: e.message,
        })
    }

    pub fn abort_spawn(&self, uid: &str, incarnation: u64) -> Result<(), HolderError> {
        self.expect_ok(
            verbs::ABORT_SPAWN,
            ch::AbortSpawnBody {
                uid: uid.into(),
                incarnation,
            },
        )
    }

    pub fn ack_exit(&self, uid: &str, incarnation: u64) -> Result<(), HolderError> {
        self.expect_ok(
            verbs::ACK_EXIT,
            ch::AckExitBody {
                uid: uid.into(),
                incarnation,
            },
        )
    }

    pub fn forget(&self, uid: &str, incarnation: u64) -> Result<(), HolderError> {
        self.expect_ok(
            verbs::FORGET,
            ch::ForgetBody {
                uid: uid.into(),
                incarnation,
            },
        )
    }

    /// Push a watcher-policy checkpoint (R12/C11).
    pub fn update_checkpoint(
        &self,
        uid: &str,
        incarnation: u64,
        watcher_checkpoint: serde_json::Value,
    ) -> Result<(), HolderError> {
        self.expect_ok(
            verbs::UPDATE_CHECKPOINT,
            ch::UpdateCheckpointBody {
                uid: uid.into(),
                incarnation,
                watcher_checkpoint,
            },
        )
    }

    /// Custody a listener fd with the holder (O11). The holder dups
    /// via SCM_RIGHTS; the caller keeps its own fd.
    pub fn store_listener(
        &self,
        meta: ch::ListenerMeta,
        fd: RawFd,
    ) -> Result<(), HolderError> {
        let (f, _) = self.request_with_fds(
            verbs::STORE_LISTENER,
            ch::StoreListenerBody { listener: meta },
            &[fd],
        )?;
        if f.v == verbs::OK {
            return Ok(());
        }
        let e: ch::ErrBody = f.parse_body().map_err(internal)?;
        Err(HolderError {
            code: e.code,
            message: e.message,
        })
    }

    /// Arm a brain deploy: the holder stores the pinned fd as
    /// "next" and execs it when THIS brain exits (C8's arm-late
    /// rule — call only after quiesce + checked persistence).
    pub fn restart_brain(&self, pinned_fd: RawFd) -> Result<(), HolderError> {
        let (f, _) = self.request_with_fds(
            verbs::RESTART_BRAIN,
            serde_json::json!({}),
            &[pinned_fd],
        )?;
        if f.v == verbs::OK {
            return Ok(());
        }
        let e: ch::ErrBody = f.parse_body().map_err(internal)?;
        Err(HolderError {
            code: e.code,
            message: e.message,
        })
    }

    /// Arm the operator rollback: exec the holder's PREVIOUS pin
    /// when this brain exits (O9).
    pub fn rollback_brain(&self) -> Result<(), HolderError> {
        self.expect_ok(verbs::ROLLBACK_BRAIN, serde_json::json!({}))
    }

    /// Disarm any armed deploy (C8's abort path).
    pub fn cancel_pending(&self) -> Result<(), HolderError> {
        self.expect_ok(verbs::CANCEL_PENDING, serde_json::json!({}))
    }

    /// The holder's live status (surfaced on `daemon.health`).
    pub fn status(&self) -> Result<ch::StatusReplyBody, HolderError> {
        let (f, _) = self.request(verbs::STATUS, serde_json::json!({}))?;
        if f.v != verbs::OK {
            let e: ch::ErrBody = f.parse_body().map_err(internal)?;
            return Err(HolderError {
                code: e.code,
                message: e.message,
            });
        }
        f.parse_body().map_err(internal)
    }

    /// Register for a session's exit event, claiming any parked
    /// redelivery first.
    pub fn subscribe_exit(&self, uid: &str, incarnation: u64) -> mpsc::Receiver<HolderExit> {
        let (tx, rx) = mpsc::channel();
        // Parked event for this key? Deliver inline.
        {
            let mut parked = self.parked_exits.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(pos) = parked
                .iter()
                .position(|e| e.uid == uid && e.incarnation == incarnation)
            {
                let ev = parked.remove(pos);
                let _ = tx.send(holder_exit_from_event(&ev));
            }
        }
        self.exit_subs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert((uid.to_string(), incarnation), tx);
        rx
    }

    /// The adopt handshake: every holder-resident session record
    /// with fresh fd dups, the custodied listeners (O11), then the
    /// done marker.
    pub fn adopt(&self) -> Result<HolderBoot, HolderError> {
        let req_id = self
            .next_req
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(req_id, tx);
        let frame = Frame::new(verbs::ADOPT, Some(req_id), 0, serde_json::json!({}));
        self.send_raw(&frame, &[])?;
        let mut records = Vec::new();
        let mut listeners: Vec<(ch::ListenerMeta, OwnedFd)> = Vec::new();
        let done: ch::AdoptDoneBody = loop {
            let (f, mut fds) = rx
                .recv_timeout(REPLY_TIMEOUT)
                .map_err(|_| internal("adopt stream stalled"))?;
            match f.v.as_str() {
                verbs::ADOPT_RECORD => {
                    if fds.len() != 2 {
                        self.pending
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .remove(&req_id);
                        return Err(internal("adopt_record without its 2 fds"));
                    }
                    let body: ch::AdoptRecordBody = f.parse_body().map_err(internal)?;
                    let pidfd = fds.pop().expect("len checked");
                    let master = fds.pop().expect("len checked");
                    records.push((body, master, pidfd));
                }
                verbs::ADOPT_LISTENERS => {
                    let body: ch::AdoptListenersBody = f.parse_body().map_err(internal)?;
                    if fds.len() != body.listeners.len() {
                        self.pending
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .remove(&req_id);
                        return Err(internal(format!(
                            "adopt_listeners: {} metas, {} fds",
                            body.listeners.len(),
                            fds.len()
                        )));
                    }
                    listeners = body.listeners.into_iter().zip(fds).collect();
                }
                verbs::ADOPT_DONE => break f.parse_body().map_err(internal)?,
                verbs::ERR => {
                    // A per-record dup failure — logged holder-side;
                    // surface and continue collecting.
                    if let Ok(e) = f.parse_body::<ch::ErrBody>() {
                        eprintln!("cm-daemon: adopt stream error: {} ({})", e.message, e.code);
                    }
                }
                other => {
                    eprintln!("cm-daemon: unexpected adopt-stream verb '{other}' — ignored");
                }
            }
        };
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&req_id);
        Ok(HolderBoot {
            records,
            listeners,
            exit_events_pending: done.exit_events_pending,
        })
    }
}

/// Everything the adopt handshake returned, fetched EARLY in `run()`
/// (before the listener decision — the custodied control socket must
/// be resolvable before `bind_socket` would run) and consumed by
/// [`adopt_at_boot`] once state exists. Session fds park here in the
/// interim, untouched.
pub struct HolderBoot {
    pub records: Vec<(ch::AdoptRecordBody, OwnedFd, OwnedFd)>,
    pub listeners: Vec<(ch::ListenerMeta, OwnedFd)>,
    pub exit_events_pending: usize,
}

impl HolderBoot {
    /// Take the custodied listener of `kind`, if the holder held one.
    pub fn take_listener(&mut self, kind: &str) -> Option<(ch::ListenerMeta, OwnedFd)> {
        let pos = self.listeners.iter().position(|(m, _)| m.kind == kind)?;
        Some(self.listeners.remove(pos))
    }
}

/// Fetch the adopt handshake or die — a brain that cannot adopt has
/// nothing to serve; the holder respawns a fresh generation.
pub fn fetch_boot(client: &Arc<HolderClient>) -> HolderBoot {
    match client.adopt() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("cm-daemon: holder adopt failed: {e} — exiting (holder respawns us)");
            std::process::exit(70);
        }
    }
}

fn holder_exit_from_event(ev: &ch::ExitEventBody) -> HolderExit {
    HolderExit {
        status: DaemonExitStatus {
            code: ev.code,
            signal: ev.signal,
        },
        incarnation: ev.incarnation,
        exited_at: ev.exited_at,
        attribution: ev.last_signal_request.as_ref().map(|l| HolderAttribution {
            sig: l.sig,
            who: l.attribution.clone(),
            at: l.at,
        }),
        memory_events: ev.memory_events_snapshot.clone(),
    }
}

// ============================================================
// Startup
// ============================================================

/// Detect holder mode from [`ENV_CHANNEL_FD`], couple our life to
/// the holder's (C5), connect + hello, and set the global client.
/// Returns `true` in holder mode. Called once from `run()` BEFORE
/// anything can spawn.
pub fn init_from_env() -> bool {
    let Some(val) = std::env::var_os(ENV_CHANNEL_FD) else {
        return false;
    };
    // Consume unconditionally: children (session env is spec-built
    // anyway, but the brain's own transient subprocesses inherit our
    // environ) must never see a channel-fd pointer.
    std::env::remove_var(ENV_CHANNEL_FD);
    let Some(raw) = val.to_str().and_then(|s| s.parse::<RawFd>().ok()) else {
        eprintln!("cm-daemon: {ENV_CHANNEL_FD} is not an fd number — ignoring (fresh start)");
        return false;
    };
    if raw <= 2 {
        eprintln!("cm-daemon: {ENV_CHANNEL_FD}={raw} is stdio-range — ignoring (fresh start)");
        return false;
    }
    // Validate it's a socket before believing anything.
    // SAFETY: fstat on a numeric fd; failure handled.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(raw, &mut st) } != 0
        || (st.st_mode & libc::S_IFMT) != libc::S_IFSOCK
    {
        eprintln!("cm-daemon: {ENV_CHANNEL_FD}={raw} is not a socket — ignoring (fresh start)");
        return false;
    }
    // S10: the dup2'd fd arrives CLOEXEC-cleared by necessity; re-set
    // the flag now so no transient child can inherit the channel.
    // SAFETY: plain fcntl.
    unsafe {
        let flags = libc::fcntl(raw, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(raw, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
    // SAFETY: the holder handed us this fd; we own it from here.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };

    // The holder execs us via /proc/self/fd/<pin> (R7), which makes
    // the kernel-derived comm "4" — reclaim the real name so ps,
    // pgrep, and the e2e's find-by-comm see cm-daemon.
    // SAFETY: prctl(PR_SET_NAME) with a NUL-terminated ≤15-char name.
    unsafe {
        libc::prctl(
            libc::PR_SET_NAME,
            b"cm-daemon\0".as_ptr() as libc::c_ulong,
            0,
            0,
            0,
        );
    }
    // C5: die with the holder. PDEATHSIG first, then the
    // parent-died-before-prctl race check.
    // SAFETY: plain prctl on self.
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0);
    }
    // SAFETY: getppid is trivially safe.
    if unsafe { libc::getppid() } == 1 {
        eprintln!("cm-daemon: holder parent already gone (ppid=1) — exiting");
        std::process::exit(70);
    }

    match HolderClient::connect(fd) {
        Ok(client) => {
            eprintln!(
                "cm-daemon: HOLDER MODE — connected to holder {} (epoch {})",
                client.holder_build_id, client.epoch
            );
            let _ = GLOBAL.set(client);
            true
        }
        Err(e) => {
            eprintln!("cm-daemon: holder handshake failed: {e} — exiting (holder respawns us)");
            std::process::exit(70);
        }
    }
}

// ============================================================
// The holder-backed spawn path
// ============================================================

/// Compose the COMPLETE child environment (S1): the brain's own
/// post-`env_sanitize` environ as the base, the per-session
/// composition on top, and the same secret scrubs
/// `PendingSession::spawn` applies — as ABSENCE, not empty-string,
/// where the monolith used `env_remove`.
pub fn compose_full_env(params: &SpawnParams) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect();
    for (k, v) in &params.env {
        env.insert(k.clone(), v.clone());
    }
    // Secret scrubs (see PendingSession::spawn's rationale): the
    // monolith sets empty-string (its consumers treat empty as
    // unset); the spec map simply omits them.
    for key in ["CM_OPERATOR_TOKEN", "CM_DAEMON_TOKEN"] {
        if !params.env.contains_key(key) {
            env.remove(key);
        }
    }
    // Workflow vars are participant-only: strip inherited values
    // for non-participants (participants carry them in params.env).
    for key in ["CM_WORKFLOW_RUN_ID", "CM_ROLE"] {
        if !params.env.contains_key(key) {
            env.remove(key);
        }
    }
    env.remove(ENV_CHANNEL_FD);
    env
}

/// The holder-mode sibling of `PendingSession`: a spawned-but-not-
/// yet-inserted session. Drop = `abort_spawn` (the holder SIGKILLs
/// + reaps; no event) — the same recovery shape as
/// `PendingSession::Drop`, routed through the verb (S2).
pub struct HolderPending {
    build: Option<crate::session::AdoptedSessionBuild>,
    pub uid: String,
    pub incarnation: u64,
    /// Spawn-time kill-log baseline (0 when uncapped) — the
    /// checkpoint composer's session-side half (the watcher's push
    /// closure fills it into each pushed checkpoint).
    pub kills_baseline: u64,
    pid: libc::pid_t,
    client: Arc<HolderClient>,
}

/// Runs between `on_exit` (which recorded the tombstone in memory)
/// and the holder ack: `true` = the tombstone reached DISK (the 4c
/// checked write) and the ack may proceed; `false` = the write
/// failed — skip the ack so the holder redelivers to a brain
/// generation that can persist (the C4 durable-commit order).
pub type PreAckFn = Box<dyn FnOnce() -> bool + Send>;

impl HolderPending {
    pub fn pid(&self) -> libc::pid_t {
        self.pid
    }

    /// The watcher's checkpoint-push closure for this session
    /// (R12/C11): fills in the session-side `kills_baseline` and
    /// routes `update_checkpoint`, best-effort.
    pub fn checkpoint_push(&self) -> crate::session_watch::CheckpointPushFn {
        let client = Arc::clone(&self.client);
        let (uid, inc, baseline) = (self.uid.clone(), self.incarnation, self.kills_baseline);
        Arc::new(move |mut cp: crate::session_watch::WatcherCheckpoint| {
            cp.kills_baseline = baseline;
            match serde_json::to_value(&cp) {
                Ok(v) => {
                    if let Err(e) = client.update_checkpoint(&uid, inc, v) {
                        eprintln!("cm-daemon: checkpoint push {uid}: {e}");
                    }
                }
                Err(e) => eprintln!("cm-daemon: checkpoint serialize {uid}: {e}"),
            }
        })
    }

    /// Arm under the state lock (same discipline as `arm_reaper`):
    /// wires the exit observer to the client's subscription, the
    /// settle (pre-ack persist gate → ack + forget) hook, and the
    /// verb-routed kill.
    pub fn arm(
        mut self,
        on_exit: Option<crate::session::OnExitCallback>,
        pre_ack: Option<PreAckFn>,
    ) -> anyhow::Result<DaemonSession> {
        let build = self.build.take().expect("arm called once");
        let events = self.client.subscribe_exit(&self.uid, self.incarnation);
        let settle_client = Arc::clone(&self.client);
        let (uid_s, inc_s) = (self.uid.clone(), self.incarnation);
        let settle: Box<dyn FnOnce() + Send> = Box::new(move || {
            // C4: the ack is a statement that the tombstone is
            // DURABLE. A failed checked write skips the ack — the
            // event stays holder-pending and redelivers.
            if let Some(gate) = pre_ack {
                if !gate() {
                    eprintln!(
                        "cm-daemon: tombstone persist for {uid_s} failed — \
                         NOT acking (the holder redelivers the exit event)"
                    );
                    return;
                }
            }
            if let Err(e) = settle_client.ack_exit(&uid_s, inc_s) {
                eprintln!("cm-daemon: ack_exit {uid_s}: {e} (holder will redeliver)");
                return;
            }
            if let Err(e) = settle_client.forget(&uid_s, inc_s) {
                eprintln!("cm-daemon: forget {uid_s}: {e}");
            }
        });
        let kill_client = Arc::clone(&self.client);
        let (uid_k, inc_k) = (self.uid.clone(), self.incarnation);
        let kill: crate::session::HolderKillFn = Box::new(move |sig, who| {
            match kill_client.signal(&uid_k, inc_k, sig, who) {
                Ok(_) => Ok(()),
                Err(e) => Err(io::Error::other(e.to_string())),
            }
        });
        let session = build.arm(
            on_exit,
            ExitAuthority::Holder {
                events,
                settle,
                kill,
            },
        )?;
        // Armed: defuse Drop's abort.
        std::mem::forget(self);
        Ok(session)
    }
}

impl Drop for HolderPending {
    fn drop(&mut self) {
        // Pre-insert failure: tear the child down via the verb (the
        // holder kills + reaps unconditionally, no event). The build
        // (if still present) drops non-killing — its fds are dups.
        if let Err(e) = self.client.abort_spawn(&self.uid, self.incarnation) {
            eprintln!(
                "cm-daemon: abort_spawn {} inc {}: {e}",
                self.uid, self.incarnation
            );
        }
    }
}

/// fork/exec via the holder: compose spec → `spawn` verb → wrap the
/// dups in the adoption primitives. Mirrors `PendingSession::spawn`'s
/// ordering (pretrust + kills-baseline BEFORE the child exists).
pub fn holder_spawn(
    client: &Arc<HolderClient>,
    params: SpawnParams,
) -> anyhow::Result<HolderPending> {
    // Same choke-point policy calls as the monolith spawn:
    crate::claude_trust::maybe_pretrust_for_spawn(
        &params.shell,
        &params.args,
        params.working_dir.as_deref(),
    );
    let kills_baseline = params.kills_dir.as_ref().map(|dir| {
        crate::reaper::capture_baseline_for_spawn(dir, &params.uid).unwrap_or(0)
    });

    let mut argv = Vec::with_capacity(1 + params.args.len());
    argv.push(params.shell.clone());
    argv.extend(params.args.iter().cloned());
    let spec = ch::SpawnBody {
        uid: params.uid.clone(),
        generation_meta: 0,
        argv,
        env: compose_full_env(&params),
        cwd: params
            .working_dir
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        cols: params.cols,
        rows: params.rows,
        cgroup_prefix: params
            .cgroup_prefix
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
    };
    let (ok, master, pidfd) = client
        .spawn(spec)
        .map_err(|e| anyhow::anyhow!("holder spawn: {e}"))?;

    let candidate = crate::adopt::SessionCandidate::from_raw_parts(
        params.uid.clone(),
        ok.pid,
        pidfd,
        master,
    );
    let parts = candidate.promote();
    let meta = AdoptedSessionMeta {
        title: params.title.clone(),
        session_type: params.session_type.clone(),
        workspace_id: params.workspace_id.clone(),
        managed_by_uid: params.managed_by_uid.clone(),
        task_id: params.task_id.clone(),
        transcript_path: params.transcript_path.clone(),
        memory_cap_soft_bytes: params.memory_cap_soft_bytes,
        memory_cap_hard_bytes: params.memory_cap_hard_bytes,
        cgroup_prefix: params.cgroup_prefix.clone(),
        workflow_run_id: params.workflow_run_id.clone(),
        workflow_role: params.workflow_role.clone(),
        continuous_task_id: params.continuous_task_id.clone(),
        global_perms: params.global_perms,
        generation: 0,
        last_activity_at: None,
        last_input_at: None,
        last_operator_input_at: None,
        last_turn_end_at: None,
        done_report: None,
        kills_dir: params.kills_dir.clone(),
        kills_baseline,
    };
    match DaemonSession::build_adopted(parts, meta) {
        Ok(build) => Ok(HolderPending {
            build: Some(build),
            uid: params.uid,
            incarnation: ok.incarnation,
            kills_baseline: kills_baseline.unwrap_or(0),
            pid: ok.pid,
            client: Arc::clone(client),
        }),
        Err(e) => {
            // The build failed after the child exists: abort now
            // (no HolderPending exists to do it on drop).
            let _ = client.abort_spawn(&params.uid, ok.incarnation);
            Err(e)
        }
    }
}

// ============================================================
// Brain deploys (§ Brain deploys — daemon.restart / rollback_brain
// in split mode, phase 6)
// ============================================================

/// What the detached deploy thread arms after quiescing.
enum DeployAction {
    NewPin(OwnedFd),
    UsePrevious,
}

/// The split-mode `daemon.restart`: pin + preflight the new brain
/// binary, reply `accepted` (the caller's contract, O8: refused vs
/// in-progress; completion is verify-based), then — on a detached
/// thread, per C8's arm-late rule — quiesce, checked-persist, ARM,
/// exit. Returns the RPC result value.
pub fn restart_brain_flow(
    state_arc: &Arc<Mutex<DaemonState>>,
    binary_path: Option<&str>,
) -> Result<serde_json::Value, (crate::control::protocol::ErrorCode, String)> {
    use crate::control::protocol::ErrorCode;
    let client = global().ok_or((
        ErrorCode::Conflict,
        "not in holder/brain split mode".to_string(),
    ))?;
    let target = crate::reexec::resolve_restart_target(binary_path)
        .map_err(|msg| (ErrorCode::InvalidParams, format!("daemon.restart: {msg}")))?;
    let pin: OwnedFd = std::fs::File::open(&target.path)
        .map_err(|e| {
            (
                ErrorCode::InvalidParams,
                format!("daemon.restart: pin {}: {e}", target.path.display()),
            )
        })?
        .into();
    brain_deploy_preflight(&pin).map_err(|msg| (ErrorCode::Conflict, msg))?;
    eprintln!(
        "cm-daemon: daemon.restart (split) — target {} preflighted; quiescing \
         then arming restart_brain (this brain exits; the holder execs the pin)",
        target.path.display()
    );
    spawn_deploy_thread(state_arc, client, DeployAction::NewPin(pin));
    Ok(serde_json::json!({
        "accepted": true,
        "mode": "split",
        "message": "brain deploy accepted: quiescing, persisting, then the \
                    holder execs the pinned binary. Verify via daemon.health \
                    (holder_epoch increments; soak per the O8 recipe).",
    }))
}

/// The operator rollback (O9): arm `rollback_brain` after quiesce —
/// the answer to a bad-but-not-crashing brain.
pub fn rollback_brain_flow(
    state_arc: &Arc<Mutex<DaemonState>>,
) -> Result<serde_json::Value, (crate::control::protocol::ErrorCode, String)> {
    use crate::control::protocol::ErrorCode;
    let client = global().ok_or((
        ErrorCode::Conflict,
        "not in holder/brain split mode".to_string(),
    ))?;
    // Pre-check so the caller's `accepted` can't precede an
    // impossible rollback.
    match client.status() {
        Ok(st) if st.previous_pin => {}
        Ok(_) => {
            return Err((
                ErrorCode::Conflict,
                "the holder has no previous brain pin to roll back to".into(),
            ))
        }
        Err(e) => return Err((ErrorCode::Internal, format!("holder status: {e}"))),
    }
    eprintln!(
        "cm-daemon: daemon.rollback_brain — quiescing then arming rollback \
         (this brain exits; the holder execs the previous pin)"
    );
    spawn_deploy_thread(state_arc, client, DeployAction::UsePrevious);
    Ok(serde_json::json!({
        "accepted": true,
        "mode": "split",
        "message": "rollback accepted: the holder will exec the previous \
                    pinned brain. Verify via daemon.health.",
    }))
}

/// The candidate brain proves itself: `<pin> --daemon-preflight`
/// (config + durable-state parse), via /proc/self/fd so the checked
/// inode is the armed inode.
fn brain_deploy_preflight(pin: &OwnedFd) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    let raw = pin.as_raw_fd();
    let mut cmd = std::process::Command::new(format!("/proc/self/fd/{raw}"));
    cmd.arg("--daemon-preflight")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // SAFETY (pre_exec): clear CLOEXEC on the pin in the CHILD only,
    // so the exec can resolve /proc/self/fd/<raw> (the reexec
    // preflight's R9-safe shape).
    unsafe {
        cmd.pre_exec(move || {
            let flags = libc::fcntl(raw, libc::F_GETFD);
            if flags < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::fcntl(raw, libc::F_SETFD, flags & !libc::FD_CLOEXEC) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let out = cmd
        .output()
        .map_err(|e| format!("brain preflight spawn failed: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "brain deploy REFUSED — the candidate binary failed \
             --daemon-preflight ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// C8's ordered tail, detached so the RPC reply reaches the caller
/// first: quiesce (mutation barrier + writer/reader freezes) →
/// checked persistence → ARM → exit(0). Any failure disarms
/// (`cancel_pending`), un-drains, and logs loudly — the caller was
/// told "accepted", so the failure must be findable.
fn spawn_deploy_thread(
    state_arc: &Arc<Mutex<DaemonState>>,
    client: &'static Arc<HolderClient>,
    action: DeployAction,
) {
    let state = Arc::clone(state_arc);
    let _ = std::thread::Builder::new()
        .name("cm-brain-deploy".into())
        .spawn(move || {
            // Let the RPC reply flush before the drain flips.
            std::thread::sleep(Duration::from_millis(300));
            let guard = match crate::restart_coordinator::begin(&state) {
                Ok(g) => g,
                Err(busy) => {
                    eprintln!("cm-daemon: brain deploy ABORTED: {busy}");
                    return;
                }
            };
            if let Err(t) = guard.wait_quiesced(Duration::from_secs(10)) {
                eprintln!("cm-daemon: brain deploy ABORTED: {t}");
                guard.abort();
                return;
            }
            // Byte + prompt quiescence (the doc's planned-restart
            // zero-loss guarantee): reader gate (no bytes die on
            // reader stacks) and writer gate (no half-typed prompts).
            let writer_pause = crate::writer_gate::request_pause();
            let _writer_freeze = match crate::writer_gate::freeze(
                &writer_pause,
                Duration::from_secs(10),
            ) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("cm-daemon: brain deploy ABORTED (writer gate): {e}");
                    guard.abort();
                    return;
                }
            };
            let _reader_freeze = crate::reader_gate::freeze();
            // Checked persistence — the state the next brain adopts
            // against.
            {
                let st = state.lock().unwrap_or_else(|p| p.into_inner());
                if let Err(e) = st.save_daemon_sessions_checked(
                    &crate::state::default_daemon_sessions_path(),
                ) {
                    eprintln!("cm-daemon: brain deploy ABORTED (registry persist): {e}");
                    drop(st);
                    guard.abort();
                    return;
                }
                if let Err(e) = st.save_daemon_tombstones_checked(
                    &crate::state::default_daemon_tombstones_path(),
                ) {
                    eprintln!("cm-daemon: brain deploy ABORTED (tombstone persist): {e}");
                    drop(st);
                    guard.abort();
                    return;
                }
            }
            // ARM — nothing fallible between here and exit (C8).
            let armed = match action {
                DeployAction::NewPin(pin) => client.restart_brain(pin.as_raw_fd()),
                DeployAction::UsePrevious => client.rollback_brain(),
            };
            if let Err(e) = armed {
                eprintln!("cm-daemon: brain deploy ABORTED (arm failed): {e}");
                let _ = client.cancel_pending();
                guard.abort();
                return;
            }
            eprintln!("cm-daemon: brain deploy armed — exiting for the holder to exec");
            std::process::exit(0);
        });
}

// ============================================================
// Adopt-at-boot (O16 order: before the MCP preflight)
// ============================================================

/// Re-adopt every holder-resident session into the registry. Runs
/// EARLY in startup (readers must precede the seconds-long python
/// preflight — kernel PTY buffers are ~64 KiB). Meta comes from the
/// persisted `daemon-sessions.json` via `rehydrate_derived_state`
/// (the shared non-spawning restore half); records the snapshot
/// doesn't know are adopted with minimal meta and flagged in the
/// title (`orphan_adopted` — sessions are user-owned, never
/// auto-killed).
pub fn adopt_at_boot(
    state_arc: &Arc<Mutex<DaemonState>>,
    client: &Arc<HolderClient>,
    boot: HolderBoot,
) {
    let HolderBoot {
        records,
        listeners,
        exit_events_pending,
    } = boot;
    // Listeners were claimed by run()'s bind decision; anything left
    // is a kind this build doesn't know — closed (dropped), logged,
    // tolerated (additive discipline).
    for (meta, _fd) in &listeners {
        eprintln!(
            "cm-daemon: holder custodies a listener of unknown kind '{}' ({}) — ignored",
            meta.kind, meta.meta
        );
    }
    drop(listeners);
    eprintln!(
        "cm-daemon: holder adopt: {} record(s), {} pending exit event(s)",
        records.len(),
        exit_events_pending
    );

    // Index the persisted registry (also rebuilds workspaces /
    // bindings / task edges — the 4g non-spawning half).
    let mut persisted: HashMap<String, (String, crate::manifest::ManifestEntry)> =
        HashMap::new();
    if let Some(manifest) = crate::control::methods::rehydrate_derived_state(state_arc) {
        for (ws_id, ws) in &manifest.workspaces {
            for e in &ws.sessions {
                persisted.insert(e.uid.clone(), (ws_id.clone(), e.clone()));
            }
        }
    }
    let coordinator = {
        let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        Arc::downgrade(&st.restart_coordinator)
    };
    let mut adopted_uids: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (rec, master, pidfd) in records {
        // V2 reconciliation rule 3: reaped, nothing pending — the
        // exit was acked and tombstoned by a previous generation;
        // `forget` is the correct disposal, never adoption.
        if rec.reaped && !rec.exit_event_pending {
            eprintln!(
                "cm-daemon: holder record '{}' reaped+acked — forgetting",
                rec.uid
            );
            let _ = client.forget(&rec.uid, rec.incarnation);
            continue;
        }
        let entry = persisted.get(&rec.uid).cloned();
        // R12: the holder-held policy checkpoint, parsed with the
        // same loud version-gated parser the re-exec path uses; an
        // unusable blob degrades to the fresh-policy reset inside
        // `readopt_watcher`.
        let checkpoint = rec
            .watcher_checkpoint
            .as_ref()
            .and_then(|v| crate::session_watch::parse_watcher_checkpoint(&rec.uid, v));
        let mut meta = match &entry {
            Some((ws_id, e)) => adopted_meta_from_manifest_entry(ws_id, e),
            None => {
                eprintln!(
                    "cm-daemon: holder record '{}' unknown to the persisted registry — orphan_adopted",
                    rec.uid
                );
                orphan_meta(&rec.uid)
            }
        };
        // The checkpoint's spawn-time kill-log baseline restores the
        // cap-kill attribution window (R12); no checkpoint → fresh
        // adopt-time capture inside build_adopted.
        meta.kills_baseline = checkpoint.as_ref().map(|cp| cp.kills_baseline);
        let candidate = crate::adopt::SessionCandidate::from_raw_parts(
            rec.uid.clone(),
            rec.child_pid,
            pidfd,
            master,
        );
        let parts = candidate.promote();
        let session_type = meta.session_type.clone();
        let build = match DaemonSession::build_adopted(parts, meta) {
            Ok(b) => b,
            Err(e) => {
                eprintln!(
                    "cm-daemon: holder adopt '{}' failed to build: {e} — skipping (never signaled; the holder keeps it)",
                    rec.uid
                );
                continue;
            }
        };
        let pending = HolderPending {
            build: Some(build),
            uid: rec.uid.clone(),
            incarnation: rec.incarnation,
            kills_baseline: checkpoint.as_ref().map(|cp| cp.kills_baseline).unwrap_or(0),
            pid: rec.child_pid,
            client: Arc::clone(client),
        };
        // Re-arm the memory-cap watcher for capped records, seeded
        // from the checkpoint (R12) — before the lock, mirroring the
        // spawn path's ordering. Its policy publishes push back to
        // the holder (C11).
        let watcher = readopt_watcher(
            entry.as_ref().map(|(_, e)| e),
            &rec,
            checkpoint.as_ref(),
            coordinator.clone(),
            pending.checkpoint_push(),
        );
        let state_for_cleanup = Arc::clone(state_arc);
        let uid_for_cleanup = rec.uid.clone();
        let on_exit: crate::session::OnExitCallback = Box::new(move |_status| {
            let mut s = state_for_cleanup
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            crate::control::methods::handle_session_exit(&mut s, &uid_for_cleanup);
        });
        // C4: the holder ack asserts the tombstone reached disk.
        let state_for_ack = Arc::clone(state_arc);
        let pre_ack: PreAckFn = Box::new(move || {
            let st = state_for_ack.lock().unwrap_or_else(|p| p.into_inner());
            match st.save_daemon_tombstones_checked(
                &crate::state::default_daemon_tombstones_path(),
            ) {
                Ok(()) => true,
                Err(e) => {
                    eprintln!("cm-daemon: checked tombstone persist failed: {e}");
                    false
                }
            }
        });
        // Same lock discipline as the spawn path: arm + insert under
        // one hold, arm_reap after (insert-first is the ordering
        // invariant — events deliver only post-arm).
        {
            let mut st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
            if st.sessions.contains_key(&rec.uid) {
                eprintln!(
                    "cm-daemon: holder adopt '{}': uid already live — skipping duplicate",
                    rec.uid
                );
                // pending drops OUTSIDE this branch's lock scope —
                // but its Drop calls abort_spawn, which would KILL a
                // live child. Defuse instead: never abort a record
                // we merely declined to duplicate.
                std::mem::forget(pending);
                continue;
            }
            match pending.arm(Some(on_exit), Some(pre_ack)) {
                Ok(mut sess) => {
                    if let Some(w) = watcher {
                        sess.watcher_handle = Some(w.handle);
                        sess.watcher_state = Some(w.state);
                    }
                    st.sessions.insert(rec.uid.clone(), sess);
                }
                Err(e) => {
                    eprintln!(
                        "cm-daemon: holder adopt '{}' failed to arm: {e} — skipping (never signaled)",
                        rec.uid
                    );
                    continue;
                }
            }
        }
        // Prefer the checkpoint's DISCOVERED scope path for the
        // holder's memory.events carve-out (V5); the prefix-shaped
        // rec.cgroup_path is the fallback.
        let carveout_path = checkpoint
            .as_ref()
            .map(|cp| cp.cgroup_path.clone())
            .or_else(|| rec.cgroup_path.clone());
        if let Err(e) = client.arm_reap(&rec.uid, rec.incarnation, carveout_path) {
            eprintln!("cm-daemon: arm_reap {}: {e}", rec.uid);
        }
        // 4f parity with the spawn funnel: adopted codex sessions get
        // their rollout watch re-armed (rollout ids rotate on
        // /compact and codex runs no cm hook).
        if session_type == "codex" {
            crate::transcript_detect::spawn_codex_rollout_watch(
                Arc::clone(state_arc),
                rec.uid.clone(),
            );
        }
        adopted_uids.insert(rec.uid.clone());
        eprintln!(
            "cm-daemon: holder adopt '{}' (pid {}, incarnation {})",
            rec.uid, rec.child_pid, rec.incarnation
        );
    }

    // § Exit provenance, the "no status" residual: a SURVIVING holder
    // (epoch > 1) that does not hold a session the persisted registry
    // believes is live means the session is gone with no
    // reconstructible status — tombstone it `status_lost`, honestly,
    // instead of letting legacy restore respawn a `--resume` twin of
    // a conversation whose ending nobody saw. A FRESH holder
    // (epoch == 1: first boot after a machine restart) holds nothing
    // by construction — those entries belong to legacy restore.
    if client.epoch > 1 {
        let mut lost = 0usize;
        let mut st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        for (uid, (ws_id, e)) in &persisted {
            if adopted_uids.contains(uid)
                || st.sessions.contains_key(uid)
                || e.last_exit.is_some()
                || e.workflow_run_id.is_some()
            {
                continue;
            }
            let exited_at = unix_now();
            st.record_exited(crate::state::ExitedTombstone {
                session_uid: uid.clone(),
                transcript_path: e.transcript_path.clone(),
                generation: e.generation,
                session_type: e.session_type.clone(),
                workspace_id: ws_id.clone(),
                task_id: e.task_id.clone(),
                managed_by_uid: e.managed_by_uid.clone(),
                label: e.label.clone(),
                workflow_run_id: None,
                workflow_role: None,
                worktree_path: None,
                global_perms: e.global_perms,
                exited_at,
                killed: false,
                killed_by: None,
                reported_done_at: e.reported_done_at,
                report_reason: e.report_reason.clone(),
                incarnation: None,
                status_lost: true,
            });
            eprintln!(
                "cm-daemon: session '{}' is in the persisted registry but not \
                 held by the surviving holder — tombstoned status_lost (no \
                 reconstructible exit status)",
                uid
            );
            lost += 1;
        }
        if lost > 0 {
            // Durable: the status_lost verdict must survive the next
            // brain restart too.
            if let Err(e) = st.save_daemon_tombstones_checked(
                &crate::state::default_daemon_tombstones_path(),
            ) {
                eprintln!("cm-daemon: status_lost tombstone persist: {e}");
            }
            // Rewriting the registry from the LIVE sessions drops the
            // lost entries, so the legacy restore pass below cannot
            // respawn them. Deliberately NOT done at epoch 1, where
            // the same rewrite would erase legitimately-restorable
            // entries before restore reads them.
            st.persist_sessions_best_effort();
        }
    }
    let count = {
        let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        st.sessions.len()
    };
    eprintln!("cm-daemon: holder adopt complete — {count} session(s) live");
}

fn adopted_meta_from_manifest_entry(
    ws_id: &str,
    e: &crate::manifest::ManifestEntry,
) -> AdoptedSessionMeta {
    AdoptedSessionMeta {
        title: e.label.clone(),
        session_type: e.session_type.clone(),
        workspace_id: ws_id.to_string(),
        managed_by_uid: e.managed_by_uid.clone(),
        task_id: e.task_id.clone(),
        // Phase 4: the persisted entry now carries the PATH (the
        // phase-3 gap) — transcript reads work immediately
        // post-adopt. Older files default `None`; the TUI's next
        // `session.set_transcript_path` heals those.
        transcript_path: e.transcript_path.clone(),
        memory_cap_soft_bytes: e.memory_cap_soft_bytes,
        memory_cap_hard_bytes: e.memory_cap_hard_bytes,
        cgroup_prefix: e.cgroup_prefix.clone(),
        workflow_run_id: e.workflow_run_id.clone(),
        workflow_role: e.workflow_role.clone(),
        continuous_task_id: e.continuous_task_id.clone(),
        global_perms: e.global_perms,
        generation: e.generation,
        // Activity cells reset to adoption time — the state-inventory
        // delta's accepted reset (afterglow UI restarts; auth and
        // status logic re-derive from fresh input).
        last_activity_at: None,
        last_input_at: None,
        last_operator_input_at: None,
        last_turn_end_at: None,
        // R11: the report_done marker survives the brain restart —
        // an until="final" watcher must keep seeing status="reported".
        done_report: e.reported_done_at.map(|at| crate::session::ReportedDone {
            at_unix: at,
            at_instant: instant_at_unix(at),
            reason: e.report_reason.clone(),
        }),
        kills_dir: e
            .memory_cap_soft_bytes
            .map(|_| crate::path::default_kills_dir())
            .flatten(),
        kills_baseline: None,
    }
}

/// Reconstruct an `Instant` for a past unix timestamp (the re-exec
/// age-reconstruction idiom): `now - age`, clamped at now for a
/// future-dated stamp (clock skew).
fn instant_at_unix(at_unix: f64) -> std::time::Instant {
    let now = unix_now();
    let age = (now - at_unix).max(0.0);
    std::time::Instant::now()
        .checked_sub(Duration::from_secs_f64(age))
        .unwrap_or_else(std::time::Instant::now)
}

/// Re-arm the memory-cap watcher for an adopted capped record —
/// the holder-mode sibling of `reexec::respawn_adopted_watcher`:
/// caps from the persisted entry, policy from the holder-held
/// checkpoint (R12), fresh-policy reset (loud) without one.
fn readopt_watcher(
    entry: Option<&crate::manifest::ManifestEntry>,
    rec: &ch::AdoptRecordBody,
    checkpoint: Option<&crate::session_watch::WatcherCheckpoint>,
    coordinator: std::sync::Weak<crate::restart_coordinator::RestartCoordinator>,
    push: crate::session_watch::CheckpointPushFn,
) -> Option<crate::session_watch::SpawnedWatcher> {
    let entry = entry?;
    let soft_cap_bytes = entry.memory_cap_soft_bytes?;
    let hard_cap_bytes = crate::control::methods::resolve_watcher_hard_cap_bytes(
        entry.memory_cap_hard_bytes,
    );
    let Some(kills_dir) = crate::path::default_kills_dir() else {
        eprintln!(
            "cm-daemon: adopted capped session '{}': no kills dir resolves — \
             memory cap UNENFORCED (no watcher); adoption proceeds",
            rec.uid,
        );
        return None;
    };
    let (cgroup_path, initial_high, restored_protected) = match checkpoint {
        Some(cp) => (
            std::path::PathBuf::from(&cp.cgroup_path),
            cp.last_high,
            cp.protected
                .as_ref()
                .map(|v| v.iter().copied().collect::<std::collections::HashSet<(u32, u64)>>()),
        ),
        None => {
            eprintln!(
                "cm-daemon: adopted capped session '{}': no usable watcher \
                 checkpoint — POLICY RESET: fresh watcher (protected set \
                 recomputed, breach watermark re-anchored)",
                rec.uid,
            );
            let discovered = match crate::path::discover_session_cgroup_path(
                rec.child_pid as u32,
                Duration::from_millis(500),
            ) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!(
                        "cm-daemon: adopted capped session '{}': cgroup \
                         re-discovery failed ({}) — memory cap UNENFORCED; \
                         adoption proceeds",
                        rec.uid, e,
                    );
                    return None;
                }
            };
            let high = crate::session_watch::read_memory_events_high(&discovered);
            (discovered, high, None)
        }
    };
    match crate::session_watch::spawn_watcher(
        rec.uid.clone(),
        cgroup_path.clone(),
        soft_cap_bytes,
        hard_cap_bytes,
        kills_dir,
        initial_high,
        crate::session_watch::default_watcher_spawn_fn(),
        Some(coordinator),
        restored_protected,
        Some(push),
    ) {
        Ok(w) => {
            eprintln!(
                "cm-daemon: adopted capped session '{}': memory-cap watcher \
                 re-armed on {} ({})",
                rec.uid,
                cgroup_path.display(),
                if checkpoint.is_some() {
                    "checkpoint policy"
                } else {
                    "fresh policy"
                },
            );
            Some(w)
        }
        Err(e) => {
            eprintln!(
                "cm-daemon: adopted capped session '{}': watcher re-arm \
                 failed ({}) — memory cap UNENFORCED; adoption proceeds",
                rec.uid, e,
            );
            None
        }
    }
}

fn orphan_meta(uid: &str) -> AdoptedSessionMeta {
    AdoptedSessionMeta {
        title: format!("{uid} (orphan_adopted)"),
        session_type: "bash".to_string(),
        workspace_id: String::new(),
        managed_by_uid: None,
        task_id: None,
        transcript_path: None,
        memory_cap_soft_bytes: None,
        memory_cap_hard_bytes: None,
        cgroup_prefix: None,
        workflow_run_id: None,
        workflow_role: None,
        continuous_task_id: None,
        global_perms: false,
        generation: 0,
        last_activity_at: None,
        last_input_at: None,
        last_operator_input_at: None,
        last_turn_end_at: None,
        done_report: None,
        kills_dir: None,
        kills_baseline: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The S2/O4 source-scan guard: brain-side code outside
    /// `session.rs` (the `kill()` primitive + the pre-arm spawn
    /// cleanups), `session_watch.rs` (the watcher's own re-verified
    /// victim kills, outside the holder verb set by design), and
    /// `reexec.rs` (the monolith terminal fallback) must NEVER
    /// signal a session pidfd directly — holder-owned children are
    /// killed by VERB only, so the attribution echo can't be
    /// silently bypassed by a future least-change edit.
    #[test]
    fn no_direct_session_pidfd_signaling_outside_the_allowlist() {
        // Needles assembled at runtime so this test's own source
        // can't self-match.
        let needles = [
            ["SYS_pidfd_", "send_signal"].concat(),
            ["send_sigkill_", "via_pidfd("].concat(),
        ];
        for (name, src) in [
            ("holder_mode.rs", include_str!("holder_mode.rs")),
            ("control/methods.rs", include_str!("control/methods.rs")),
            ("control/dispatch.rs", include_str!("control/dispatch.rs")),
            ("control/stream.rs", include_str!("control/stream.rs")),
        ] {
            for needle in &needles {
                let hits: Vec<&str> = src
                    .lines()
                    .filter(|l| l.contains(needle.as_str()) && !l.trim_start().starts_with("//"))
                    .collect();
                assert!(
                    hits.is_empty(),
                    "{name} signals a pidfd directly ({needle}): {hits:?} — \
                     route the holder `signal` verb (S2/O4)"
                );
            }
        }
    }

    fn entry_with(
        reported_done_at: Option<f64>,
        report_reason: Option<String>,
        transcript_path: Option<String>,
    ) -> crate::manifest::ManifestEntry {
        crate::manifest::ManifestEntry {
            uid: "ts-abc-1".into(),
            managed_by_uid: Some("ts-parent-1".into()),
            generation: 3,
            label: "worker".into(),
            session_type: "claude-code".into(),
            transcript_id: Some("abc".into()),
            hidden: false,
            idle_timeout_secs: 0,
            burst_threshold: 0,
            workflow_run_id: None,
            workflow_role: None,
            continuous_task_id: None,
            task_id: Some("task-1".into()),
            notify_on_idle: false,
            color: None,
            memory_cap_soft_bytes: Some(1024),
            memory_cap_hard_bytes: Some(2048),
            cgroup_prefix: None,
            global_perms: true,
            seeded_from_snapshot: None,
            last_exit: None,
            host_id: crate::host_id::HostId::local(),
            transcript_path,
            reported_done_at,
            report_reason,
        }
    }

    /// R11: the persisted report_done marker reconstructs onto the
    /// adopted session — status="reported" survives a brain restart.
    #[test]
    fn adopted_meta_restores_done_report_and_transcript_path() {
        let now = unix_now();
        let e = entry_with(
            Some(now - 30.0),
            Some("all tests green".into()),
            Some("/tmp/abc.jsonl".into()),
        );
        let meta = adopted_meta_from_manifest_entry("ws-1", &e);
        assert_eq!(meta.workspace_id, "ws-1");
        assert_eq!(meta.transcript_path.as_deref(), Some("/tmp/abc.jsonl"));
        assert_eq!(meta.global_perms, true);
        assert_eq!(meta.memory_cap_soft_bytes, Some(1024));
        let dr = meta.done_report.expect("marker restored");
        assert_eq!(dr.at_unix, now - 30.0);
        assert_eq!(dr.reason.as_deref(), Some("all tests green"));
        // The reconstructed Instant is ~30s old (the age math), with
        // slack for test scheduling.
        let age = dr.at_instant.elapsed().as_secs_f64();
        assert!((25.0..40.0).contains(&age), "age {age}");
    }

    #[test]
    fn adopted_meta_without_marker_stays_unreported() {
        let e = entry_with(None, None, None);
        let meta = adopted_meta_from_manifest_entry("ws-1", &e);
        assert!(meta.done_report.is_none());
        assert!(meta.transcript_path.is_none());
    }

    /// A future-dated stamp (clock skew) clamps to now instead of
    /// panicking on Instant underflow.
    #[test]
    fn instant_at_unix_tolerates_future_stamps() {
        let i = instant_at_unix(unix_now() + 3600.0);
        assert!(i.elapsed().as_secs() < 5);
    }
}
