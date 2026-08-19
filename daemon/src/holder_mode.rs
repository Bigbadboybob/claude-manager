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
use std::os::fd::{AsFd, FromRawFd, OwnedFd, RawFd};
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
                eprintln!("cm-daemon: holder channel EOF — exiting (the holder is gone or replacing us)");
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
        let frame = Frame::new(verb, Some(req_id), 0, body);
        if let Err(e) = self.send_raw(&frame, &[]) {
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
    /// with fresh fd dups, then the done marker.
    #[allow(clippy::type_complexity)]
    pub fn adopt(
        &self,
    ) -> Result<(Vec<(ch::AdoptRecordBody, OwnedFd, OwnedFd)>, ch::AdoptDoneBody), HolderError>
    {
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
        let done = loop {
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
                verbs::ADOPT_LISTENERS => { /* phase 4 */ }
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
        Ok((records, done))
    }
}

fn holder_exit_from_event(ev: &ch::ExitEventBody) -> HolderExit {
    HolderExit {
        status: DaemonExitStatus {
            code: ev.code,
            signal: ev.signal,
        },
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
    pid: libc::pid_t,
    client: Arc<HolderClient>,
}

impl HolderPending {
    pub fn pid(&self) -> libc::pid_t {
        self.pid
    }

    /// Arm under the state lock (same discipline as `arm_reaper`):
    /// wires the exit observer to the client's subscription, the
    /// settle (ack + forget) hook, and the verb-routed kill.
    pub fn arm(
        mut self,
        on_exit: Option<crate::session::OnExitCallback>,
    ) -> anyhow::Result<DaemonSession> {
        let build = self.build.take().expect("arm called once");
        let events = self.client.subscribe_exit(&self.uid, self.incarnation);
        let settle_client = Arc::clone(&self.client);
        let (uid_s, inc_s) = (self.uid.clone(), self.incarnation);
        let settle: Box<dyn FnOnce() + Send> = Box::new(move || {
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
pub fn adopt_at_boot(state_arc: &Arc<Mutex<DaemonState>>, client: &Arc<HolderClient>) {
    let (records, done) = match client.adopt() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("cm-daemon: holder adopt failed: {e} — exiting (holder respawns us)");
            std::process::exit(70);
        }
    };
    if records.is_empty() {
        eprintln!("cm-daemon: holder adopt: no sessions held");
        return;
    }
    eprintln!(
        "cm-daemon: holder adopt: {} record(s), {} pending exit event(s)",
        records.len(),
        done.exit_events_pending
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
        let meta = match persisted.get(&rec.uid) {
            Some((ws_id, e)) => adopted_meta_from_manifest_entry(ws_id, e),
            None => {
                eprintln!(
                    "cm-daemon: holder record '{}' unknown to the persisted registry — orphan_adopted",
                    rec.uid
                );
                orphan_meta(&rec.uid)
            }
        };
        let candidate = crate::adopt::SessionCandidate::from_raw_parts(
            rec.uid.clone(),
            rec.child_pid,
            pidfd,
            master,
        );
        let parts = candidate.promote();
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
            pid: rec.child_pid,
            client: Arc::clone(client),
        };
        let state_for_cleanup = Arc::clone(state_arc);
        let uid_for_cleanup = rec.uid.clone();
        let on_exit: crate::session::OnExitCallback = Box::new(move |_status| {
            let mut s = state_for_cleanup
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            crate::control::methods::handle_session_exit(&mut s, &uid_for_cleanup);
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
            match pending.arm(Some(on_exit)) {
                Ok(sess) => {
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
        if let Err(e) = client.arm_reap(&rec.uid, rec.incarnation, rec.cgroup_path.clone()) {
            eprintln!("cm-daemon: arm_reap {}: {e}", rec.uid);
        }
        eprintln!(
            "cm-daemon: holder adopt '{}' (pid {}, incarnation {})",
            rec.uid, rec.child_pid, rec.incarnation
        );
    }
    let count = {
        let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        st.persist_sessions_best_effort();
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
        // The persisted entry carries the transcript ID, not the
        // path; the path rebinds via `session.set_transcript_path`
        // (TUI detection) — a named phase-4 gap, harmless for
        // control-plane recovery.
        transcript_path: None,
        memory_cap_soft_bytes: e.memory_cap_soft_bytes,
        memory_cap_hard_bytes: e.memory_cap_hard_bytes,
        cgroup_prefix: e.cgroup_prefix.clone(),
        workflow_run_id: e.workflow_run_id.clone(),
        workflow_role: e.workflow_role.clone(),
        continuous_task_id: e.continuous_task_id.clone(),
        global_perms: e.global_perms,
        generation: e.generation,
        last_activity_at: None,
        last_input_at: None,
        last_operator_input_at: None,
        last_turn_end_at: None,
        done_report: None,
        kills_dir: e
            .memory_cap_soft_bytes
            .map(|_| crate::path::default_kills_dir())
            .flatten(),
        kills_baseline: None,
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
