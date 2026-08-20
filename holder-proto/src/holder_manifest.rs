//! The holder-upgrade manifest — the third sealed-manifest variant
//! (DESIGN_HOLDER_BRAIN_SPLIT § Holder upgrades, V3/V4).
//!
//! A holder upgrades by **re-exec'ing itself** with a sealed FD
//! manifest, exactly the DESIGN_SEAMLESS_RESTART mechanism applied to
//! a process whose entire state IS the manifest's natural content:
//! per-session records (canonical master + pidfd + reap/delivery
//! flags + the pending exit-event queue), custodied listeners, the
//! live brain's pid/pidfd/socketpair end, both brain pins, breaker
//! counters, and — correctness-bearing (V3) — the **incarnation
//! high-water mark**: a post-upgrade holder must never re-mint a
//! previously-issued `(uid, incarnation)`, because the brain's
//! exit-idempotency keys depend on it.
//!
//! Same trust model and mechanics as `reexec_manifest` (sealed memfd,
//! envelope with magic/format/length/SHA-256, positional IO, env var
//! as a bootstrap pointer only), with a DISTINCT magic, memfd name,
//! and env var so the two manifest kinds can never be presented as
//! each other. The corrupt-manifest rule applies unchanged: a holder
//! image that cannot validate this manifest touches no escrow fd and
//! boots fresh — which for a holder is the crash-class residual the
//! design names (tiny image, a few times a year).
//!
//! The brain survives the holder's exec (children survive a parent's
//! exec); the new holder image rebuilds from this manifest and opens
//! with an unsolicited `rehello` over the manifest-carried socketpair
//! end — the one exception to brain-sends-first.

use std::collections::HashSet;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::channel::{ExitEventBody, LastSignalRequest, ListenerMeta};

/// Bootstrap pointer for the holder self-exec — the manifest fd
/// number, nothing else. Distinct from `CM_REEXEC_MANIFEST_FD` so a
/// daemon manifest can never be consumed as a holder manifest or
/// vice versa.
pub const ENV_HOLDER_MANIFEST_FD: &str = "CM_HOLDER_UPGRADE_MANIFEST_FD";

/// Envelope format version (framing).
pub const HOLDER_MANIFEST_FORMAT_VERSION: u32 = 1;

/// JSON payload schema version. Singleton until a second holder
/// vintage ships (the reexec_manifest v1/v2 discipline).
pub const HOLDER_MANIFEST_SCHEMA_VERSION: u32 = 1;

/// Size cap, both sides. Holder records are small (no logic meta),
/// but the opaque `rollback_record` blobs ride here too when a
/// reverse migration is staged across an upgrade — same cap as the
/// daemon manifest keeps the arithmetic shared.
pub const MAX_HOLDER_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;

/// Session-record cap, matching `reexec_manifest::MAX_SESSIONS`.
pub const MAX_HOLDER_SESSIONS: usize = 4096;

const MEMFD_NAME: &str = "cm-holder-upgrade-manifest";

/// "CM Holder Upgrade".
const MAGIC: [u8; 4] = *b"CMHU";

/// magic (4) + format version u32 LE (4) + payload length u64 LE (8)
/// + SHA-256 (32).
const HEADER_LEN: usize = 4 + 4 + 8 + 32;

const REQUIRED_SEALS: libc::c_int = libc::F_SEAL_SHRINK
    | libc::F_SEAL_GROW
    | libc::F_SEAL_WRITE
    | libc::F_SEAL_SEAL;

/// The holder's full state enumeration (design § The holder), as a
/// manifest. All `*_fd` fields are inherited-slot NUMBERS.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HolderUpgradeManifest {
    pub schema_version: u32,
    /// The holder's brain-spawn counter — the new image continues
    /// it (the running brain's generation must keep its name).
    pub epoch: u64,
    /// V3: the incarnation high-water mark — the next incarnation
    /// the new image may mint. MUST be strictly greater than every
    /// incarnation ever issued by this holder lineage.
    pub next_incarnation: u64,
    /// Breaker carry-over: consecutive failures without a stability
    /// reset. (Timers restart — the stability horizon re-anchors at
    /// the upgrade, which only ever DELAYS a reset, never loses a
    /// strike.)
    pub breaker_consecutive_failures: u32,
    /// The `--brain` path (HELD_DOWN's re-pin target).
    pub brain_path: String,
    /// The live brain, when one is running (a holder may upgrade
    /// while HELD_DOWN — then `None`, and the new image resumes the
    /// path-retry loop).
    pub brain: Option<BrainRuntime>,
    /// The current + previous pinned brain binaries.
    pub brain_pin_fd: Option<RawFd>,
    pub brain_pin_previous_fd: Option<RawFd>,
    pub sessions: Vec<HolderSessionRecord>,
    pub listeners: Vec<ListenerRecord>,
}

/// The running brain's supervision handles.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrainRuntime {
    pub pid: i32,
    pub pidfd: RawFd,
    /// The holder's socketpair end — the same open file description
    /// the brain is connected to; the rehello rides it.
    pub channel_fd: RawFd,
}

/// One holder-resident session record — the holder's own state, no
/// logic meta (frozenness: nothing here needs interpretation).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HolderSessionRecord {
    pub uid: String,
    pub incarnation: u64,
    pub generation_meta: u64,
    pub child_pid: i32,
    pub child_start_time: u64,
    pub master_fd: RawFd,
    pub pidfd: RawFd,
    pub reap_armed: bool,
    /// Per-brain-generation delivery gate. Carried because the SAME
    /// brain generation continues across a holder upgrade — resetting
    /// it would strand exit events until a brain restart.
    pub delivery_ready: bool,
    /// V7's latch: exit-ready observed while un-armed (masked out of
    /// the poll set, zombie parked).
    pub exit_latched: bool,
    pub cgroup_prefix: Option<String>,
    pub cgroup_path: Option<String>,
    pub watcher_checkpoint: Option<serde_json::Value>,
    pub last_signal_request: Option<LastSignalRequest>,
    /// A consumed-but-unforgotten exit: the tombstone-side timestamp
    /// plus the event awaiting `ack_exit` (None once acked).
    pub exit: Option<ExitCarry>,
    /// Reverse-migration blob staged via `rollback_record`, riding an
    /// upgrade unparsed (C1's opacity holds here too).
    pub rollback_record: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExitCarry {
    pub exited_at: f64,
    pub pending_event: Option<ExitEventBody>,
}

/// One custodied listener.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListenerRecord {
    pub kind: String,
    pub meta: ListenerMeta,
    pub fd: RawFd,
}

/// Typed failure — mirrors `reexec_manifest::ManifestError`'s shape
/// with this manifest's identity checks.
#[derive(Debug)]
pub enum HolderManifestError {
    NotMemfd { fd: RawFd, detail: String },
    MissingSeals { fd: RawFd },
    Oversize { size: u64, cap: u64 },
    TooSmall { size: u64 },
    BadMagic,
    UnsupportedFormatVersion { found: u32 },
    BadLength { declared: u64, available: u64 },
    ChecksumMismatch,
    Payload { detail: String },
    UnsupportedSchemaVersion { found: u32 },
    Structure { detail: String },
    Io { context: &'static str, source: io::Error },
}

impl std::fmt::Display for HolderManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use HolderManifestError::*;
        match self {
            NotMemfd { fd, detail } => {
                write!(f, "fd {fd} is not a sealed holder-upgrade memfd: {detail}")
            }
            MissingSeals { fd } => {
                write!(f, "holder-upgrade memfd {fd} is missing required seals")
            }
            Oversize { size, cap } => {
                write!(f, "holder manifest is {size} bytes, over the {cap} cap")
            }
            TooSmall { size } => {
                write!(f, "holder manifest is {size} bytes, under the header")
            }
            BadMagic => write!(f, "bad holder-manifest magic"),
            UnsupportedFormatVersion { found } => {
                write!(f, "unsupported holder-manifest format version {found}")
            }
            BadLength { declared, available } => write!(
                f,
                "declared payload length {declared} != available {available}"
            ),
            ChecksumMismatch => write!(f, "holder-manifest checksum mismatch"),
            Payload { detail } => {
                write!(f, "holder-manifest payload failed to parse: {detail}")
            }
            UnsupportedSchemaVersion { found } => {
                write!(f, "unsupported holder-manifest schema version {found}")
            }
            Structure { detail } => {
                write!(f, "holder-manifest structure invalid: {detail}")
            }
            Io { context, source } => write!(f, "{context}: {source}"),
        }
    }
}

impl std::error::Error for HolderManifestError {}

/// Serialize into a fresh sealed memfd (CLOEXEC — the exec path
/// clears the flag on exactly the fds it hands off).
pub fn write_holder_manifest(
    m: &HolderUpgradeManifest,
) -> io::Result<OwnedFd> {
    validate_structure(m)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let payload = serde_json::to_vec(m)?;
    let total = HEADER_LEN as u64 + payload.len() as u64;
    if total > MAX_HOLDER_MANIFEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            HolderManifestError::Oversize {
                size: total,
                cap: MAX_HOLDER_MANIFEST_BYTES,
            },
        ));
    }
    let mut bytes = Vec::with_capacity(HEADER_LEN + payload.len());
    bytes.extend_from_slice(&MAGIC);
    bytes.extend_from_slice(&HOLDER_MANIFEST_FORMAT_VERSION.to_le_bytes());
    bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&sha256(&payload));
    bytes.extend_from_slice(&payload);

    let name = std::ffi::CString::new(MEMFD_NAME).expect("no NUL");
    // SAFETY: valid NUL-terminated name; valid flag combination.
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
    let fd = unsafe { OwnedFd::from_raw_fd(ret) };
    pwrite_all(fd.as_raw_fd(), &bytes)?;
    // SAFETY: plain fcntl on an open fd.
    let ret = unsafe {
        libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, REQUIRED_SEALS)
    };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// Read + validate from an inherited fd. Touches ONLY the manifest
/// fd (identity/seals/envelope) — never a named escrow fd — per the
/// corrupt-manifest rule.
pub fn read_holder_manifest(
    fd: BorrowedFd<'_>,
) -> Result<HolderUpgradeManifest, HolderManifestError> {
    let raw = fd.as_raw_fd();
    // SAFETY: zeroed stat is a valid out-buffer.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(raw, &mut st) } != 0 {
        return Err(HolderManifestError::Io {
            context: "fstat(holder manifest fd)",
            source: io::Error::last_os_error(),
        });
    }
    if (st.st_mode & libc::S_IFMT) != libc::S_IFREG {
        return Err(HolderManifestError::NotMemfd {
            fd: raw,
            detail: format!("not a regular file (st_mode {:#o})", st.st_mode),
        });
    }
    let link = std::fs::read_link(format!("/proc/self/fd/{raw}")).map_err(
        |e| HolderManifestError::Io {
            context: "readlink(/proc/self/fd/<holder manifest fd>)",
            source: e,
        },
    )?;
    let expect = format!("/memfd:{MEMFD_NAME}");
    let expect_deleted = format!("{expect} (deleted)");
    let link_str = link.to_string_lossy();
    if link_str != expect && link_str != expect_deleted {
        return Err(HolderManifestError::NotMemfd {
            fd: raw,
            detail: format!("/proc link is {link_str:?}, expected {expect_deleted:?}"),
        });
    }
    // SAFETY: plain fcntl query.
    let seals = unsafe { libc::fcntl(raw, libc::F_GET_SEALS) };
    if seals < 0 || seals & REQUIRED_SEALS != REQUIRED_SEALS {
        return Err(HolderManifestError::MissingSeals { fd: raw });
    }
    let size = st.st_size as u64;
    if size > MAX_HOLDER_MANIFEST_BYTES {
        return Err(HolderManifestError::Oversize {
            size,
            cap: MAX_HOLDER_MANIFEST_BYTES,
        });
    }
    if size < HEADER_LEN as u64 {
        return Err(HolderManifestError::TooSmall { size });
    }
    let bytes = pread_exact(raw, size as usize)?;
    if bytes[0..4] != MAGIC {
        return Err(HolderManifestError::BadMagic);
    }
    let format = u32::from_le_bytes(bytes[4..8].try_into().expect("4 bytes"));
    if format != HOLDER_MANIFEST_FORMAT_VERSION {
        return Err(HolderManifestError::UnsupportedFormatVersion {
            found: format,
        });
    }
    let declared =
        u64::from_le_bytes(bytes[8..16].try_into().expect("8 bytes"));
    let available = size - HEADER_LEN as u64;
    if declared != available {
        return Err(HolderManifestError::BadLength {
            declared,
            available,
        });
    }
    let payload = &bytes[HEADER_LEN..];
    if sha256(payload) != bytes[16..48] {
        return Err(HolderManifestError::ChecksumMismatch);
    }
    let m: HolderUpgradeManifest = serde_json::from_slice(payload)
        .map_err(|e| HolderManifestError::Payload {
            detail: e.to_string(),
        })?;
    validate_structure(&m)?;
    Ok(m)
}

/// Per-role kernel-object validation for every fd the holder-upgrade
/// manifest names (review F9): sealing prevents post-write mutation,
/// not a writer bug that put the wrong fd number in a slot. Probes
/// are the shared read-only ones from `reexec_manifest` — the same
/// no-side-effect contract, and the FIRST escrow-touching call, kept
/// separate from [`read_holder_manifest`] so the caller sequences the
/// trust boundary explicitly.
pub fn validate_holder_fd_roles(
    m: &HolderUpgradeManifest,
) -> Result<(), HolderManifestError> {
    use crate::reexec_manifest as probes;
    let wrap = |r: Result<(), crate::reexec_manifest::ManifestError>| {
        r.map_err(|e| HolderManifestError::Structure {
            detail: e.to_string(),
        })
    };
    if let Some(b) = &m.brain {
        wrap(probes::validate_pidfd_role("brain.pidfd", b.pidfd))?;
        wrap(probes::validate_channel_fd("brain.channel_fd", b.channel_fd))?;
    }
    if let Some(fd) = m.brain_pin_fd {
        wrap(probes::validate_exec_fd("brain_pin_fd", fd))?;
    }
    if let Some(fd) = m.brain_pin_previous_fd {
        wrap(probes::validate_exec_fd("brain_pin_previous_fd", fd))?;
    }
    for s in &m.sessions {
        wrap(probes::validate_pty_master_fd(s.master_fd))?;
        wrap(probes::validate_pidfd_role("pidfd", s.pidfd))?;
    }
    for l in &m.listeners {
        wrap(probes::validate_listener_fd("listener_fd", l.fd))?;
    }
    Ok(())
}

fn validate_structure(
    m: &HolderUpgradeManifest,
) -> Result<(), HolderManifestError> {
    if m.schema_version != HOLDER_MANIFEST_SCHEMA_VERSION {
        return Err(HolderManifestError::UnsupportedSchemaVersion {
            found: m.schema_version,
        });
    }
    if m.sessions.len() > MAX_HOLDER_SESSIONS {
        return Err(HolderManifestError::Structure {
            detail: format!(
                "{} sessions over the {} cap",
                m.sessions.len(),
                MAX_HOLDER_SESSIONS
            ),
        });
    }
    let mut seen: HashSet<RawFd> = HashSet::new();
    let mut check_fd =
        |what: &str, fd: RawFd| -> Result<(), HolderManifestError> {
            if fd <= 2 {
                return Err(HolderManifestError::Structure {
                    detail: format!("{what} fd {fd} in the stdio range / negative"),
                });
            }
            if !seen.insert(fd) {
                return Err(HolderManifestError::Structure {
                    detail: format!("fd {fd} appears in more than one slot"),
                });
            }
            Ok(())
        };
    if let Some(b) = &m.brain {
        if b.pid <= 0 {
            return Err(HolderManifestError::Structure {
                detail: format!("brain pid {} invalid", b.pid),
            });
        }
        check_fd("brain.pidfd", b.pidfd)?;
        check_fd("brain.channel_fd", b.channel_fd)?;
    }
    if let Some(fd) = m.brain_pin_fd {
        check_fd("brain_pin_fd", fd)?;
    }
    if let Some(fd) = m.brain_pin_previous_fd {
        check_fd("brain_pin_previous_fd", fd)?;
    }
    for s in &m.sessions {
        check_fd("session master_fd", s.master_fd)?;
        check_fd("session pidfd", s.pidfd)?;
        if s.child_pid <= 0 || s.child_start_time == 0 {
            return Err(HolderManifestError::Structure {
                detail: format!(
                    "session '{}' has invalid pid/starttime",
                    s.uid
                ),
            });
        }
        if s.incarnation >= m.next_incarnation {
            return Err(HolderManifestError::Structure {
                detail: format!(
                    "session '{}' incarnation {} >= next_incarnation {} — \
                     the high-water mark must exceed every issued \
                     incarnation (V3)",
                    s.uid, s.incarnation, m.next_incarnation
                ),
            });
        }
    }
    for l in &m.listeners {
        check_fd("listener fd", l.fd)?;
    }
    Ok(())
}

/// Read AND clear [`ENV_HOLDER_MANIFEST_FD`] — the consume-and-clear
/// rule (R14): scrubbed whenever present, garbage included, so the
/// var can never leak into a brain or session child. Single-threaded
/// startup only.
pub fn consume_env() -> Option<RawFd> {
    let val = std::env::var_os(ENV_HOLDER_MANIFEST_FD)?;
    std::env::remove_var(ENV_HOLDER_MANIFEST_FD);
    let s = val.to_str()?;
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<RawFd>().ok()
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

fn pwrite_all(fd: RawFd, bytes: &[u8]) -> io::Result<()> {
    let mut off = 0usize;
    while off < bytes.len() {
        // SAFETY: pointer/length name a live sub-slice; fd is open.
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

fn pread_exact(fd: RawFd, len: usize) -> Result<Vec<u8>, HolderManifestError> {
    let mut buf = vec![0u8; len];
    let mut off = 0usize;
    while off < len {
        // SAFETY: pointer/length name a live sub-slice; fd is open.
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
            return Err(HolderManifestError::Io {
                context: "pread(holder manifest fd)",
                source: err,
            });
        }
        if ret == 0 {
            return Err(HolderManifestError::Io {
                context: "pread(holder manifest fd)",
                source: io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("EOF at {off} of {len}"),
                ),
            });
        }
        off += ret as usize;
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    fn sample() -> HolderUpgradeManifest {
        HolderUpgradeManifest {
            schema_version: HOLDER_MANIFEST_SCHEMA_VERSION,
            epoch: 3,
            next_incarnation: 12,
            breaker_consecutive_failures: 1,
            brain_path: "/opt/cm-daemon/cm-daemon".into(),
            brain: Some(BrainRuntime {
                pid: 4242,
                pidfd: 10,
                channel_fd: 11,
            }),
            brain_pin_fd: Some(12),
            brain_pin_previous_fd: None,
            sessions: vec![HolderSessionRecord {
                uid: "ts-1".into(),
                incarnation: 7,
                generation_meta: 2,
                child_pid: 999,
                child_start_time: 123456,
                master_fd: 20,
                pidfd: 21,
                reap_armed: true,
                delivery_ready: true,
                exit_latched: false,
                cgroup_prefix: None,
                cgroup_path: Some("/sys/fs/cgroup/x".into()),
                watcher_checkpoint: None,
                last_signal_request: None,
                exit: None,
                rollback_record: None,
            }],
            listeners: vec![],
        }
    }

    #[test]
    fn round_trips_through_a_sealed_memfd() {
        let m = sample();
        let fd = write_holder_manifest(&m).expect("write");
        let back = read_holder_manifest(fd.as_fd()).expect("read");
        assert_eq!(back, m);
    }

    #[test]
    fn incarnation_high_water_must_exceed_every_issued_incarnation() {
        let mut m = sample();
        m.next_incarnation = 7; // == the session's incarnation
        match write_holder_manifest(&m) {
            Err(e) => assert_eq!(e.kind(), io::ErrorKind::InvalidInput),
            Ok(_) => panic!("high-water violation must refuse"),
        }
    }

    #[test]
    fn duplicate_fds_are_refused() {
        let mut m = sample();
        m.brain_pin_fd = Some(20); // collides with the session master
        assert!(write_holder_manifest(&m).is_err());
    }

    #[test]
    fn a_daemon_manifest_is_not_a_holder_manifest() {
        // Wrong memfd name + magic: a sealed reexec manifest fd must
        // be refused by identity, not by content guesswork.
        let daemon = crate::reexec_manifest::ReexecManifest {
            schema_version: crate::reexec_manifest::MANIFEST_SCHEMA_VERSION,
            attempt: 0,
            reexec_generation: 1,
            rollback_bin_fd: 10,
            sessions: vec![],
            listener_fd: 11,
            tls_listener_fd: None,
            rollback_schema_version: None,
            split: None,
        };
        let fd = crate::reexec_manifest::write_manifest(&daemon)
            .expect("daemon manifest writes");
        match read_holder_manifest(fd.as_fd()) {
            Err(HolderManifestError::NotMemfd { .. }) => {}
            other => panic!("expected NotMemfd, got {other:?}"),
        }
    }

    #[test]
    fn consume_env_scrubs_unconditionally() {
        let _guard = crate::test_support::env_lock();
        std::env::set_var(ENV_HOLDER_MANIFEST_FD, "garbage");
        assert_eq!(consume_env(), None);
        assert!(std::env::var_os(ENV_HOLDER_MANIFEST_FD).is_none());
        std::env::set_var(ENV_HOLDER_MANIFEST_FD, "17");
        assert_eq!(consume_env(), Some(17));
        assert!(std::env::var_os(ENV_HOLDER_MANIFEST_FD).is_none());
    }
}
