//! Carry each live session's PTY-output replay ring across a brain
//! restart (and the monolith re-exec swap).
//!
//! The ring ([`crate::session::PtyByteFanout`]) is what a reattaching
//! client gets replayed as its first Data frame — it IS the screen a
//! TUI paints after `attach.open`. The ring is brain-resident, so
//! pre-fix a planned brain restart (`daemon.restart` → `restart_brain`)
//! rode every session through with an EMPTY ring in the new
//! generation: the TUI's attach stream blipped, auto-reattached, and
//! rebuilt its terminal from a zero-byte replay — a blank pane that
//! stayed blank until the child happened to repaint (a full-screen
//! agent at its prompt never does), and the only recovery was `A-R`,
//! which restarts the agent.
//!
//! Mechanism: the restart's checked-persistence step (already under
//! the reader-gate freeze, so no reader is mid-push) snapshots every
//! live session's ring to `<sessions-file-dir>/daemon-rings/<uid>.bin`;
//! the next generation's adopt loop TAKES each file (read + unlink)
//! and seeds the freshly built session's ring with it before the
//! reader thread exists, so the replay order is
//! `pre-restart tail ++ kernel-buffered bytes drained by the new
//! reader ++ live`. Whatever is left in the directory after adoption
//! (sessions that were not adopted) is swept.
//!
//! Best-effort by design: a ring that fails to write or read costs
//! one session its replay (the pre-fix behaviour), never the deploy.
//! Take-semantics (unlink on read) mean a stale ring is never replayed
//! into a later generation — a crash-class brain death has no
//! persistence step, so it must not find a week-old snapshot lying
//! around and paint it as the current screen.

use std::path::{Path, PathBuf};

use crate::state::DaemonState;

/// Directory name, beside `daemon-sessions.json`.
pub const RINGS_DIR_NAME: &str = "daemon-rings";

/// The rings directory for a given sessions-registry path. Deriving
/// from the registry path (rather than `$HOME`) keeps a custom-pathed
/// test hermetic, the same way the tombstone sidecar does.
pub fn rings_dir_for(sessions_path: &Path) -> PathBuf {
    sessions_path.with_file_name(RINGS_DIR_NAME)
}

/// The production rings directory (`~/.cm/daemon-rings`).
pub fn default_rings_dir() -> PathBuf {
    rings_dir_for(&crate::state::default_daemon_sessions_path())
}

fn ring_path(dir: &Path, uid: &str) -> PathBuf {
    dir.join(format!("{uid}.bin"))
}

/// Snapshot every live session's ring into `dir`, replacing whatever
/// the directory held (a previous generation's leftovers are stale by
/// definition). Returns the number of rings written; per-file
/// failures are logged and skipped. Only a failure to create the
/// directory itself is an error, and callers treat even that as
/// non-fatal.
pub fn save_rings(st: &DaemonState, dir: &Path) -> std::io::Result<usize> {
    // Screen bytes can carry anything a session printed — keep the
    // directory and its files owner-only, like the transcripts.
    {
        use std::os::unix::fs::DirBuilderExt;
        match std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    sweep(dir);
    let mut written = 0usize;
    for (uid, sess) in &st.sessions {
        let bytes = sess.fanout.snapshot_since(None).bytes;
        if bytes.is_empty() {
            continue;
        }
        let final_path = ring_path(dir, uid);
        let tmp_path = dir.join(format!("{uid}.bin.tmp"));
        let res = write_private(&tmp_path, &bytes)
            .and_then(|()| std::fs::rename(&tmp_path, &final_path));
        match res {
            Ok(()) => written += 1,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp_path);
                eprintln!(
                    "cm-daemon: replay ring for session '{uid}' not persisted ({e}) — \
                     its next reattach starts from an empty screen"
                );
            }
        }
    }
    Ok(written)
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)
}

/// Read and unlink the persisted ring for `uid`, if any. `None` when
/// no file exists or it cannot be read (logged).
pub fn take_ring(dir: &Path, uid: &str) -> Option<Vec<u8>> {
    let path = ring_path(dir, uid);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            eprintln!(
                "cm-daemon: replay ring for session '{uid}' unreadable ({e}) — \
                 adopting with an empty replay"
            );
            let _ = std::fs::remove_file(&path);
            return None;
        }
    };
    let _ = std::fs::remove_file(&path);
    (!bytes.is_empty()).then_some(bytes)
}

/// Remove every file in `dir` (missing dir is fine). Used after
/// adoption so a ring for a session this generation did not adopt
/// can never be replayed later, and before a save to drop leftovers.
pub fn sweep(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_is_destructive_and_absent_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let rings = rings_dir_for(&dir.path().join("daemon-sessions.json"));
        assert_eq!(rings.file_name().unwrap(), RINGS_DIR_NAME);
        assert!(take_ring(&rings, "nope").is_none(), "missing dir → None");
        std::fs::create_dir_all(&rings).unwrap();
        std::fs::write(rings.join("s1.bin"), b"hello").unwrap();
        std::fs::write(rings.join("empty.bin"), b"").unwrap();
        assert_eq!(take_ring(&rings, "s1"), Some(b"hello".to_vec()));
        assert!(take_ring(&rings, "s1").is_none(), "second take finds nothing");
        assert!(take_ring(&rings, "empty").is_none(), "empty file → None");
        assert!(!rings.join("empty.bin").exists(), "empty file is consumed too");
    }

    #[test]
    fn sweep_clears_leftovers() {
        let dir = tempfile::tempdir().unwrap();
        let rings = dir.path().join(RINGS_DIR_NAME);
        std::fs::create_dir_all(&rings).unwrap();
        std::fs::write(rings.join("a.bin"), b"a").unwrap();
        std::fs::write(rings.join("b.bin.tmp"), b"b").unwrap();
        sweep(&rings);
        assert_eq!(std::fs::read_dir(&rings).unwrap().count(), 0);
        sweep(dir.path().join("absent").as_path());
    }
}
