//! The holder⇄brain channel protocol — framing, SCM_RIGHTS law, and
//! the verb payload types. DESIGN_HOLDER_BRAIN_SPLIT § "The channel"
//! + § "Protocol verbs" (phase 2).
//!
//! ## Transport
//!
//! The channel is an inherited `socketpair(AF_UNIX, SOCK_STREAM)` —
//! never a named socket (§ The channel: unforgeable, exactly one
//! brain). Frames are a 4-byte LE length prefix followed by one JSON
//! object:
//!
//! ```json
//! { "v": "<verb>", "req_id": <u64?>, "nfds": <u8>, "body": { ... } }
//! ```
//!
//! - `v` — the verb name ([`verbs`]). Unknown verbs get a typed
//!   [`ERR_UNSUPPORTED_VERB`] reply, never a disconnect.
//! - `req_id` — present on every brain→holder request (all of them
//!   except `pong`); the reply echoes it (review finding C7 — the
//!   brain runs concurrent callers over one stream and matches
//!   replies by id). Unsolicited holder→brain frames (`exit_event`,
//!   `ping`, a future re-`hello`) carry none; their acknowledgement
//!   is by their own keys.
//! - `nfds` — the number of SCM_RIGHTS fds this frame carries
//!   (declared, so the receiver can enforce the law below).
//! - `body` — the verb's payload struct. Unknown fields inside it
//!   are ignored everywhere: `deny_unknown_fields` is banned in this
//!   crate (the additive-only discipline).
//!
//! ## Frame length cap (C7)
//!
//! [`MAX_FRAME_BYTES`] bounds the JSON payload on BOTH sides,
//! enforced before allocation — a buggy peer must not be able to
//! make the other side allocate from an arbitrary length prefix
//! (the same rule the daemon's control wire applies).
//!
//! ## SCM_RIGHTS law (O15)
//!
//! 1. One `sendmsg` per fd-bearing frame: the ancillary fds ride
//!    with that frame's FIRST byte and are never coalesced with
//!    another frame's bytes in the same `sendmsg`. (A partial send
//!    may continue with plain writes — the fds already traveled
//!    with the first segment.)
//! 2. Receivers always supply a control buffer and always pass
//!    `MSG_CMSG_CLOEXEC` (S10 — a received fd must never be
//!    inheritable by a concurrently forked child).
//! 3. `MSG_CTRUNC`, a completed frame whose collected fds ≠ its
//!    declared `nfds`, or an oversized length prefix are protocol
//!    violations: the receiving side treats the CHANNEL as dead
//!    (the brain exits and is respawned; the holder counts a brain
//!    failure). Never resynchronize past a violation — fd/frame
//!    misassociation is cross-session PTY access, the worst outcome
//!    available.
//! 4. Unexpected fds on a frame that declared fewer are closed
//!    immediately (the `OwnedFd`s drop), never stored.

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

use serde::{Deserialize, Serialize};

/// Lowest/highest channel protocol version this build speaks. The
/// negotiated version is `min(brain_max, holder_max)` when the
/// ranges overlap; no overlap refuses with [`ERR_PROTO_MISMATCH`].
pub const PROTO_VERSION_MIN: u32 = 1;
pub const PROTO_VERSION_MAX: u32 = 1;

/// Hard cap on one frame's JSON payload (the 4-byte prefix not
/// included). Same order as the manifest's `MAX_MANIFEST_BYTES` —
/// nothing on this channel is bulk data (the data plane rides fd
/// dups, § "Why write/resize/read are NOT verbs").
pub const MAX_FRAME_BYTES: u32 = 1024 * 1024;

/// Bootstrap env var carrying the brain's socketpair end fd number.
/// A bootstrap pointer only — same trust model as
/// `CM_REEXEC_MANIFEST_FD` (nothing with authority rides the env).
pub const ENV_CHANNEL_FD: &str = "CM_HOLDER_CHANNEL_FD";

/// Verb names. Phase 2 implements the first eleven; the rest of the
/// design's table lands in later phases and answers
/// [`ERR_UNSUPPORTED_VERB`] until then.
pub mod verbs {
    pub const HELLO: &str = "hello";
    pub const HELLO_REPLY: &str = "hello_reply";
    pub const SPAWN: &str = "spawn";
    pub const ARM_REAP: &str = "arm_reap";
    pub const ADOPT: &str = "adopt";
    pub const ADOPT_RECORD: &str = "adopt_record";
    pub const ADOPT_LISTENERS: &str = "adopt_listeners";
    pub const ADOPT_DONE: &str = "adopt_done";
    pub const SIGNAL: &str = "signal";
    pub const ABORT_SPAWN: &str = "abort_spawn";
    pub const FORGET: &str = "forget";
    pub const EXIT_EVENT: &str = "exit_event";
    pub const ACK_EXIT: &str = "ack_exit";
    pub const STATUS: &str = "status";
    pub const PING: &str = "ping";
    pub const PONG: &str = "pong";
    pub const OK: &str = "ok";
    pub const ERR: &str = "err";
}

/// Error codes carried by [`ErrBody`].
pub const ERR_UNSUPPORTED_VERB: &str = "unsupported_verb";
pub const ERR_PROTO_MISMATCH: &str = "proto_mismatch";
pub const ERR_UID_EXISTS: &str = "uid_exists";
pub const ERR_OPENPTY_FAILED: &str = "openpty_failed";
pub const ERR_EXEC_FAILED: &str = "exec_failed";
pub const ERR_NOT_FOUND: &str = "not_found";
pub const ERR_NOT_EXITED: &str = "not_exited";
pub const ERR_UNACKED: &str = "unacked";
pub const ERR_ALREADY_EXITED: &str = "already_exited";
pub const ERR_INVALID: &str = "invalid";

// ============================================================
// Payload types (all additive-tolerant: no deny_unknown_fields)
// ============================================================

/// Brain → holder, the first frame on every channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloBody {
    pub proto_min: u32,
    pub proto_max: u32,
    pub brain_build_id: String,
}

/// Holder → brain, the `hello` reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloReplyBody {
    /// The negotiated version: `min(brain_max, holder_max)`.
    pub proto: u32,
    pub holder_build_id: String,
    pub holder_proto_min: u32,
    pub holder_proto_max: u32,
    /// The holder's brain-spawn counter (design § The holder).
    pub epoch: u64,
    pub session_count: usize,
}

/// Brain → holder: fork/exec a session child. The `env` map is the
/// COMPLETE child environment (design § Environment, S1) — the
/// holder applies it over `env_clear`, so the holder's own environ
/// reaches no session, ever.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnBody {
    pub uid: String,
    /// Opaque brain metadata (the transcript-rotation counter today);
    /// NEVER identity — that is the holder-minted incarnation (O2).
    #[serde(default)]
    pub generation_meta: u64,
    pub argv: Vec<String>,
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub cwd: Option<String>,
    pub cols: u16,
    pub rows: u16,
    #[serde(default)]
    pub cgroup_prefix: Option<String>,
}

/// Holder → brain, the `spawn` success reply. Carries 2 fds:
/// `[master_dup, pidfd_dup]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnOkBody {
    pub incarnation: u64,
    pub pid: i32,
    pub child_start_time: u64,
}

/// Brain → holder: authorize reaping + arm this brain generation's
/// exit-event delivery for one record. Sent under the brain's state
/// lock immediately after registry insert (the `arm_reaper`
/// discipline relocated onto the wire — S4/C9).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArmReapBody {
    pub uid: String,
    pub incarnation: u64,
    /// The DISCOVERED cgroup scope path (V5 — `cgroup_prefix` at
    /// spawn is caller metadata; the real path only exists
    /// post-spawn via `/proc/<pid>/cgroup`). Stored for the
    /// `memory.events` carve-out read at `waitid` time.
    #[serde(default)]
    pub cgroup_path: Option<String>,
}

/// Holder → brain: one session record in the adopt stream. Carries
/// 2 fds: `[master_dup, pidfd_dup]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdoptRecordBody {
    pub uid: String,
    pub incarnation: u64,
    #[serde(default)]
    pub generation_meta: u64,
    pub child_pid: i32,
    pub child_start_time: u64,
    #[serde(default)]
    pub cgroup_prefix: Option<String>,
    #[serde(default)]
    pub cgroup_path: Option<String>,
    #[serde(default)]
    pub watcher_checkpoint: Option<serde_json::Value>,
    #[serde(default)]
    pub last_signal_request: Option<LastSignalRequest>,
    /// Reap authorization already granted (by any brain generation).
    pub reap_armed: bool,
    /// The child has exited and its status was consumed — the V2
    /// label the adopt reconciliation branches on (a reaped record
    /// with no pending event whose exit the snapshot already
    /// tombstoned is `forget`-not-adopt).
    pub reaped: bool,
    /// A consumed exit status is queued, awaiting `ack_exit`.
    pub exit_event_pending: bool,
}

/// Holder → brain: the custodied listeners (phase 4 — phase 2 always
/// sends an empty list with 0 fds, keeping the adopt flow's shape).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdoptListenersBody {
    pub listeners: Vec<serde_json::Value>,
}

/// Holder → brain: end of the adopt stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdoptDoneBody {
    pub exit_events_pending: usize,
}

/// Brain → holder: signal a session child, with attribution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalBody {
    pub uid: String,
    pub incarnation: u64,
    pub sig: i32,
    /// The `killed_by` who-or-what string, echoed in the exit event
    /// (survives brain death — the design's gap-5 closure).
    pub attribution: String,
}

/// The holder's record of the last attributed signal request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastSignalRequest {
    pub sig: i32,
    pub attribution: String,
    /// Unix seconds (holder clock).
    pub at: f64,
}

/// Brain → holder: abandon a spawn pre-insert (the
/// `PendingSession`-abort translation, S4/O6): SIGKILL, reap
/// unconditionally, drop the record, emit NO exit event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbortSpawnBody {
    pub uid: String,
    pub incarnation: u64,
}

/// Brain → holder: post-exit GC of a reaped, acked record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgetBody {
    pub uid: String,
    pub incarnation: u64,
}

/// Holder → brain: an authoritative exit. Unsolicited; redelivered
/// on every brain generation (once the record is armed) until
/// `ack_exit`. Idempotent brain-side by `(uid, incarnation)` (C4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExitEventBody {
    pub uid: String,
    pub incarnation: u64,
    pub code: Option<i32>,
    pub signal: Option<i32>,
    /// Unix seconds, stamped at `waitid` time by the HOLDER's clock
    /// (fidelity across brain downtime).
    pub exited_at: f64,
    #[serde(default)]
    pub last_signal_request: Option<LastSignalRequest>,
    /// Best-effort raw `memory.events` snapshot read at `waitid`
    /// time when the record carries a cgroup path (the frozenness
    /// carve-out, S6).
    #[serde(default)]
    pub memory_events_snapshot: Option<String>,
}

/// Brain → holder: the exit event's durable-persistence ack (C4:
/// sent only after the checked tombstone write succeeded).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AckExitBody {
    pub uid: String,
    pub incarnation: u64,
}

/// Holder → brain, the `status` reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusReplyBody {
    pub sessions: usize,
    pub pending_exit_events: usize,
    pub epoch: u64,
    pub brain_restarts: u64,
    /// Phase 6 wires the real breaker; until then "none".
    pub breaker_state: String,
    pub holder_build_id: String,
    /// Consecutive pings the current brain has not answered.
    pub pings_unanswered: u64,
}

/// Watchdog heartbeat (holder → brain / brain → holder). The pong
/// MUST be answered by the brain's channel-reader thread, touching
/// no lock (S9).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PingBody {
    pub seq: u64,
}

/// Generic success reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OkBody {
    #[serde(default)]
    pub detail: Option<serde_json::Value>,
}

/// Generic typed-error reply. `code` is one of the `ERR_*` strings;
/// extra context rides `detail`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrBody {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub detail: Option<serde_json::Value>,
}

// ============================================================
// Frame envelope
// ============================================================

/// One decoded frame. `body` stays a raw JSON value until the
/// dispatcher knows the verb — unknown verbs must be answerable
/// without parsing their body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
    pub v: String,
    #[serde(default)]
    pub req_id: Option<u64>,
    #[serde(default)]
    pub nfds: u8,
    #[serde(default)]
    pub body: serde_json::Value,
}

impl Frame {
    pub fn new(verb: &str, req_id: Option<u64>, nfds: u8, body: impl Serialize) -> Frame {
        Frame {
            v: verb.to_string(),
            req_id,
            nfds,
            body: serde_json::to_value(body).expect("frame body serializes"),
        }
    }

    /// Parse the body as the verb's payload type. Extra fields are
    /// ignored (additive-only discipline).
    pub fn parse_body<T: for<'de> Deserialize<'de>>(&self) -> Result<T, String> {
        serde_json::from_value(self.body.clone()).map_err(|e| e.to_string())
    }

    /// Encode to wire bytes: 4-byte LE payload length + JSON.
    pub fn to_wire(&self) -> Vec<u8> {
        let payload = serde_json::to_vec(self).expect("frame serializes");
        assert!(
            payload.len() as u32 <= MAX_FRAME_BYTES,
            "outbound frame exceeds MAX_FRAME_BYTES — a sender bug"
        );
        let mut out = Vec::with_capacity(4 + payload.len());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&payload);
        out
    }
}

/// A protocol violation — the channel must be treated as dead (law
/// item 3); never resynchronize past one.
#[derive(Debug)]
pub struct ProtocolViolation(pub String);

impl std::fmt::Display for ProtocolViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "channel protocol violation: {}", self.0)
    }
}
impl std::error::Error for ProtocolViolation {}

// ============================================================
// Send side
// ============================================================

/// Send the FIRST segment of a frame with its fds attached (one
/// `sendmsg`, ancillary bound to the frame's first byte — law item
/// 1). Returns the number of payload bytes accepted; the caller
/// continues any remainder with [`send_rest`] (plain writes — the
/// fds already traveled). `WouldBlock` means nothing was sent and
/// the fds did NOT travel: retry the whole call later.
pub fn send_first(fd: BorrowedFd<'_>, bytes: &[u8], fds: &[RawFd]) -> io::Result<usize> {
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    // Control buffer for up to MAX_FDS_PER_FRAME fds.
    let space = unsafe { libc::CMSG_SPACE((fds.len() * std::mem::size_of::<RawFd>()) as u32) }
        as usize;
    let mut cbuf = vec![0u8; space.max(1)];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if !fds.is_empty() {
        msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = space;
        // SAFETY: msg_control points at a buffer of CMSG_SPACE bytes.
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len =
                libc::CMSG_LEN((fds.len() * std::mem::size_of::<RawFd>()) as u32) as _;
            let data = libc::CMSG_DATA(cmsg) as *mut RawFd;
            for (i, f) in fds.iter().enumerate() {
                data.add(i).write_unaligned(*f);
            }
        }
    }
    // SAFETY: msg is fully initialized; iov points at `bytes`.
    let n = unsafe { libc::sendmsg(fd.as_raw_fd(), &msg, libc::MSG_NOSIGNAL) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

/// Continue a partially-sent frame (no ancillary — law item 1).
pub fn send_rest(fd: BorrowedFd<'_>, bytes: &[u8]) -> io::Result<usize> {
    // SAFETY: plain send over the connected stream socket.
    let n = unsafe {
        libc::send(
            fd.as_raw_fd(),
            bytes.as_ptr() as *const libc::c_void,
            bytes.len(),
            libc::MSG_NOSIGNAL,
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

/// Blocking convenience for the (test/brain) side: send a whole
/// frame + fds, looping past partial writes — and past EAGAIN, so it
/// behaves blockingly even over a nonblocking fd.
pub fn send_frame_blocking(
    fd: BorrowedFd<'_>,
    frame: &Frame,
    fds: &[RawFd],
) -> io::Result<()> {
    debug_assert_eq!(frame.nfds as usize, fds.len(), "nfds must match");
    let bytes = frame.to_wire();
    let mut sent = 0usize;
    loop {
        let res = if sent == 0 {
            send_first(fd, &bytes, fds)
        } else {
            send_rest(fd, &bytes[sent..])
        };
        match res {
            Ok(n) => {
                sent += n;
                if sent >= bytes.len() {
                    return Ok(());
                }
            }
            Err(e)
                if e.raw_os_error() == Some(libc::EAGAIN)
                    || e.raw_os_error() == Some(libc::EINTR) =>
            {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Err(e) => return Err(e),
        }
    }
}

// ============================================================
// Receive side
// ============================================================

/// Tolerated ancillary capacity per `recvmsg` (frames declare ≤ 2
/// fds; extra headroom exists only so a law violation is DETECTED
/// as a count mismatch instead of silently truncated).
const MAX_FDS_PER_RECV: usize = 8;

/// What one [`FrameReader::feed`] call observed.
#[derive(Debug, PartialEq)]
pub enum FeedStatus {
    /// Bytes (and possibly fds) arrived.
    Progress,
    /// Nonblocking socket had nothing (EAGAIN).
    WouldBlock,
    /// Orderly EOF — the peer is gone.
    Eof,
}

/// Incremental frame decoder enforcing the SCM_RIGHTS law. Feed it
/// from `recvmsg` (it always supplies a control buffer and always
/// passes `MSG_CMSG_CLOEXEC` — law item 2), then drain completed
/// frames with [`FrameReader::next_frame`].
pub struct FrameReader {
    buf: Vec<u8>,
    /// fds received but not yet claimed by a completed frame.
    pending_fds: VecDeque<OwnedFd>,
}

impl Default for FrameReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameReader {
    pub fn new() -> FrameReader {
        FrameReader {
            buf: Vec::new(),
            pending_fds: VecDeque::new(),
        }
    }

    /// One `recvmsg` into the buffer. 64 KiB reads: frames are small
    /// (≤ [`MAX_FRAME_BYTES`]); the channel is a control plane.
    pub fn feed(&mut self, fd: BorrowedFd<'_>) -> Result<FeedStatus, ProtocolViolation> {
        let mut data = [0u8; 65536];
        let mut iov = libc::iovec {
            iov_base: data.as_mut_ptr() as *mut libc::c_void,
            iov_len: data.len(),
        };
        let space = unsafe {
            libc::CMSG_SPACE((MAX_FDS_PER_RECV * std::mem::size_of::<RawFd>()) as u32)
        } as usize;
        let mut cbuf = vec![0u8; space];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = space;
        // SAFETY: msg fully initialized; buffers live for the call.
        let n = unsafe { libc::recvmsg(fd.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
        if n < 0 {
            let err = io::Error::last_os_error();
            return match err.raw_os_error() {
                Some(libc::EAGAIN) => Ok(FeedStatus::WouldBlock),
                Some(libc::EINTR) => Ok(FeedStatus::WouldBlock),
                _ => Err(ProtocolViolation(format!("recvmsg: {err}"))),
            };
        }
        if msg.msg_flags & libc::MSG_CTRUNC != 0 {
            return Err(ProtocolViolation(
                "MSG_CTRUNC: ancillary data truncated (law item 3)".into(),
            ));
        }
        // Collect any fds (already CLOEXEC via MSG_CMSG_CLOEXEC).
        // SAFETY: cmsg iteration over the kernel-filled control buf.
        unsafe {
            let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level == libc::SOL_SOCKET
                    && (*cmsg).cmsg_type == libc::SCM_RIGHTS
                {
                    let payload_len =
                        (*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                    let count = payload_len / std::mem::size_of::<RawFd>();
                    let data_ptr = libc::CMSG_DATA(cmsg) as *const RawFd;
                    for i in 0..count {
                        let raw = data_ptr.add(i).read_unaligned();
                        self.pending_fds.push_back(OwnedFd::from_raw_fd(raw));
                    }
                }
                cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
            }
        }
        if n == 0 {
            return Ok(FeedStatus::Eof);
        }
        self.buf.extend_from_slice(&data[..n as usize]);
        Ok(FeedStatus::Progress)
    }

    /// Pop the next complete frame, claiming exactly its declared
    /// fds. `Ok(None)` = need more bytes.
    pub fn next_frame(
        &mut self,
    ) -> Result<Option<(Frame, Vec<OwnedFd>)>, ProtocolViolation> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]);
        if len > MAX_FRAME_BYTES {
            return Err(ProtocolViolation(format!(
                "frame length {len} exceeds MAX_FRAME_BYTES {MAX_FRAME_BYTES} (C7)"
            )));
        }
        let total = 4 + len as usize;
        if self.buf.len() < total {
            return Ok(None);
        }
        let frame: Frame = serde_json::from_slice(&self.buf[4..total])
            .map_err(|e| ProtocolViolation(format!("frame JSON: {e}")))?;
        self.buf.drain(..total);
        // Claim declared fds; a mismatch when the frame is complete
        // is a violation (law item 3) — with one tolerance: fds for
        // the NEXT frame cannot have arrived yet under law item 1,
        // so any surplus is a genuine violation too.
        let want = frame.nfds as usize;
        if self.pending_fds.len() < want {
            return Err(ProtocolViolation(format!(
                "frame '{}' declared {} fds, {} arrived",
                frame.v,
                want,
                self.pending_fds.len()
            )));
        }
        let fds: Vec<OwnedFd> = self.pending_fds.drain(..want).collect();
        if !self.pending_fds.is_empty() && self.buf.is_empty() {
            // Surplus fds with no in-flight next frame: close them
            // (drop) and refuse (law item 4 detection).
            let surplus = self.pending_fds.len();
            self.pending_fds.clear();
            return Err(ProtocolViolation(format!(
                "{surplus} undeclared fd(s) on frame '{}'",
                frame.v
            )));
        }
        Ok(Some((frame, fds)))
    }
}

/// Blocking convenience for the (test/brain) side: read one frame.
pub fn recv_frame_blocking(
    fd: BorrowedFd<'_>,
    reader: &mut FrameReader,
) -> Result<Option<(Frame, Vec<OwnedFd>)>, ProtocolViolation> {
    loop {
        if let Some(hit) = reader.next_frame()? {
            return Ok(Some(hit));
        }
        match reader.feed(fd)? {
            FeedStatus::Eof => {
                return if reader.buf.is_empty() {
                    Ok(None)
                } else {
                    Err(ProtocolViolation("EOF mid-frame".into()))
                }
            }
            FeedStatus::Progress | FeedStatus::WouldBlock => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrips_through_wire_encoding() {
        let f = Frame::new(
            verbs::SPAWN,
            Some(7),
            2,
            SpawnBody {
                uid: "s1".into(),
                generation_meta: 3,
                argv: vec!["/bin/cat".into()],
                env: Default::default(),
                cwd: None,
                cols: 80,
                rows: 24,
                cgroup_prefix: None,
            },
        );
        let wire = f.to_wire();
        let len = u32::from_le_bytes([wire[0], wire[1], wire[2], wire[3]]) as usize;
        assert_eq!(len + 4, wire.len());
        let back: Frame = serde_json::from_slice(&wire[4..]).unwrap();
        assert_eq!(back.v, verbs::SPAWN);
        assert_eq!(back.req_id, Some(7));
        assert_eq!(back.nfds, 2);
        let body: SpawnBody = back.parse_body().unwrap();
        assert_eq!(body.uid, "s1");
        assert_eq!(body.generation_meta, 3);
    }

    #[test]
    fn unknown_fields_are_ignored_everywhere() {
        // Envelope-level AND body-level unknown fields must parse —
        // the additive-only discipline's canary.
        let json = serde_json::json!({
            "v": "arm_reap",
            "req_id": 1,
            "nfds": 0,
            "body": {"uid": "x", "incarnation": 4,
                     "future_field": {"deep": true}},
            "future_envelope_field": 9,
        });
        let f: Frame = serde_json::from_value(json).unwrap();
        let b: ArmReapBody = f.parse_body().unwrap();
        assert_eq!(b.uid, "x");
        assert_eq!(b.incarnation, 4);
        assert_eq!(b.cgroup_path, None);
    }

    #[test]
    fn oversized_length_prefix_is_refused_before_allocation() {
        let mut r = FrameReader::new();
        r.buf
            .extend_from_slice(&(MAX_FRAME_BYTES + 1).to_le_bytes());
        r.buf.extend_from_slice(b"junk");
        let err = r.next_frame().unwrap_err();
        assert!(err.0.contains("exceeds MAX_FRAME_BYTES"), "{}", err.0);
    }

    #[test]
    fn frames_and_fds_roundtrip_over_a_real_socketpair() {
        let mut sv = [0i32; 2];
        // SAFETY: valid out-array for socketpair.
        let ret = unsafe {
            libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0, sv.as_mut_ptr())
        };
        assert_eq!(ret, 0);
        // SAFETY: socketpair succeeded; we own both.
        let (a, b) = (unsafe { OwnedFd::from_raw_fd(sv[0]) }, unsafe {
            OwnedFd::from_raw_fd(sv[1])
        });

        // An fd to pass: a pipe end whose identity we can verify by
        // writing through the received copy.
        let mut pipefd = [0i32; 2];
        // SAFETY: valid out-array.
        assert_eq!(unsafe { libc::pipe(pipefd.as_mut_ptr()) }, 0);
        // SAFETY: pipe succeeded.
        let (pr, pw) = (unsafe { OwnedFd::from_raw_fd(pipefd[0]) }, unsafe {
            OwnedFd::from_raw_fd(pipefd[1])
        });

        let f = Frame::new(verbs::OK, Some(1), 1, OkBody { detail: None });
        send_frame_blocking(a.as_fd(), &f, &[pw.as_raw_fd()]).unwrap();

        let mut reader = FrameReader::new();
        let (frame, fds) = recv_frame_blocking(b.as_fd(), &mut reader)
            .unwrap()
            .unwrap();
        assert_eq!(frame.v, verbs::OK);
        assert_eq!(fds.len(), 1);
        // Write through the received dup; read from the pipe's
        // original read end.
        // SAFETY: valid fd + buffer.
        let n = unsafe {
            libc::write(fds[0].as_raw_fd(), b"hi".as_ptr() as *const _, 2)
        };
        assert_eq!(n, 2);
        let mut buf = [0u8; 2];
        // SAFETY: valid fd + buffer.
        let n = unsafe { libc::read(pr.as_raw_fd(), buf.as_mut_ptr() as *mut _, 2) };
        assert_eq!(n, 2);
        assert_eq!(&buf, b"hi");
        use std::os::fd::AsFd;
        drop((a, b, pr));
    }

    #[test]
    fn undeclared_fds_are_a_violation_and_get_closed() {
        let mut sv = [0i32; 2];
        // SAFETY: valid out-array.
        let ret = unsafe {
            libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0, sv.as_mut_ptr())
        };
        assert_eq!(ret, 0);
        // SAFETY: socketpair succeeded.
        let (a, b) = (unsafe { OwnedFd::from_raw_fd(sv[0]) }, unsafe {
            OwnedFd::from_raw_fd(sv[1])
        });
        use std::os::fd::AsFd;
        // Frame declares 0 fds but the sendmsg smuggles one.
        let f = Frame::new(verbs::OK, Some(1), 0, OkBody { detail: None });
        let mut pipefd = [0i32; 2];
        // SAFETY: valid out-array.
        assert_eq!(unsafe { libc::pipe(pipefd.as_mut_ptr()) }, 0);
        // SAFETY: pipe succeeded.
        let (_pr, pw) = (unsafe { OwnedFd::from_raw_fd(pipefd[0]) }, unsafe {
            OwnedFd::from_raw_fd(pipefd[1])
        });
        let bytes = f.to_wire();
        let sent = send_first(a.as_fd(), &bytes, &[pw.as_raw_fd()]).unwrap();
        assert_eq!(sent, bytes.len());

        let mut reader = FrameReader::new();
        let err = recv_frame_blocking(b.as_fd(), &mut reader).unwrap_err();
        assert!(err.0.contains("undeclared fd"), "{}", err.0);
    }
}
