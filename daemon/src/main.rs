//! `cm-daemon` binary entry point.
//!
//! Thin shim around [`cm_daemon::run`] so the launchable binary and the
//! testable surface stay separated. Phase 1 of doc/persistent-host-daemon.md.

fn main() -> anyhow::Result<()> {
    cm_daemon::run()
}
