//! Non-killing session-adoption primitives. Restart hardening /
//! DESIGN_SEAMLESS_RESTART phase 2c (review finding R5 + the
//! portable-pty adoption gap).
//!
//! ## Why this exists
//!
//! The future re-exec handoff rebuilds every live session in the new
//! daemon image from inherited FDs (the design's Rehydrate step — "a
//! transaction against escrowed FDs"). Two things make today's types
//! unusable for that transaction:
//!
//! 1. **`DaemonSession`'s `Drop` SIGKILLs its child** (via pidfd —
//!    see `session.rs::impl Drop for DaemonSession`). Rehydrate
//!    builds sessions one record at a time; if record N+1 fails
//!    validation and the partially-built registry unwinds, normal
//!    Drop semantics would SIGKILL the already-verified children
//!    1..N — the rollback path itself killing the sessions it exists
//!    to protect (R5). Escrowed records therefore ride in
//!    [`SessionCandidate`], a holder whose drop closes its own fds
//!    and NEVER signals, waits on, or otherwise touches the child.
//!
//! 2. **portable-pty has no adoption constructor.** Its
//!    `UnixMasterPty` is private and only reachable through a fresh
//!    `openpty()`, so a new image cannot rebuild the
//!    `Box<dyn portable_pty::MasterPty + Send>` a `DaemonSession`
//!    holds from an inherited raw master fd at all.
//!    [`AdoptedMasterPty`] implements the trait honestly over an
//!    `OwnedFd`, so a future adopted session holds its master
//!    through the SAME trait-object field with zero changes to
//!    `DaemonSession`'s shape.
//!
//! ## The contract
//!
//! - Constructing a candidate ([`SessionCandidate::from_raw_parts`])
//!   performs NO validation and NO side effects. Validation belongs
//!   to the rehydrate transaction, which runs against the
//!   non-consuming probes: [`SessionCandidate::child_alive`] (pidfd
//!   `poll(2)` with a zero timeout — the same technique as
//!   `session.rs::poll_pidfd_until_exit_ready`, never `waitid`, so
//!   an exited child's status stays reconstructible) and
//!   [`SessionCandidate::child_start_time`] (`/proc/<pid>/stat`
//!   field 22, the design's R6 identity cross-check).
//! - [`SessionCandidate::promote`] is the only way out of escrow: it
//!   consumes the candidate and hands out the raw parts for real
//!   session construction. Promotion is the moment kill-on-drop
//!   semantics MAY begin — actually wiring the parts into a
//!   `DaemonSession` is a later phase (the design's commit gate),
//!   deliberately not this module's business. [`PromotedSessionParts`]
//!   itself is still inert: plain fds, no signaling drop.
//! - **Dropping a candidate closes the candidate's own fds and
//!   touches nothing else. It never signals, never waits, never
//!   reaps.** This is THE property of the type — it is what makes a
//!   failed rehydrate unwind safe for children 1..N. The guarantee
//!   is structural, not behavioral: every field is a `String`,
//!   `pid_t`, or `OwnedFd` (whose drop is `close(2)` only), and the
//!   type has no `Drop` impl — nor can one be added by accident,
//!   because `promote` moves fields out of `self`, which the
//!   compiler forbids for types with a custom `Drop`.
//!
//! ## Scope
//!
//! Self-contained: no exec, no manifest, no handoff wiring, and no
//! changes to `DaemonSession`. Behavioral tests against real
//! children on manually-opened ptys live in
//! `daemon/tests/adopt_candidate.rs` (own process, per repo
//! convention for child-spawning suites).

use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use portable_pty::{MasterPty, PtySize};

/// A [`portable_pty::MasterPty`] implementation over a raw inherited
/// PTY master fd. DESIGN_SEAMLESS_RESTART phase 2c (the portable-pty
/// adoption gap): portable-pty's own `UnixMasterPty` cannot be built
/// from an existing fd, so this type re-implements the trait's unix
/// surface directly over the `OwnedFd` — each method an honest
/// syscall against the adopted fd, mirroring the upstream
/// implementation's semantics so an adopted session behaves like a
/// spawned one.
///
/// Two deliberate deviations from upstream `UnixMasterPty`, both
/// safe for daemon usage (`DaemonSession` calls exactly
/// `try_clone_reader` once, `take_writer` once, and `resize`; it
/// never relies on the deviating behaviors):
///
/// - **The taken writer's `Drop` does NOT inject VEOF.** Upstream's
///   `UnixMasterWriter::drop` writes `\n` + the termios VEOF byte
///   into the pty so "dropping the writer sends EOF to the slave".
///   For an *adopted* fd that would mean an unwind path injecting a
///   Ctrl-D into a live inherited child's terminal — an interactive
///   shell exits on that, i.e. a kill-by-input on exactly the paths
///   this module exists to keep side-effect-free. The daemon never
///   uses writer-drop-EOF semantics (its `SessionWriter` lives as
///   long as the session and teardown is pidfd-SIGKILL-based), so
///   the adopted writer's drop just closes its dup silently.
/// - **`tty_name` is computed on demand via `ioctl(TIOCGPTN)`.**
///   Upstream caches `ttyname_r(slave_fd)` at `openpty` time; an
///   adopted master has no slave fd to ask. On Linux (this crate is
///   `#![cfg(target_os = "linux")]`) the pty multiplexor recovers
///   the same `/dev/pts/<N>` path from the master. `None` on any
///   failure, matching the trait's "if applicable" contract.
pub struct AdoptedMasterPty {
    fd: OwnedFd,
    /// `take_writer` is documented upstream as valid at most once;
    /// mirror that with an atomic (the trait object rides behind
    /// `Send` and the daemon touches it from multiple threads).
    took_writer: AtomicBool,
}

impl AdoptedMasterPty {
    /// Adopt `fd` as a PTY master. Performs no syscalls and no
    /// validation — an fd that is not really a pty master simply
    /// yields honest `ioctl` errors from the trait methods later.
    /// Pre-promotion FD-type validation (`fstat` role checks) is the
    /// rehydrate transaction's job, per the design's manifest
    /// validation step, not this constructor's.
    pub fn from_master_fd(fd: OwnedFd) -> Self {
        Self {
            fd,
            took_writer: AtomicBool::new(false),
        }
    }

    /// Dup the adopted fd for a reader/writer handle. Uses
    /// `OwnedFd::try_clone` (`F_DUPFD_CLOEXEC`), so handle dups are
    /// close-on-exec — same hygiene as upstream's
    /// `FileDescriptor::try_clone`, and required here: a future
    /// re-exec must never leak derived handles, only the one
    /// canonical manifest-listed master fd.
    fn dup_file(&self) -> std::io::Result<std::fs::File> {
        Ok(std::fs::File::from(self.fd.try_clone()?))
    }
}

/// Reader half dup'd off an [`AdoptedMasterPty`]. Mirrors upstream
/// `PtyFd`'s `Read`: a master read returning `EIO` means the slave
/// side closed, which is the pty's EOF shape — map it to `Ok(0)` so
/// the daemon's reader thread observes the same teardown signal
/// (`Ok(0)` → fanout close) on an adopted session as on a spawned
/// one.
struct AdoptedPtyReader {
    file: std::fs::File,
}

impl Read for AdoptedPtyReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.file.read(buf) {
            Err(ref e) if e.raw_os_error() == Some(libc::EIO) => Ok(0),
            x => x,
        }
    }
}

/// Writer half dup'd off an [`AdoptedMasterPty`]. Deliberately has
/// NO `Drop` impl: dropping it closes the dup and injects nothing
/// into the child's terminal — see the VEOF deviation note on
/// [`AdoptedMasterPty`].
struct AdoptedPtyWriter {
    file: std::fs::File,
}

impl Write for AdoptedPtyWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.file.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

impl MasterPty for AdoptedMasterPty {
    /// `ioctl(TIOCSWINSZ)` on the adopted fd — the kernel updates
    /// the pty's winsize and raises SIGWINCH in the child, exactly
    /// as for a spawned master.
    fn resize(&self, size: PtySize) -> Result<(), anyhow::Error> {
        let ws = libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: size.pixel_width,
            ws_ypixel: size.pixel_height,
        };
        // SAFETY: TIOCSWINSZ reads a valid `winsize` we own for the
        // duration of the call; the fd is owned and open.
        let ret = unsafe {
            libc::ioctl(self.fd.as_raw_fd(), libc::TIOCSWINSZ as _, &ws)
        };
        if ret != 0 {
            anyhow::bail!(
                "failed to ioctl(TIOCSWINSZ) on adopted pty master: {:?}",
                std::io::Error::last_os_error()
            );
        }
        Ok(())
    }

    /// `ioctl(TIOCGWINSZ)` on the adopted fd.
    fn get_size(&self) -> Result<PtySize, anyhow::Error> {
        // SAFETY: zeroed `winsize` is a valid all-fields-zero value
        // for the kernel to overwrite.
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: TIOCGWINSZ writes a `winsize` into memory we own;
        // the fd is owned and open.
        let ret = unsafe {
            libc::ioctl(self.fd.as_raw_fd(), libc::TIOCGWINSZ as _, &mut ws)
        };
        if ret != 0 {
            anyhow::bail!(
                "failed to ioctl(TIOCGWINSZ) on adopted pty master: {:?}",
                std::io::Error::last_os_error()
            );
        }
        Ok(PtySize {
            rows: ws.ws_row,
            cols: ws.ws_col,
            pixel_width: ws.ws_xpixel,
            pixel_height: ws.ws_ypixel,
        })
    }

    /// Dup → EIO-mapping reader (see [`AdoptedPtyReader`]).
    /// Clonable any number of times, like upstream.
    fn try_clone_reader(&self) -> Result<Box<dyn Read + Send>, anyhow::Error> {
        Ok(Box::new(AdoptedPtyReader {
            file: self.dup_file()?,
        }))
    }

    /// Dup → writer, valid at most once (upstream contract; second
    /// take errors with upstream's message so callers treating the
    /// string as a discriminator see no difference).
    fn take_writer(&self) -> Result<Box<dyn Write + Send>, anyhow::Error> {
        if self.took_writer.swap(true, Ordering::SeqCst) {
            anyhow::bail!("cannot take writer more than once");
        }
        Ok(Box::new(AdoptedPtyWriter {
            file: self.dup_file()?,
        }))
    }

    /// Foreground process group of the adopted pty via
    /// `ioctl(TIOCGPGRP)` (what `tcgetpgrp(3)` wraps — upstream uses
    /// the wrapper, we ask the kernel directly). `None` when the pty
    /// has no foreground process group or the ioctl fails.
    fn process_group_leader(&self) -> Option<libc::pid_t> {
        let mut pgrp: libc::pid_t = 0;
        // SAFETY: TIOCGPGRP writes a `pid_t` into memory we own; the
        // fd is owned and open.
        let ret = unsafe {
            libc::ioctl(self.fd.as_raw_fd(), libc::TIOCGPGRP as _, &mut pgrp)
        };
        if ret == 0 && pgrp > 0 {
            Some(pgrp)
        } else {
            None
        }
    }

    fn as_raw_fd(&self) -> Option<RawFd> {
        Some(self.fd.as_raw_fd())
    }

    /// Slave device path recovered from the master via
    /// `ioctl(TIOCGPTN)` — see the deviation note on the type.
    fn tty_name(&self) -> Option<PathBuf> {
        let mut n: libc::c_uint = 0;
        // SAFETY: TIOCGPTN writes a `c_uint` (the pts index) into
        // memory we own; the fd is owned and open.
        let ret = unsafe {
            libc::ioctl(self.fd.as_raw_fd(), libc::TIOCGPTN as _, &mut n)
        };
        if ret == 0 {
            Some(PathBuf::from(format!("/dev/pts/{}", n)))
        } else {
            None
        }
    }

    /// `tcgetattr` on the adopted fd, mirroring upstream (this is
    /// why the daemon's `nix` version must track portable-pty's —
    /// see the dep comment in `daemon/Cargo.toml`).
    fn get_termios(&self) -> Option<nix::sys::termios::Termios> {
        nix::sys::termios::tcgetattr(self.fd.as_fd()).ok()
    }
}

/// Non-killing escrow holder for one would-be-adopted session:
/// the identity (`uid`, `pid`) plus the two inherited kernel handles
/// (spawn-time pidfd, canonical PTY master fd) a rehydrate record
/// carries. DESIGN_SEAMLESS_RESTART phase 2c (R5).
///
/// Richer identity metadata (generation, transcript_id,
/// cgroup_prefix, …) belongs to the manifest schema — a later phase.
/// This type deliberately carries only what its probes and promotion
/// need.
///
/// **THE property: dropping a `SessionCandidate` closes its own two
/// fds and NEVER signals, waits on, reaps, or otherwise touches the
/// child.** A failed rehydrate that unwinds a `Vec<SessionCandidate>`
/// therefore leaves every child running and every exit status
/// unconsumed — the exact opposite of what unwinding a partial
/// registry of real `DaemonSession`s would do (their `Drop` SIGKILLs
/// via pidfd). The guarantee is structural: see the module docs.
///
/// Note the escrow topology from the design: candidates are built
/// from CLOEXEC *dups* of the escrowed FDs, so a candidate's
/// fd-close on drop releases only its own references — the escrow
/// originals (and the pty they keep alive) are unaffected. That
/// wiring is the rehydrate phase's job; this type just guarantees
/// the drop itself is inert.
pub struct SessionCandidate {
    uid: String,
    pid: libc::pid_t,
    pidfd: OwnedFd,
    master_fd: OwnedFd,
}

impl SessionCandidate {
    /// Build a candidate from raw inherited parts. Performs NO
    /// validation and NO side effects — no probe, no syscall, no
    /// disk touch. A candidate for a long-dead pid is legal to
    /// construct; the rehydrate transaction discovers that through
    /// the probes and decides (tombstone honestly / abort), never
    /// this constructor.
    pub fn from_raw_parts(
        uid: impl Into<String>,
        pid: libc::pid_t,
        pidfd: OwnedFd,
        master_fd: OwnedFd,
    ) -> Self {
        Self {
            uid: uid.into(),
            pid,
            pidfd,
            master_fd,
        }
    }

    pub fn uid(&self) -> &str {
        &self.uid
    }

    pub fn pid(&self) -> libc::pid_t {
        self.pid
    }

    /// Borrow the candidate's pidfd for validation the probes don't
    /// cover (e.g. the rehydrate transaction's `fstat` FD-type
    /// checks). Borrowing keeps the non-killing ownership story
    /// intact — the fd still closes with the candidate.
    pub fn pidfd(&self) -> &OwnedFd {
        &self.pidfd
    }

    /// Borrow the candidate's PTY master fd (same rationale as
    /// [`Self::pidfd`]).
    pub fn master_fd(&self) -> &OwnedFd {
        &self.master_fd
    }

    /// Non-consuming liveness probe: `poll(2)` the pidfd with a zero
    /// timeout (the same readiness technique as
    /// `session.rs::poll_pidfd_until_exit_ready`, minus the waiting).
    ///
    /// - `Ok(true)` — the child has not exited.
    /// - `Ok(false)` — the child has exited (`POLLIN` on a pidfd is
    ///   exit-readiness). Its wait status is NOT consumed — the
    ///   child parks as a kernel zombie, fully reconstructible by
    ///   whoever holds reap rights after the commit gate
    ///   (`waitid(P_PIDFD)` under the reap permit, per phase 2a).
    /// - `Err(_)` — the poll itself failed or the fd is broken
    ///   (`POLLERR`/`POLLNVAL`); surfaced honestly so the rehydrate
    ///   transaction can abort on it rather than guess.
    ///
    /// Repeatable: calling this any number of times, in any state,
    /// consumes nothing and signals nothing.
    pub fn child_alive(&self) -> std::io::Result<bool> {
        loop {
            let mut pfd = libc::pollfd {
                fd: self.pidfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: `pfd` is a valid pollfd we own; nfds=1 matches;
            // a zero timeout returns immediately.
            let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
            if ret == 0 {
                // No exit-readiness event pending — still running.
                return Ok(true);
            }
            if ret > 0 {
                if pfd.revents & libc::POLLIN != 0 {
                    // Exited (possibly an unreaped zombie).
                    return Ok(false);
                }
                // POLLERR/POLLNVAL: the fd itself is broken — a
                // program bug or a corrupt manifest entry. Never
                // guess liveness off a broken handle.
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!(
                        "pidfd poll for pid {} reported revents {:#x} \
                         without POLLIN — broken pidfd",
                        self.pid, pfd.revents
                    ),
                ));
            }
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
    }

    /// Non-consuming identity probe: the child's kernel start time
    /// (`/proc/<pid>/stat` field 22, in clock ticks since boot).
    ///
    /// This is the design's R6 cross-check value: the manifest
    /// records it at spawn time, and rehydrate compares this read
    /// against the recorded one. It is read by NUMERIC pid, so on
    /// its own it is pid-reuse-racy — that is exactly why it is a
    /// cross-check paired with the handed-off pidfd, never an
    /// identity source alone (a reused pid yields a *different*
    /// start time, which is the mismatch the check exists to catch).
    ///
    /// Errors: `NotFound`-shaped when `/proc/<pid>` is gone (child
    /// exited AND was reaped), `InvalidData` when the stat line
    /// doesn't parse.
    pub fn child_start_time(&self) -> std::io::Result<u64> {
        proc_starttime(self.pid)
    }

    /// Leave escrow: consume the candidate and hand out the parts
    /// for real session construction. **Promotion is the moment
    /// kill-on-drop semantics MAY begin** — the returned parts are
    /// themselves still inert (see [`PromotedSessionParts`]), but
    /// the caller is now free to arm them into a `DaemonSession`,
    /// whose `Drop` SIGKILLs. That wiring is a later phase (the
    /// commit gate); nothing in this module performs it.
    ///
    /// Destructures `self` by move — which is also the structural
    /// guard that keeps this type `Drop`-impl-free forever (the
    /// compiler rejects moving fields out of a type with a custom
    /// `Drop`).
    pub fn promote(self) -> PromotedSessionParts {
        let SessionCandidate {
            uid,
            pid,
            pidfd,
            master_fd,
        } = self;
        PromotedSessionParts {
            uid,
            pid,
            pidfd,
            master: AdoptedMasterPty::from_master_fd(master_fd),
        }
    }
}

/// The parts [`SessionCandidate::promote`] hands out for real
/// session construction: identity, the spawn-time pidfd, and the
/// master fd wrapped as an [`AdoptedMasterPty`] (box it and it slots
/// into `DaemonSession`'s existing
/// `Box<dyn portable_pty::MasterPty + Send>` field unchanged).
///
/// Still non-killing by itself — dropping this closes fds and
/// nothing more. Kill-on-drop begins only when a later phase feeds
/// these parts into an armed `DaemonSession`. The type exists so the
/// commit gate can hold fully-validated, ready-to-arm parts without
/// having armed them yet.
pub struct PromotedSessionParts {
    pub uid: String,
    pub pid: libc::pid_t,
    pub pidfd: OwnedFd,
    pub master: AdoptedMasterPty,
}

/// Read a live process's kernel start time (`/proc/<pid>/stat`
/// field 22, clock ticks since boot) — the design's R6 pid-reuse
/// cross-check value. Shared by [`SessionCandidate::child_start_time`]
/// (the rehydrate-side probe) and the re-exec manifest's write side
/// (`crate::reexec`, DESIGN_SEAMLESS_RESTART phase 3b), so the value
/// the old image records and the value the new image re-reads come
/// from the SAME parser — a divergence there would fail every
/// cross-check silently.
///
/// Errors: `NotFound`-shaped when `/proc/<pid>` is gone (process
/// exited AND was reaped; a zombie still has a stat line),
/// `InvalidData` when the line doesn't parse.
pub(crate) fn proc_starttime(pid: libc::pid_t) -> std::io::Result<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid))?;
    parse_proc_stat_starttime(&stat).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unparseable /proc/{}/stat line: {:?}", pid, stat),
        )
    })
}

/// Parse the `starttime` field (field 22) out of a
/// `/proc/<pid>/stat` line.
///
/// The trap this exists to not fall into: field 2 (`comm`) is an
/// arbitrary, UNESCAPED string that may itself contain spaces and
/// `)` — e.g. a process named `sh) 0 0 0`. Naive
/// whitespace-splitting of the whole line miscounts fields for such
/// names. The kernel guarantees `comm` is the only non-numeric,
/// parenthesized field, so splitting after the LAST `)` is the
/// canonical correct parse; the remainder starts at field 3
/// (`state`), putting `starttime` at index 19 of the
/// whitespace-split remainder.
fn parse_proc_stat_starttime(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::FromRawFd;

    /// Open a fresh pty pair via `openpty(3)`. Unit-test helper —
    /// no child is ever spawned in this module's tests (child-
    /// spawning behavioral tests live in
    /// `daemon/tests/adopt_candidate.rs`, own process).
    fn openpty_raw() -> (OwnedFd, OwnedFd) {
        let mut master: libc::c_int = -1;
        let mut slave: libc::c_int = -1;
        // SAFETY: valid out-pointers for the two fds; name/termios/
        // winsize are all optional (null).
        let ret = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        assert_eq!(
            ret,
            0,
            "openpty failed: {:?}",
            std::io::Error::last_os_error()
        );
        // SAFETY: openpty succeeded, so both are fresh fds we own.
        (unsafe { OwnedFd::from_raw_fd(master) }, unsafe {
            OwnedFd::from_raw_fd(slave)
        })
    }

    /// resize → get_size round-trips through the kernel winsize on
    /// an adopted master (no child needed — the winsize lives on the
    /// pty, not the process).
    #[test]
    fn adopted_master_resize_get_size_round_trip() {
        let (master, _slave) = openpty_raw();
        let adopted = AdoptedMasterPty::from_master_fd(master);
        let size = PtySize {
            rows: 37,
            cols: 111,
            pixel_width: 0,
            pixel_height: 0,
        };
        adopted.resize(size).expect("TIOCSWINSZ on adopted master");
        let got = adopted.get_size().expect("TIOCGWINSZ on adopted master");
        assert_eq!(got, size, "kernel winsize must round-trip");
    }

    /// The upstream once-only writer contract is preserved: the
    /// second `take_writer` errors, the first (and any number of
    /// readers) succeed.
    #[test]
    fn take_writer_only_once_readers_unlimited() {
        let (master, _slave) = openpty_raw();
        let adopted = AdoptedMasterPty::from_master_fd(master);
        let _w = adopted.take_writer().expect("first take_writer");
        let second = adopted.take_writer();
        assert!(
            second.is_err(),
            "second take_writer must refuse (upstream contract)"
        );
        let _r1 = adopted.try_clone_reader().expect("reader 1");
        let _r2 = adopted.try_clone_reader().expect("reader 2");
    }

    /// `tty_name` recovers the slave device path from the master via
    /// TIOCGPTN — cross-checked against `ttyname_r` on the actual
    /// slave fd, which is the value upstream would have cached.
    #[test]
    fn tty_name_matches_slave_ttyname() {
        let (master, slave) = openpty_raw();
        let adopted = AdoptedMasterPty::from_master_fd(master);
        let name = adopted.tty_name().expect("tty_name on adopted master");
        assert!(
            name.to_string_lossy().starts_with("/dev/pts/"),
            "expected a /dev/pts path, got {:?}",
            name
        );

        let mut buf = [0i8 as libc::c_char; 128];
        // SAFETY: valid fd, valid buffer + length.
        let ret = unsafe {
            libc::ttyname_r(slave.as_raw_fd(), buf.as_mut_ptr(), buf.len())
        };
        assert_eq!(ret, 0, "ttyname_r(slave) failed");
        // SAFETY: ttyname_r succeeded, so buf holds a NUL-terminated
        // C string.
        let expected = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            name.to_string_lossy(),
            expected,
            "TIOCGPTN-derived name must match ttyname_r(slave)"
        );
    }

    /// The remaining honest-over-the-fd methods answer on a fresh
    /// adopted master: `as_raw_fd` reports the adopted fd,
    /// `get_termios` reads real termios, and `process_group_leader`
    /// is `None` (nothing has ever been foreground on this pty).
    #[test]
    fn fd_termios_and_pgrp_probes_answer() {
        let (master, _slave) = openpty_raw();
        let raw = master.as_raw_fd();
        let adopted = AdoptedMasterPty::from_master_fd(master);
        assert_eq!(MasterPty::as_raw_fd(&adopted), Some(raw));
        assert!(
            adopted.get_termios().is_some(),
            "tcgetattr must answer on a real pty master"
        );
        assert_eq!(
            adopted.process_group_leader(),
            None,
            "no foreground pgrp exists on a childless pty"
        );
    }

    /// Field-22 parse: the documented comm-with-spaces-and-parens
    /// trap. `comm` here is `sh) 0 0 0` — a naive whole-line split
    /// would land on the wrong field; splitting after the LAST `)`
    /// must not.
    #[test]
    fn starttime_parse_survives_hostile_comm() {
        // Remainder after the last ')' carries fields 3..: state,
        // ppid, pgrp, session, tty_nr, tpgid, flags, minflt,
        // cminflt, majflt, cmajflt, utime, stime, cutime, cstime,
        // priority, nice, num_threads, itrealvalue, starttime, …
        let line = "42 (sh) 0 0 0) S 1 42 42 0 -1 4194304 100 0 0 0 \
                    5 3 0 0 20 0 1 0 987654321 12345 67";
        assert_eq!(parse_proc_stat_starttime(line), Some(987654321));
    }

    /// Malformed stat lines yield `None` (surfaced as `InvalidData`
    /// by the probe), never a wrong number.
    #[test]
    fn starttime_parse_rejects_malformed() {
        // No closing paren at all.
        assert_eq!(parse_proc_stat_starttime("42 (sh"), None);
        // Too few fields after the comm.
        assert_eq!(parse_proc_stat_starttime("42 (sh) S 1 2 3"), None);
        // Non-numeric where starttime should be.
        let line = "42 (sh) S 1 42 42 0 -1 4194304 100 0 0 0 \
                    5 3 0 0 20 0 1 0 not-a-number 12345";
        assert_eq!(parse_proc_stat_starttime(line), None);
    }

    /// A real read of our own `/proc/self/stat` parses and is
    /// nonzero — the shape check against a line the kernel actually
    /// wrote.
    #[test]
    fn starttime_parse_reads_real_proc_stat() {
        let stat = std::fs::read_to_string("/proc/self/stat")
            .expect("read /proc/self/stat");
        let t = parse_proc_stat_starttime(&stat)
            .expect("parse own stat line");
        assert!(t > 0, "own starttime must be nonzero");
    }
}
