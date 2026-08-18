//! Sealed re-exec manifest — the serialization/validation half of the
//! in-place-restart FD handoff. DESIGN_SEAMLESS_RESTART phase 3a
//! (restart-sequence step 4; review findings R7, R8, R14).
//!
//! ## Why this exists
//!
//! When a future `daemon.restart` execs the new binary in the same
//! process (the design's step 5), every durable kernel object — each
//! session's PTY master and spawn-time pidfd, the control-socket
//! listener, the TLS listener when configured, the pinned rollback
//! executable (R7) — rides across the exec as an inherited FD. The
//! manifest is the record of WHICH inherited fd is what. This module
//! owns its wire format and both trust boundaries: the old image's
//! write side ([`write_manifest`]) and the new image's read/validate
//! side ([`read_manifest`] + [`validate_fd_roles`]). There is
//! deliberately **no exec, no CLOEXEC manipulation, and no handoff
//! wiring anywhere in this module** — the exec skeleton (phase 3b)
//! consumes it; everything here is fully testable against throwaway
//! fds in an ordinary test process.
//!
//! ## Trust model (R8)
//!
//! The environment is a **bootstrap pointer, not a trust surface**:
//! the only env var is [`ENV_MANIFEST_FD`], carrying nothing but the
//! manifest fd number. Everything with authority — the attempt
//! counter (the rollback state machine's loop bound), the rollback
//! binary fd, the session records — lives INSIDE the sealed memfd,
//! whose integrity the kernel enforces (memfd seals) and the envelope
//! re-checks (magic / format version / explicit length / SHA-256).
//! Nothing an env-editing process can write into `CM_REEXEC_*` can
//! forge attempts or redirect the rollback exec. A fresh daemon
//! started from a leaked environment — the 2026-08-18 pattern, where
//! a daemon armed from inside a hosted session bequeathed session
//! identity to every child — that sees the var but fails validation
//! must scrub it and boot as a normal fresh start; [`consume_env`]
//! performs the scrub unconditionally at read time so no caller can
//! forget (R14). The `run()` wiring that sequences detect → validate
//! → (rehydrate | scrub-and-boot-fresh) is 3b's job.
//!
//! ## The corrupt-manifest rule (design → "Failure classes")
//!
//! "On a manifest that fails validation outright: touch NO escrow fd
//! — never trust a rollback path or PID from a corrupt manifest."
//! [`read_manifest`] therefore touches only the manifest fd itself
//! (fstat/fstatfs/readlink/F_GET_SEALS/pread on that one fd); every
//! escrowed fd NAMED in the manifest stays untouched no matter how
//! validation fails. [`validate_fd_roles`] is the first call that
//! touches escrow fds, and it is a SEPARATE function precisely so the
//! caller (3b) sequences that boundary explicitly: integrity envelope
//! and structural checks first, then — and only then — role probes
//! against the fds themselves.
//!
//! ## The memfd offset gotcha (R8)
//!
//! A memfd's FILE OFFSET is shared state on the open file description
//! and **survives the exec**. Whatever the previous image left it at
//! — EOF after a plain `write(2)` writer, mid-file after a debugging
//! read — is what the new image inherits, so an ordinary `read()`
//! can return empty on a manifest that is entirely intact. The reader
//! therefore never touches the shared offset: all IO here is
//! positional (`pread`), and the writer is positional too (`pwrite`
//! at offset 0). The offset-independence is proven by test (read
//! with the offset deliberately parked at EOF).
//!
//! ## Checksum choice: SHA-256 over CRC32
//!
//! `sha2` is already a direct dependency of this crate (the memory-
//! cap kill-log's argv hashing in `session_watch.rs`), so the strong
//! primitive is free — CRC32 would mean a new crate or a hand-rolled
//! table for no benefit. The checksum's job is defending against
//! torn/partial writes and wrong-fd mixups, not adversaries (the
//! seals plus the memfd-identity checks carry the adversarial load);
//! at the ≤ 1 MiB cap SHA-256 costs microseconds.
//!
//! ## Scope
//!
//! Self-contained: no exec, no `daemon.restart` RPC, no CLOEXEC
//! audit, no rehydrate. The escrow/candidate types this manifest
//! feeds live in `crate::adopt` (phase 2c); the quiesce barrier that
//! makes the write-side snapshot coherent lives in
//! `crate::restart_coordinator` (phase 2d). Role-validation tests
//! against real fds (ptys, pidfds, listeners, children) live in
//! `daemon/tests/reexec_manifest_roles.rs` (own process, per the repo
//! convention for child-spawning suites — see
//! `daemon/tests/adopt_candidate.rs`).

use std::collections::HashSet;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The one environment variable of the handoff: the manifest fd
/// number, nothing else. See the module docs' trust model (R8) — the
/// attempt counter and rollback fd deliberately live INSIDE the
/// sealed manifest, never in env.
pub const ENV_MANIFEST_FD: &str = "CM_REEXEC_MANIFEST_FD";

/// Envelope format version (the binary framing: magic/length/
/// checksum layout). Distinct from [`MANIFEST_SCHEMA_VERSION`], which
/// versions the JSON payload's shape — the framing can survive a
/// payload reshape and vice versa.
pub const MANIFEST_FORMAT_VERSION: u32 = 1;

/// JSON payload schema version. Bump on any change to
/// [`ReexecManifest`] / [`SessionRecord`] shape; per the design's
/// state-schema rule, a schema bump ships with a legacy (drain)
/// restart, everything else re-execs.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// Hard cap on the manifest file's TOTAL size (envelope included).
/// Enforced on both sides: at write time (a coordinator bug fails
/// the restart while the old image is still in charge — before any
/// point of no return) and at read time (the reader trusts nothing
/// it didn't verify, and refuses to buffer unbounded input).
pub const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

/// Hard cap on the session count. Consistent with
/// [`MAX_MANIFEST_BYTES`]: ~250 bytes of JSON per session × 4096 is
/// the same order as the byte cap. Also enforced on both sides.
pub const MAX_SESSIONS: usize = 4096;

/// Name passed to `memfd_create(2)`. The read side verifies the fd's
/// `/proc/self/fd` link resolves to exactly this memfd name (see
/// [`read_manifest`] — deliberately an exact match, not the design's
/// "starts with", so `cm-reexec-manifest-evil` can't ride the
/// prefix).
const MEMFD_NAME: &str = "cm-reexec-manifest";

/// Distinctive magic at offset 0 — "CM Re-eXec".
const MAGIC: [u8; 4] = *b"CMRX";

/// Fixed envelope header: magic (4) + format version u32 LE (4) +
/// payload length u64 LE (8) + SHA-256 of the payload (32).
const HEADER_LEN: usize = 4 + 4 + 8 + 32;

/// All four seals, all required: SHRINK/GROW/WRITE freeze the bytes,
/// SEAL freezes the seal set itself (nobody un-seals downstream).
const REQUIRED_SEALS: libc::c_int = libc::F_SEAL_SHRINK
    | libc::F_SEAL_GROW
    | libc::F_SEAL_WRITE
    | libc::F_SEAL_SEAL;

/// The FD manifest: everything the new image needs to rebuild the
/// daemon around the inherited fds, per the design's step 4.
///
/// All `*_fd` fields are raw fd NUMBERS (`RawFd`, i.e. `i32` in the
/// JSON) — the manifest names inherited table slots; it never owns
/// them. Ownership of the escrowed fds belongs to the handoff path
/// (3b), which is also the only place CLOEXEC is cleared on them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReexecManifest {
    /// Payload schema version — must equal
    /// [`MANIFEST_SCHEMA_VERSION`] to be accepted (checked before
    /// anything in the payload is believed).
    pub schema_version: u32,
    /// Rollback attempt counter — the finite state machine's loop
    /// bound (design: attempt ≥ 2 → deliberate-kill terminal
    /// fallback). Lives HERE, inside the sealed manifest, so it
    /// cannot be looped by env alone (R8).
    pub attempt: u8,
    /// The pinned OLD executable (`/proc/self/exe` opened at restart
    /// time, step 1 — an inode, not a pathname, because deploys
    /// overwrite the path; R7). Validated as a regular file with
    /// execute permission by [`validate_fd_roles`]; only ever exec'd
    /// by 3b, and only when the manifest's integrity envelope
    /// verified — never trust a rollback fd from a corrupt manifest.
    pub rollback_bin_fd: RawFd,
    /// One record per live session (see [`SessionRecord`]).
    pub sessions: Vec<SessionRecord>,
    /// The control-socket `UnixListener` — inherited so the socket
    /// stays bound across the exec (no connection-refused window).
    pub listener_fd: RawFd,
    /// The TLS-TCP listener when `[tls]` is configured (R13); absent
    /// otherwise.
    pub tls_listener_fd: Option<RawFd>,
}

/// One live session's handoff record: identity, the two kernel
/// handles (canonical PTY master + spawn-time pidfd), and the
/// identity cross-checks the rehydrate transaction runs before
/// building anything signal-capable (R6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// Session uid — the registry key.
    pub uid: String,
    /// Session generation (monotonic across revives).
    pub generation: u64,
    /// Detected transcript id, when one exists — rehydrate
    /// cross-checks it against on-disk state.
    pub transcript_id: Option<String>,
    /// The child's numeric pid. NEVER an identity source on its own
    /// (pid reuse) — always paired with `pidfd` +
    /// `child_start_time`. Must be > 0.
    pub child_pid: i32,
    /// `/proc/<pid>/stat` field 22 (kernel start time, clock ticks
    /// since boot) captured at spawn — the R6 cross-check against
    /// pid reuse. Must be > 0 (the kernel value is nonzero for any
    /// process spawned after boot-instant zero).
    pub child_start_time: u64,
    /// The ONE canonical PTY master fd for this session (design step
    /// 4: reader/writer handles re-derive by dup post-exec, they are
    /// never separately manifest-listed).
    pub pty_master_fd: RawFd,
    /// The spawn-time pidfd itself — handed off, not re-acquired, so
    /// pid reuse is structurally impossible for signaling.
    pub pidfd: RawFd,
    /// The session's memory-cap cgroup scope prefix, when capped.
    pub cgroup_prefix: Option<String>,
    /// Memory-cap watcher policy checkpoint (protected PID set,
    /// breach counters, baselines — R12). PLACEHOLDER: the watcher
    /// checkpoint slice defines the shape; the manifest just carries
    /// it opaquely so the framing doesn't churn when it lands.
    pub watcher_checkpoint: Option<serde_json::Value>,
}

/// Typed validation failure — every variant names exactly what was
/// wrong, because the caller's failure handling branches on it (a
/// [`ManifestError::NotMemfd`] on a fresh start means "scrub env and
/// boot fresh"; a role mismatch mid-handoff means "rollback").
#[derive(Debug)]
pub enum ManifestError {
    /// The fd is not a sealed memfd of ours: wrong file type, wrong
    /// filesystem, wrong `/proc/self/fd` name, or a file that does
    /// not support sealing. `detail` says which probe refused.
    NotMemfd { fd: RawFd, detail: String },
    /// The memfd exists but is missing required seals — an unsealed
    /// (still-writable) manifest is untrusted by definition.
    MissingSeals {
        fd: RawFd,
        have: libc::c_int,
        want: libc::c_int,
    },
    /// File size exceeds [`MAX_MANIFEST_BYTES`].
    Oversize { size: u64, cap: u64 },
    /// File smaller than the fixed envelope header.
    TooSmall { size: u64, min: u64 },
    /// Envelope magic bytes are not [`MAGIC`].
    BadMagic { found: [u8; 4] },
    /// Envelope format version is not one this binary reads.
    UnsupportedFormatVersion { found: u32, supported: u32 },
    /// Declared payload length disagrees with the file size. Exact
    /// equality is required — sealing means the writer controls the
    /// size precisely, so trailing garbage is as disqualifying as
    /// truncation.
    BadLength { declared: u64, available: u64 },
    /// SHA-256 over the payload disagrees with the envelope header.
    ChecksumMismatch,
    /// The payload is not valid JSON for the manifest shape.
    Payload { detail: String },
    /// Payload schema version is not one this binary understands.
    UnsupportedSchemaVersion { found: u32, supported: u32 },
    /// Session count exceeds [`MAX_SESSIONS`].
    TooManySessions { count: usize, cap: usize },
    /// A manifest-listed fd number is negative.
    NegativeFd { role: &'static str, fd: RawFd },
    /// A manifest-listed fd number is in the stdio range 0–2. The
    /// handoff never escrows stdio; a manifest claiming otherwise is
    /// corrupt or hostile.
    StdioRangeFd { role: &'static str, fd: RawFd },
    /// The same fd number appears in two manifest slots.
    DuplicateFd { fd: RawFd },
    /// A session record's `child_pid` is not a valid pid (> 0).
    BadChildPid { uid: String, pid: i32 },
    /// A session record's `child_start_time` is zero — no real
    /// spawn-time capture produces that.
    BadChildStartTime { uid: String },
    /// An fd exists but is not the kind of object its manifest role
    /// requires (from [`validate_fd_roles`] — the only variant that
    /// can be produced by touching an escrow fd).
    FdRoleMismatch {
        role: &'static str,
        fd: RawFd,
        detail: String,
    },
    /// A syscall against the MANIFEST fd itself failed (never an
    /// escrow fd — those surface as [`ManifestError::FdRoleMismatch`]).
    Io {
        context: &'static str,
        source: io::Error,
    },
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManifestError::NotMemfd { fd, detail } => write!(
                f,
                "fd {} is not a sealed cm re-exec manifest memfd: {}",
                fd, detail
            ),
            ManifestError::MissingSeals { fd, have, want } => write!(
                f,
                "manifest memfd {} is missing seals: have {:#x}, need {:#x} \
                 — an unsealed manifest is untrusted by definition",
                fd, have, want
            ),
            ManifestError::Oversize { size, cap } => write!(
                f,
                "manifest is {} bytes, over the {} byte cap",
                size, cap
            ),
            ManifestError::TooSmall { size, min } => write!(
                f,
                "manifest is {} bytes, smaller than the {}-byte envelope header",
                size, min
            ),
            ManifestError::BadMagic { found } => write!(
                f,
                "bad envelope magic {:?} (expected {:?})",
                found, MAGIC
            ),
            ManifestError::UnsupportedFormatVersion { found, supported } => {
                write!(
                    f,
                    "unsupported envelope format version {} (this binary \
                     reads {})",
                    found, supported
                )
            }
            ManifestError::BadLength {
                declared,
                available,
            } => write!(
                f,
                "declared payload length {} != available bytes {} \
                 (truncated or trailing garbage)",
                declared, available
            ),
            ManifestError::ChecksumMismatch => {
                write!(f, "payload SHA-256 does not match the envelope header")
            }
            ManifestError::Payload { detail } => {
                write!(f, "manifest payload failed to deserialize: {}", detail)
            }
            ManifestError::UnsupportedSchemaVersion { found, supported } => {
                write!(
                    f,
                    "unsupported manifest schema version {} (this binary \
                     understands {})",
                    found, supported
                )
            }
            ManifestError::TooManySessions { count, cap } => write!(
                f,
                "manifest carries {} sessions, over the {} cap",
                count, cap
            ),
            ManifestError::NegativeFd { role, fd } => {
                write!(f, "{} is negative ({})", role, fd)
            }
            ManifestError::StdioRangeFd { role, fd } => write!(
                f,
                "{} is fd {} — in the stdio range 0-2, which the handoff \
                 never escrows",
                role, fd
            ),
            ManifestError::DuplicateFd { fd } => write!(
                f,
                "fd {} appears in more than one manifest slot",
                fd
            ),
            ManifestError::BadChildPid { uid, pid } => write!(
                f,
                "session '{}' has invalid child_pid {} (must be > 0)",
                uid, pid
            ),
            ManifestError::BadChildStartTime { uid } => write!(
                f,
                "session '{}' has child_start_time 0 — no real spawn-time \
                 capture produces that",
                uid
            ),
            ManifestError::FdRoleMismatch { role, fd, detail } => write!(
                f,
                "fd {} does not match its manifest role {}: {}",
                fd, role, detail
            ),
            ManifestError::Io { context, source } => {
                write!(f, "{}: {}", context, source)
            }
        }
    }
}

impl std::error::Error for ManifestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ManifestError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Write side (old image)
// ---------------------------------------------------------------------------

/// Serialize `manifest` into a fresh **sealed** memfd and return it.
///
/// The fd is created with `MFD_CLOEXEC | MFD_ALLOW_SEALING`; the
/// envelope (magic, format version, length, SHA-256, JSON payload)
/// is `pwrite`n at offset 0 — the fd's file offset is never moved —
/// and then all four seals are added, so from the moment this
/// returns, the bytes are immutable to everyone including us.
///
/// **The returned fd is CLOEXEC.** The future exec step (3b) clears
/// CLOEXEC on exactly the fds it hands off, immediately before the
/// exec, per the design's CLOEXEC-discipline step (R9). This module
/// never clears it — a manifest that leaks into an ordinary child
/// spawn must die at that child's exec.
///
/// Beyond the caps the design requires at write time (size, session
/// count), this also runs the full structural validation the reader
/// runs — the writer refuses to produce what the reader would
/// reject, so a coordinator bug (duplicate fd, stdio-range fd, bad
/// pid) fails the restart RPC while the old image is still in
/// charge, instead of surfacing as a rollback after the exec.
/// Structural failures come back as `InvalidInput` with the typed
/// [`ManifestError`] as the source.
pub fn write_manifest(manifest: &ReexecManifest) -> io::Result<OwnedFd> {
    validate_structure(manifest)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    let payload = serde_json::to_vec(manifest)?;
    let total = HEADER_LEN as u64 + payload.len() as u64;
    if total > MAX_MANIFEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            ManifestError::Oversize {
                size: total,
                cap: MAX_MANIFEST_BYTES,
            },
        ));
    }

    let bytes = encode_envelope(&payload);
    let fd = create_manifest_memfd()?;
    pwrite_all(fd.as_raw_fd(), &bytes)?;
    seal_memfd(fd.as_raw_fd())?;
    Ok(fd)
}

/// Build the binary envelope around a serialized payload:
/// magic (4) | format version u32 LE (4) | payload length u64 LE (8)
/// | SHA-256(payload) (32) | payload.
fn encode_envelope(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&MANIFEST_FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    out.extend_from_slice(&sha256(payload));
    out.extend_from_slice(payload);
    out
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// `memfd_create(2)` with CLOEXEC + sealing enabled. See
/// [`write_manifest`] for why CLOEXEC is deliberate and never
/// cleared here.
fn create_manifest_memfd() -> io::Result<OwnedFd> {
    let name = std::ffi::CString::new(MEMFD_NAME)
        .expect("MEMFD_NAME contains no NUL");
    // SAFETY: `name` is a valid NUL-terminated string for the call's
    // duration; the flags are a valid combination.
    let ret = unsafe {
        libc::memfd_create(
            name.as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: non-negative return is a fresh fd we own.
    Ok(unsafe { OwnedFd::from_raw_fd(ret) })
}

/// Positional write of the whole buffer at offset 0 — the fd's file
/// offset is deliberately never moved (see the module docs' offset
/// gotcha; the READER must be offset-independent regardless, and its
/// tests park the offset at EOF to prove it).
fn pwrite_all(fd: RawFd, bytes: &[u8]) -> io::Result<()> {
    let mut off = 0usize;
    while off < bytes.len() {
        // SAFETY: the pointer/length name a live sub-slice of
        // `bytes`; the fd is open for the call's duration.
        let ret = unsafe {
            libc::pwrite(
                fd,
                bytes[off..].as_ptr() as *const libc::c_void,
                bytes.len() - off,
                off as libc::off_t,
            )
        };
        if ret < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        off += ret as usize;
    }
    Ok(())
}

/// `fcntl(F_ADD_SEALS)` with all four required seals.
fn seal_memfd(fd: RawFd) -> io::Result<()> {
    // SAFETY: plain fcntl on an open fd with an int argument.
    let ret = unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, REQUIRED_SEALS) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Read side (new image)
// ---------------------------------------------------------------------------

/// Read and validate a manifest from an inherited fd.
///
/// Touches ONLY the manifest fd itself (and its `/proc/self/fd`
/// entry) — never any fd the manifest names — so a failure at any
/// step honors the corrupt-manifest rule (no escrow fd touched; see
/// the module docs). Validation order, all before the payload is
/// deserialized or believed:
///
/// 1. **The fd is a sealed memfd of ours**: `fstat` says regular
///    file, `fstatfs` says tmpfs (memfds live on the kernel's
///    internal shmem mount), `readlink("/proc/self/fd/N")` resolves
///    to exactly our memfd name (exact match — stricter than the
///    design's "starts with", so a memfd named
///    `cm-reexec-manifest-evil` can't ride the prefix), and
///    `fcntl(F_GET_SEALS)` reports all four seals.
/// 2. **Size within [`MAX_MANIFEST_BYTES`]** (and at least the
///    envelope header).
/// 3. **`pread` from offset 0** — NEVER `read`. The memfd's file
///    offset survives the exec and sits wherever the previous image
///    left it (typically EOF), so an ordinary `read` would return
///    empty on a perfectly intact manifest. This is review finding
///    R8's memfd-offset gotcha; see the module docs.
/// 4. **Envelope**: magic, format version, exact declared length,
///    SHA-256.
///
/// Then the payload is deserialized and structurally validated:
/// supported schema version, session count within [`MAX_SESSIONS`],
/// no negative / stdio-range / duplicate fd numbers anywhere, and
/// per-session `child_pid > 0` / `child_start_time > 0`.
///
/// What this deliberately does NOT do: probe the fds the manifest
/// names. That is [`validate_fd_roles`], a separate call the caller
/// sequences explicitly after this one succeeds.
pub fn read_manifest(
    fd: BorrowedFd<'_>,
) -> Result<ReexecManifest, ManifestError> {
    let raw = fd.as_raw_fd();

    // -- 1: sealed memfd of ours ------------------------------------
    let st = fstat_manifest(raw)?;
    if (st.st_mode & libc::S_IFMT) != libc::S_IFREG {
        return Err(ManifestError::NotMemfd {
            fd: raw,
            detail: format!(
                "not a regular file (st_mode {:#o})",
                st.st_mode
            ),
        });
    }
    // SAFETY: zeroed statfs is a valid out-buffer for the kernel to
    // overwrite; the fd is open for the call's duration.
    let mut sfs: libc::statfs = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::fstatfs(raw, &mut sfs) };
    if ret != 0 {
        return Err(ManifestError::Io {
            context: "fstatfs(manifest fd)",
            source: io::Error::last_os_error(),
        });
    }
    if sfs.f_type as i64 != libc::TMPFS_MAGIC as i64 {
        return Err(ManifestError::NotMemfd {
            fd: raw,
            detail: format!(
                "not on tmpfs (f_type {:#x}; memfds live on the kernel's \
                 shmem mount)",
                sfs.f_type
            ),
        });
    }
    let link = std::fs::read_link(format!("/proc/self/fd/{}", raw)).map_err(
        |e| ManifestError::Io {
            context: "readlink(/proc/self/fd/<manifest fd>)",
            source: e,
        },
    )?;
    // A live memfd's link is "/memfd:<name> (deleted)" (the inode is
    // unlinked by construction); accept the bare form too in case a
    // kernel ever drops the suffix. Exact-match both — see step 1's
    // doc note on why not starts_with.
    let expect = format!("/memfd:{}", MEMFD_NAME);
    let expect_deleted = format!("{} (deleted)", expect);
    let link_str = link.to_string_lossy();
    if link_str != expect && link_str != expect_deleted {
        return Err(ManifestError::NotMemfd {
            fd: raw,
            detail: format!(
                "/proc/self/fd link is {:?}, expected {:?}",
                link_str, expect_deleted
            ),
        });
    }
    // SAFETY: plain fcntl query on an open fd.
    let seals = unsafe { libc::fcntl(raw, libc::F_GET_SEALS) };
    if seals < 0 {
        // EINVAL here means "this file does not support sealing" —
        // definitionally not a memfd of ours.
        return Err(ManifestError::NotMemfd {
            fd: raw,
            detail: format!(
                "F_GET_SEALS failed: {} (file does not support sealing?)",
                io::Error::last_os_error()
            ),
        });
    }
    if seals & REQUIRED_SEALS != REQUIRED_SEALS {
        return Err(ManifestError::MissingSeals {
            fd: raw,
            have: seals,
            want: REQUIRED_SEALS,
        });
    }

    // -- 2: size ------------------------------------------------------
    let size = st.st_size as u64;
    if size > MAX_MANIFEST_BYTES {
        return Err(ManifestError::Oversize {
            size,
            cap: MAX_MANIFEST_BYTES,
        });
    }
    if size < HEADER_LEN as u64 {
        return Err(ManifestError::TooSmall {
            size,
            min: HEADER_LEN as u64,
        });
    }

    // -- 3: positional read (R8 — never the shared file offset) ------
    let bytes = pread_exact(raw, size as usize)?;

    // -- 4: envelope --------------------------------------------------
    let mut magic = [0u8; 4];
    magic.copy_from_slice(&bytes[0..4]);
    if magic != MAGIC {
        return Err(ManifestError::BadMagic { found: magic });
    }
    let format_version =
        u32::from_le_bytes(bytes[4..8].try_into().expect("4-byte slice"));
    if format_version != MANIFEST_FORMAT_VERSION {
        return Err(ManifestError::UnsupportedFormatVersion {
            found: format_version,
            supported: MANIFEST_FORMAT_VERSION,
        });
    }
    let declared =
        u64::from_le_bytes(bytes[8..16].try_into().expect("8-byte slice"));
    let available = size - HEADER_LEN as u64;
    if declared != available {
        return Err(ManifestError::BadLength {
            declared,
            available,
        });
    }
    let payload = &bytes[HEADER_LEN..];
    if sha256(payload) != bytes[16..48] {
        return Err(ManifestError::ChecksumMismatch);
    }

    // -- payload + structure ------------------------------------------
    let manifest: ReexecManifest = serde_json::from_slice(payload)
        .map_err(|e| ManifestError::Payload {
            detail: e.to_string(),
        })?;
    validate_structure(&manifest)?;
    Ok(manifest)
}

fn fstat_manifest(fd: RawFd) -> Result<libc::stat, ManifestError> {
    // SAFETY: zeroed stat is a valid out-buffer; the fd is open for
    // the call's duration.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::fstat(fd, &mut st) };
    if ret != 0 {
        return Err(ManifestError::Io {
            context: "fstat(manifest fd)",
            source: io::Error::last_os_error(),
        });
    }
    Ok(st)
}

/// Positional read of exactly `len` bytes from offset 0. Short reads
/// are retried at the advanced offset; an early EOF is an error (the
/// size came from `fstat` and SHRINK is sealed, so it cannot happen
/// on a validated manifest — surfacing it honestly beats guessing).
fn pread_exact(fd: RawFd, len: usize) -> Result<Vec<u8>, ManifestError> {
    let mut buf = vec![0u8; len];
    let mut off = 0usize;
    while off < len {
        // SAFETY: the pointer/length name a live sub-slice of `buf`;
        // the fd is open for the call's duration.
        let ret = unsafe {
            libc::pread(
                fd,
                buf[off..].as_mut_ptr() as *mut libc::c_void,
                len - off,
                off as libc::off_t,
            )
        };
        if ret < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(ManifestError::Io {
                context: "pread(manifest fd)",
                source: err,
            });
        }
        if ret == 0 {
            return Err(ManifestError::Io {
                context: "pread(manifest fd)",
                source: io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("EOF at {} of {} bytes", off, len),
                ),
            });
        }
        off += ret as usize;
    }
    Ok(buf)
}

/// Structural validation of a deserialized manifest — everything
/// that can be checked WITHOUT touching any fd the manifest names.
/// Run by [`read_manifest`] after the envelope verifies and by
/// [`write_manifest`] before serializing.
fn validate_structure(m: &ReexecManifest) -> Result<(), ManifestError> {
    if m.schema_version != MANIFEST_SCHEMA_VERSION {
        return Err(ManifestError::UnsupportedSchemaVersion {
            found: m.schema_version,
            supported: MANIFEST_SCHEMA_VERSION,
        });
    }
    if m.sessions.len() > MAX_SESSIONS {
        return Err(ManifestError::TooManySessions {
            count: m.sessions.len(),
            cap: MAX_SESSIONS,
        });
    }

    let mut seen: HashSet<RawFd> = HashSet::new();
    let mut check_fd = |role: &'static str,
                        fd: RawFd|
     -> Result<(), ManifestError> {
        if fd < 0 {
            return Err(ManifestError::NegativeFd { role, fd });
        }
        if fd <= 2 {
            return Err(ManifestError::StdioRangeFd { role, fd });
        }
        if !seen.insert(fd) {
            return Err(ManifestError::DuplicateFd { fd });
        }
        Ok(())
    };

    check_fd("rollback_bin_fd", m.rollback_bin_fd)?;
    check_fd("listener_fd", m.listener_fd)?;
    if let Some(fd) = m.tls_listener_fd {
        check_fd("tls_listener_fd", fd)?;
    }
    for s in &m.sessions {
        check_fd("pty_master_fd", s.pty_master_fd)?;
        check_fd("pidfd", s.pidfd)?;
        if s.child_pid <= 0 {
            return Err(ManifestError::BadChildPid {
                uid: s.uid.clone(),
                pid: s.child_pid,
            });
        }
        if s.child_start_time == 0 {
            return Err(ManifestError::BadChildStartTime {
                uid: s.uid.clone(),
            });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Per-role fd-type validation
// ---------------------------------------------------------------------------

/// Validate that every fd the manifest names IS the kind of kernel
/// object its role requires. **This is the first — and in this
/// module, the only — code that touches escrow fds**, which is why
/// it is a separate function from [`read_manifest`]: the design's
/// corrupt-manifest rule ("on parse failure, touch NO escrow fd")
/// requires the caller to sequence integrity/structure validation
/// and fd probing explicitly, and a combined function could not
/// honor that.
///
/// Per role:
/// - `pty_master_fd`: a character device that answers
///   `ioctl(TIOCGWINSZ)`.
/// - `pidfd`: `/proc/self/fd/N` readlinks to `anon_inode:[pidfd]`.
/// - `listener_fd` / `tls_listener_fd`: a socket with
///   `SO_ACCEPTCONN` true (bound AND listening — a connected stream
///   fails).
/// - `rollback_bin_fd`: a regular file with execute permission —
///   openable-for-exec SHAPE only (`fstat` mode bits). Nothing is
///   ever exec'd here, and a mode check cannot prove `execveat` will
///   succeed (noexec mounts etc.); it proves the slot plausibly
///   holds the pinned executable (R7) rather than an arbitrary fd.
///
/// Every probe is read-only and side-effect-free: `fstat`, a query
/// `ioctl`, `readlink`, `getsockopt`. Nothing is signaled, written,
/// closed, or dup'd — a role-validation failure leaves the escrow
/// exactly as it found it, so the caller's rollback path still holds
/// intact fds.
///
/// Precondition: the fd NUMBERS were already structurally validated
/// (a manifest from [`read_manifest`]). A stale/closed number fails
/// its probe with `EBADF`, reported as a role mismatch — honest, if
/// less specific.
pub fn validate_fd_roles(m: &ReexecManifest) -> Result<(), ManifestError> {
    validate_rollback_fd(m.rollback_bin_fd)?;
    validate_listener_fd("listener_fd", m.listener_fd)?;
    if let Some(fd) = m.tls_listener_fd {
        validate_listener_fd("tls_listener_fd", fd)?;
    }
    for s in &m.sessions {
        validate_pty_master_fd(s.pty_master_fd)?;
        validate_pidfd(s.pidfd)?;
    }
    Ok(())
}

fn role_fstat(
    role: &'static str,
    fd: RawFd,
) -> Result<libc::stat, ManifestError> {
    // SAFETY: zeroed stat is a valid out-buffer; fstat on a bad fd
    // fails with EBADF rather than faulting.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::fstat(fd, &mut st) };
    if ret != 0 {
        return Err(ManifestError::FdRoleMismatch {
            role,
            fd,
            detail: format!("fstat failed: {}", io::Error::last_os_error()),
        });
    }
    Ok(st)
}

/// PTY master: character device + answers `TIOCGWINSZ`.
fn validate_pty_master_fd(fd: RawFd) -> Result<(), ManifestError> {
    let role = "pty_master_fd";
    let st = role_fstat(role, fd)?;
    if (st.st_mode & libc::S_IFMT) != libc::S_IFCHR {
        return Err(ManifestError::FdRoleMismatch {
            role,
            fd,
            detail: format!(
                "not a character device (st_mode {:#o})",
                st.st_mode
            ),
        });
    }
    // SAFETY: zeroed winsize is a valid out-buffer for the query
    // ioctl; a non-tty fd fails with ENOTTY rather than faulting.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ as _, &mut ws) };
    if ret != 0 {
        return Err(ManifestError::FdRoleMismatch {
            role,
            fd,
            detail: format!(
                "TIOCGWINSZ refused: {} (char device but not a pty?)",
                io::Error::last_os_error()
            ),
        });
    }
    Ok(())
}

/// pidfd: the `/proc/self/fd` link of a pidfd is the anon-inode
/// marker `anon_inode:[pidfd]` — nothing else readlinks to that.
fn validate_pidfd(fd: RawFd) -> Result<(), ManifestError> {
    let role = "pidfd";
    let link =
        std::fs::read_link(format!("/proc/self/fd/{}", fd)).map_err(|e| {
            ManifestError::FdRoleMismatch {
                role,
                fd,
                detail: format!("readlink(/proc/self/fd/{}) failed: {}", fd, e),
            }
        })?;
    if link.to_string_lossy() != "anon_inode:[pidfd]" {
        return Err(ManifestError::FdRoleMismatch {
            role,
            fd,
            detail: format!(
                "/proc/self/fd link is {:?}, expected \"anon_inode:[pidfd]\"",
                link
            ),
        });
    }
    Ok(())
}

/// Listener: a socket for which `SO_ACCEPTCONN` is true — i.e. one
/// that `listen(2)` was called on. A connected stream answers false;
/// a non-socket fails the getsockopt with ENOTSOCK.
fn validate_listener_fd(
    role: &'static str,
    fd: RawFd,
) -> Result<(), ManifestError> {
    let mut val: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: valid out-pointer + length for a c_int option; a
    // non-socket fd fails with ENOTSOCK rather than faulting.
    let ret = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ACCEPTCONN,
            &mut val as *mut libc::c_int as *mut libc::c_void,
            &mut len,
        )
    };
    if ret != 0 {
        return Err(ManifestError::FdRoleMismatch {
            role,
            fd,
            detail: format!(
                "getsockopt(SO_ACCEPTCONN) failed: {} (not a socket?)",
                io::Error::last_os_error()
            ),
        });
    }
    if val == 0 {
        return Err(ManifestError::FdRoleMismatch {
            role,
            fd,
            detail: "socket is not listening (SO_ACCEPTCONN false — a \
                     connected stream, not a listener)"
                .to_string(),
        });
    }
    Ok(())
}

/// Rollback executable: regular file with an execute bit. Shape
/// only — see [`validate_fd_roles`]; the actual `execveat` (and its
/// new-inode ≠ rollback-inode assertion) belongs to 3b.
fn validate_rollback_fd(fd: RawFd) -> Result<(), ManifestError> {
    let role = "rollback_bin_fd";
    let st = role_fstat(role, fd)?;
    if (st.st_mode & libc::S_IFMT) != libc::S_IFREG {
        return Err(ManifestError::FdRoleMismatch {
            role,
            fd,
            detail: format!("not a regular file (st_mode {:#o})", st.st_mode),
        });
    }
    if st.st_mode & 0o111 == 0 {
        return Err(ManifestError::FdRoleMismatch {
            role,
            fd,
            detail: format!(
                "no execute permission (st_mode {:#o})",
                st.st_mode
            ),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Env bootstrap helpers
// ---------------------------------------------------------------------------

/// Peek at [`ENV_MANIFEST_FD`] without consuming it: parse the value
/// as a non-negative fd number, `None` for absent / empty /
/// non-numeric / negative (any character outside ASCII digits —
/// including sign characters and whitespace — disqualifies).
///
/// This is the non-mutating detection probe; the handoff path (3b)
/// uses it to decide "is this startup a handoff?" and then calls
/// [`consume_env`] exactly once for the value it acts on.
pub fn read_env_fd() -> Option<RawFd> {
    parse_fd(&std::env::var(ENV_MANIFEST_FD).ok()?)
}

/// Read AND clear [`ENV_MANIFEST_FD`] — the design's
/// consume-and-clear rule (R14): the manifest env, validated or not,
/// must never leak into children (every child inherits our environ,
/// exactly the propagation mechanics of the 2026-08-18 session-
/// identity leak — see `crate::env_sanitize`).
///
/// The var is removed whenever it is present, INCLUDING when its
/// value is garbage: a fresh daemon started from a leaked env that
/// sees the var but fails validation (of the value here, or of the
/// manifest downstream) must scrub it and boot as a normal fresh
/// start. Scrubbing at read time makes that the only possible path —
/// no caller can consume the value and forget the clear. (On a
/// validation failure DOWNSTREAM of a successful parse, there is
/// nothing left to scrub: the var is already gone by the time
/// `read_manifest` runs.)
///
/// Mutates the process environment, so — like
/// `env_sanitize::scrub_inherited_session_env` — it must run during
/// single-threaded startup, before anything spawns threads or
/// children. 3b's `run()` wiring calls it before `bind_socket` and
/// before `restore_sessions`.
pub fn consume_env() -> Option<RawFd> {
    let val = std::env::var_os(ENV_MANIFEST_FD)?;
    std::env::remove_var(ENV_MANIFEST_FD);
    parse_fd(val.to_str()?)
}

/// Strict fd-number parse: nonempty, ASCII digits only (no sign, no
/// whitespace, no hex), within `RawFd` range. Rejecting `-1` (and
/// every other non-canonical spelling) here means no caller ever
/// holds a "parsed" fd it can't `BorrowedFd::borrow_raw`.
fn parse_fd(s: &str) -> Option<RawFd> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<RawFd>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::env_lock;
    use std::io::Write;
    use std::os::fd::AsFd;

    /// A structurally valid manifest over FAKE fd numbers. Fine for
    /// everything except [`validate_fd_roles`] — `read_manifest` and
    /// `write_manifest` never touch the fds a manifest names (the
    /// corrupt-manifest rule), so the numbers only need to pass the
    /// structural checks (≥ 3, unique). Role validation against real
    /// fds lives in `daemon/tests/reexec_manifest_roles.rs`.
    fn sample_manifest() -> ReexecManifest {
        ReexecManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            attempt: 1,
            rollback_bin_fd: 10,
            sessions: vec![
                SessionRecord {
                    uid: "sess-a".into(),
                    generation: 3,
                    transcript_id: Some("abc-123".into()),
                    child_pid: 4242,
                    child_start_time: 987654321,
                    pty_master_fd: 20,
                    pidfd: 21,
                    cgroup_prefix: Some("cm-cap-sess-a".into()),
                    watcher_checkpoint: Some(serde_json::json!({
                        "protected": [4242],
                        "last_high": 2,
                    })),
                },
                SessionRecord {
                    uid: "sess-b".into(),
                    generation: 1,
                    transcript_id: None,
                    child_pid: 4300,
                    child_start_time: 987654999,
                    pty_master_fd: 22,
                    pidfd: 23,
                    cgroup_prefix: None,
                    watcher_checkpoint: None,
                },
            ],
            listener_fd: 11,
            tls_listener_fd: Some(12),
        }
    }

    /// Build a sealed manifest memfd from RAW envelope bytes —
    /// the test-side back door around `write_manifest`'s caps and
    /// structural validation, for proving the READER rejects what a
    /// buggy/hostile writer could produce.
    fn sealed_memfd_from(bytes: &[u8]) -> OwnedFd {
        let fd = create_manifest_memfd().expect("memfd_create");
        pwrite_all(fd.as_raw_fd(), bytes).expect("pwrite envelope");
        seal_memfd(fd.as_raw_fd()).expect("seal");
        fd
    }

    fn envelope_for(m: &ReexecManifest) -> Vec<u8> {
        encode_envelope(&serde_json::to_vec(m).expect("serialize"))
    }

    /// Full write → read round trip: every field survives intact.
    #[test]
    fn round_trip_write_read_equality() {
        let m = sample_manifest();
        let fd = write_manifest(&m).expect("write_manifest");
        let got = read_manifest(fd.as_fd()).expect("read_manifest");
        assert_eq!(got, m, "manifest must round-trip exactly");
    }

    /// The R8 offset gotcha, proven: `read_manifest` succeeds with
    /// the fd's file offset deliberately parked at EOF — the state a
    /// real handoff inherits (the offset survives the exec at
    /// whatever the previous image left it). An implementation that
    /// slipped back to ordinary `read()` would see zero bytes here
    /// and fail.
    #[test]
    fn read_is_offset_independent() {
        let m = sample_manifest();
        let fd = write_manifest(&m).expect("write_manifest");

        // Park the shared offset at EOF (write_manifest's pwrite
        // leaves it at 0, so this is an explicit simulation of the
        // post-exec worst case).
        // SAFETY: plain lseek on an open fd.
        let end = unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_END) };
        assert!(end > 0, "lseek(SEEK_END) failed");

        let got = read_manifest(fd.as_fd())
            .expect("read_manifest must not depend on the file offset");
        assert_eq!(got, m);

        // And again mid-file, for completeness.
        // SAFETY: plain lseek on an open fd.
        unsafe { libc::lseek(fd.as_raw_fd(), 7, libc::SEEK_SET) };
        assert_eq!(read_manifest(fd.as_fd()).expect("mid-file offset"), m);
    }

    /// The fd `write_manifest` returns is CLOEXEC (the future exec
    /// step clears the flag on exactly the handed-off fds — this
    /// module must never pre-clear it) and fully sealed (a
    /// post-seal write must be refused by the kernel).
    #[test]
    fn written_fd_is_cloexec_and_sealed() {
        let fd = write_manifest(&sample_manifest()).expect("write_manifest");

        // SAFETY: plain fcntl queries on an open fd.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0, "F_GETFD failed");
        assert_ne!(
            flags & libc::FD_CLOEXEC,
            0,
            "manifest fd must be CLOEXEC at creation"
        );

        let seals = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GET_SEALS) };
        assert_eq!(
            seals & REQUIRED_SEALS,
            REQUIRED_SEALS,
            "all four seals must be present (got {:#x})",
            seals
        );

        // The seals must actually bite: a write attempt fails.
        assert!(
            pwrite_all(fd.as_raw_fd(), b"tamper").is_err(),
            "write into a sealed memfd must be refused"
        );
    }

    /// An unsealed memfd — right name, right content, no seals — is
    /// rejected: writable means untrusted. Also covers the partial-
    /// seal case.
    #[test]
    fn rejects_unsealed_and_partially_sealed_memfd() {
        let bytes = envelope_for(&sample_manifest());

        let unsealed = create_manifest_memfd().expect("memfd");
        pwrite_all(unsealed.as_raw_fd(), &bytes).expect("pwrite");
        match read_manifest(unsealed.as_fd()) {
            Err(ManifestError::MissingSeals { have, .. }) => {
                assert_eq!(have & REQUIRED_SEALS, 0)
            }
            other => panic!("expected MissingSeals, got {:?}", other),
        }

        let partial = create_manifest_memfd().expect("memfd");
        pwrite_all(partial.as_raw_fd(), &bytes).expect("pwrite");
        // SAFETY: plain fcntl with an int argument.
        let ret = unsafe {
            libc::fcntl(
                partial.as_raw_fd(),
                libc::F_ADD_SEALS,
                libc::F_SEAL_SHRINK | libc::F_SEAL_GROW,
            )
        };
        assert_eq!(ret, 0, "partial seal failed");
        assert!(
            matches!(
                read_manifest(partial.as_fd()),
                Err(ManifestError::MissingSeals { .. })
            ),
            "SHRINK+GROW without WRITE+SEAL must still be refused"
        );
    }

    /// A regular file carrying byte-identical envelope content is
    /// rejected on identity, not content: it isn't a memfd of ours
    /// (its /proc link is a real path — and on a non-tmpfs /tmp, the
    /// fstatfs probe refuses even earlier; either way: NotMemfd).
    #[test]
    fn rejects_regular_file_masquerade() {
        let bytes = envelope_for(&sample_manifest());
        let mut f = tempfile::tempfile().expect("tempfile");
        f.write_all(&bytes).expect("write envelope to regular file");
        let fd: OwnedFd = f.into();
        match read_manifest(fd.as_fd()) {
            Err(ManifestError::NotMemfd { .. }) => {}
            other => panic!("expected NotMemfd, got {:?}", other),
        }
    }

    /// Each envelope field is independently load-bearing: corrupt
    /// one (via the pre-seal byte image), get the matching typed
    /// rejection.
    #[test]
    fn rejects_each_corrupted_envelope_field() {
        let good = envelope_for(&sample_manifest());

        // Magic.
        let mut b = good.clone();
        b[0] ^= 0xFF;
        match read_manifest(sealed_memfd_from(&b).as_fd()) {
            Err(ManifestError::BadMagic { found }) => {
                assert_ne!(found, MAGIC)
            }
            other => panic!("expected BadMagic, got {:?}", other),
        }

        // Format version.
        let mut b = good.clone();
        b[4..8].copy_from_slice(&999u32.to_le_bytes());
        match read_manifest(sealed_memfd_from(&b).as_fd()) {
            Err(ManifestError::UnsupportedFormatVersion {
                found: 999, ..
            }) => {}
            other => {
                panic!("expected UnsupportedFormatVersion, got {:?}", other)
            }
        }

        // Declared length (off by one vs. the actual byte count).
        let mut b = good.clone();
        let declared =
            u64::from_le_bytes(b[8..16].try_into().unwrap()) + 1;
        b[8..16].copy_from_slice(&declared.to_le_bytes());
        match read_manifest(sealed_memfd_from(&b).as_fd()) {
            Err(ManifestError::BadLength {
                declared: d,
                available,
            }) => assert_eq!(d, available + 1),
            other => panic!("expected BadLength, got {:?}", other),
        }

        // Checksum field flipped.
        let mut b = good.clone();
        b[16] ^= 0xFF;
        assert!(matches!(
            read_manifest(sealed_memfd_from(&b).as_fd()),
            Err(ManifestError::ChecksumMismatch)
        ));

        // Payload byte flipped (checksum catches it before any
        // JSON parsing sees the mangled byte).
        let mut b = good.clone();
        b[HEADER_LEN] ^= 0xFF;
        assert!(matches!(
            read_manifest(sealed_memfd_from(&b).as_fd()),
            Err(ManifestError::ChecksumMismatch)
        ));
    }

    /// A truncated file (declared length > available bytes) is a
    /// BadLength, and a sub-header file is TooSmall.
    #[test]
    fn rejects_truncated_payload() {
        let good = envelope_for(&sample_manifest());

        let cut = &good[..good.len() - 7];
        match read_manifest(sealed_memfd_from(cut).as_fd()) {
            Err(ManifestError::BadLength {
                declared,
                available,
            }) => assert_eq!(declared, available + 7),
            other => panic!("expected BadLength, got {:?}", other),
        }

        assert!(matches!(
            read_manifest(sealed_memfd_from(&good[..HEADER_LEN - 1]).as_fd()),
            Err(ManifestError::TooSmall { .. })
        ));
    }

    /// Checksum-valid JSON that isn't the manifest shape fails as a
    /// Payload error (after the envelope, before any structural
    /// checks).
    #[test]
    fn rejects_wrong_shape_payload() {
        let b = encode_envelope(br#"{"not": "a manifest"}"#);
        assert!(matches!(
            read_manifest(sealed_memfd_from(&b).as_fd()),
            Err(ManifestError::Payload { .. })
        ));
    }

    /// Duplicate fd numbers are rejected across ALL slots — session
    /// fds against each other and against listener/rollback fds —
    /// on both the read side (via the raw back door) and the write
    /// side.
    #[test]
    fn rejects_duplicate_fds() {
        // Session pty fd colliding with the rollback fd.
        let mut m = sample_manifest();
        m.sessions[1].pty_master_fd = m.rollback_bin_fd;
        match read_manifest(sealed_memfd_from(&envelope_for(&m)).as_fd()) {
            Err(ManifestError::DuplicateFd { fd }) => {
                assert_eq!(fd, m.rollback_bin_fd)
            }
            other => panic!("expected DuplicateFd, got {:?}", other),
        }
        let werr = write_manifest(&m).expect_err("write must refuse too");
        assert_eq!(werr.kind(), io::ErrorKind::InvalidInput);

        // TLS listener colliding with the unix listener.
        let mut m = sample_manifest();
        m.tls_listener_fd = Some(m.listener_fd);
        assert!(matches!(
            read_manifest(sealed_memfd_from(&envelope_for(&m)).as_fd()),
            Err(ManifestError::DuplicateFd { .. })
        ));
    }

    /// Stdio-range (0-2) and negative fds are rejected with their
    /// role named.
    #[test]
    fn rejects_stdio_range_and_negative_fds() {
        let mut m = sample_manifest();
        m.sessions[0].pty_master_fd = 1;
        match read_manifest(sealed_memfd_from(&envelope_for(&m)).as_fd()) {
            Err(ManifestError::StdioRangeFd {
                role: "pty_master_fd",
                fd: 1,
            }) => {}
            other => panic!("expected StdioRangeFd, got {:?}", other),
        }

        let mut m = sample_manifest();
        m.listener_fd = 0;
        assert!(matches!(
            read_manifest(sealed_memfd_from(&envelope_for(&m)).as_fd()),
            Err(ManifestError::StdioRangeFd {
                role: "listener_fd",
                fd: 0
            })
        ));

        let mut m = sample_manifest();
        m.rollback_bin_fd = -1;
        assert!(matches!(
            read_manifest(sealed_memfd_from(&envelope_for(&m)).as_fd()),
            Err(ManifestError::NegativeFd {
                role: "rollback_bin_fd",
                fd: -1
            })
        ));
        assert_eq!(
            write_manifest(&m).expect_err("write refuses too").kind(),
            io::ErrorKind::InvalidInput
        );
    }

    /// Bad per-session identity values: pid ≤ 0, start_time == 0.
    #[test]
    fn rejects_bad_child_pid_and_start_time() {
        let mut m = sample_manifest();
        m.sessions[0].child_pid = 0;
        assert!(matches!(
            read_manifest(sealed_memfd_from(&envelope_for(&m)).as_fd()),
            Err(ManifestError::BadChildPid { pid: 0, .. })
        ));

        let mut m = sample_manifest();
        m.sessions[1].child_pid = -4;
        assert!(matches!(
            read_manifest(sealed_memfd_from(&envelope_for(&m)).as_fd()),
            Err(ManifestError::BadChildPid { pid: -4, .. })
        ));

        let mut m = sample_manifest();
        m.sessions[0].child_start_time = 0;
        assert!(matches!(
            read_manifest(sealed_memfd_from(&envelope_for(&m)).as_fd()),
            Err(ManifestError::BadChildStartTime { .. })
        ));
    }

    /// Unknown schema versions are refused before any field is
    /// believed.
    #[test]
    fn rejects_unsupported_schema_version() {
        let mut m = sample_manifest();
        m.schema_version = MANIFEST_SCHEMA_VERSION + 1;
        match read_manifest(sealed_memfd_from(&envelope_for(&m)).as_fd()) {
            Err(ManifestError::UnsupportedSchemaVersion { found, .. }) => {
                assert_eq!(found, MANIFEST_SCHEMA_VERSION + 1)
            }
            other => {
                panic!("expected UnsupportedSchemaVersion, got {:?}", other)
            }
        }
        assert_eq!(
            write_manifest(&m).expect_err("write refuses too").kind(),
            io::ErrorKind::InvalidInput
        );
    }

    /// Size cap: the reader refuses an over-cap file before reading
    /// a byte of it; the writer refuses to produce one.
    #[test]
    fn rejects_oversize() {
        // Read side: a sealed memfd one byte over the cap (content
        // irrelevant — the size gate fires before the envelope).
        let big = vec![0u8; MAX_MANIFEST_BYTES as usize + 1];
        match read_manifest(sealed_memfd_from(&big).as_fd()) {
            Err(ManifestError::Oversize { size, cap }) => {
                assert_eq!(size, MAX_MANIFEST_BYTES + 1);
                assert_eq!(cap, MAX_MANIFEST_BYTES);
            }
            other => panic!("expected Oversize, got {:?}", other),
        }

        // Write side: a payload that can't fit under the cap.
        let mut m = sample_manifest();
        m.sessions[0].transcript_id =
            Some("x".repeat(MAX_MANIFEST_BYTES as usize));
        assert_eq!(
            write_manifest(&m).expect_err("oversize write").kind(),
            io::ErrorKind::InvalidInput
        );
    }

    /// Session-count cap on both sides. The 4097-session payload is
    /// well under the byte cap, so this exercises the count check
    /// specifically.
    #[test]
    fn rejects_session_count_overflow() {
        let mut m = sample_manifest();
        m.sessions = (0..(MAX_SESSIONS as i32 + 1))
            .map(|i| SessionRecord {
                uid: format!("s{}", i),
                generation: 0,
                transcript_id: None,
                child_pid: 100 + i,
                child_start_time: 1,
                pty_master_fd: 100 + 2 * i,
                pidfd: 101 + 2 * i,
                cgroup_prefix: None,
                watcher_checkpoint: None,
            })
            .collect();

        match read_manifest(sealed_memfd_from(&envelope_for(&m)).as_fd()) {
            Err(ManifestError::TooManySessions { count, cap }) => {
                assert_eq!(count, MAX_SESSIONS + 1);
                assert_eq!(cap, MAX_SESSIONS);
            }
            other => panic!("expected TooManySessions, got {:?}", other),
        }
        assert_eq!(
            write_manifest(&m).expect_err("write refuses too").kind(),
            io::ErrorKind::InvalidInput
        );
    }

    /// Env parse: reject "", "-1", "abc", padded, signed, and
    /// overflowing values; accept canonical digit strings.
    #[test]
    fn env_fd_parse_is_strict() {
        let _g = env_lock();
        struct EnvGuard {
            prev: Option<std::ffi::OsString>,
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match self.prev.take() {
                    Some(v) => std::env::set_var(ENV_MANIFEST_FD, v),
                    None => std::env::remove_var(ENV_MANIFEST_FD),
                }
            }
        }
        let _restore = EnvGuard {
            prev: std::env::var_os(ENV_MANIFEST_FD),
        };

        std::env::remove_var(ENV_MANIFEST_FD);
        assert_eq!(read_env_fd(), None, "absent var");

        for bad in ["", "-1", "abc", " 5", "5 ", "+5", "0x10", "4294967296"] {
            std::env::set_var(ENV_MANIFEST_FD, bad);
            assert_eq!(
                read_env_fd(),
                None,
                "{:?} must be rejected as an fd number",
                bad
            );
        }

        std::env::set_var(ENV_MANIFEST_FD, "17");
        assert_eq!(read_env_fd(), Some(17));
        // read_env_fd is a peek — the var survives it.
        assert!(std::env::var_os(ENV_MANIFEST_FD).is_some());
    }

    /// consume_env reads AND clears — including when the value is
    /// garbage (the Aug-18 leaked-env pattern: scrub no matter
    /// what, so a fresh start never bequeaths the var to children).
    #[test]
    fn consume_env_reads_and_always_clears() {
        let _g = env_lock();
        struct EnvGuard {
            prev: Option<std::ffi::OsString>,
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match self.prev.take() {
                    Some(v) => std::env::set_var(ENV_MANIFEST_FD, v),
                    None => std::env::remove_var(ENV_MANIFEST_FD),
                }
            }
        }
        let _restore = EnvGuard {
            prev: std::env::var_os(ENV_MANIFEST_FD),
        };

        // Valid value: returned, and gone afterwards.
        std::env::set_var(ENV_MANIFEST_FD, "23");
        assert_eq!(consume_env(), Some(23));
        assert!(
            std::env::var_os(ENV_MANIFEST_FD).is_none(),
            "consume_env must clear the var"
        );

        // Absent: None, nothing to clear.
        assert_eq!(consume_env(), None);

        // Garbage value: None, AND scrubbed anyway.
        std::env::set_var(ENV_MANIFEST_FD, "not-an-fd");
        assert_eq!(consume_env(), None);
        assert!(
            std::env::var_os(ENV_MANIFEST_FD).is_none(),
            "a garbage value must still be scrubbed (leaked-env pattern)"
        );
    }
}
