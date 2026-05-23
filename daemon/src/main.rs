//! `cm-daemon` binary entry point.
//!
//! Thin shim around [`cm_daemon::run`] so the launchable binary and the
//! testable surface stay separated. Phase 1 of doc/persistent-host-daemon.md.
//!
//! ## Non-Linux build shape
//!
//! Slice 10d watcher-fix #5: the `lib.rs` crate-level gate makes
//! `cm_daemon` an empty crate on non-Linux, which would make this
//! bin's `main` fail to resolve `cm_daemon::run`. Pre-fix-#5 we
//! attempted `#![cfg(target_os = "linux")]` at the bin's crate
//! root, but Cargo still treats `main.rs` as a bin target and the
//! crate-level cfg removed `main` entirely → "main function not
//! found" on non-Linux.
//!
//! Right shape: dual `main` definitions selected by an outer cfg
//! attribute on the function itself. The non-Linux stub eprintln-
//! exits, satisfying Cargo's "bin must have a main" requirement
//! while still failing cleanly when an operator tries to run the
//! binary on a non-Linux host.

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    cm_daemon::run()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!(
        "cm-daemon is Linux-only (requires pidfd, cgroup v2, \
         systemd-run, /proc). Compile + run on a Linux host."
    );
    std::process::exit(1);
}
