//! Memory-cap-kill detection producer. Slice 12 of doc/persistent-host-daemon.md.
//!
//! The TUI's existing `session_watch::write_kill_log_to` writes a
//! JSONL record into `~/.cm/memory_kills/<uid>.jsonl` *only* when the
//! cgroup OOM-killer fires on a session that hit its memory cap. So
//! the file's content is the source of truth for "was a cap-kill
//! ever recorded for this UID?" — but the file is append-only, and
//! session UIDs are reused across respawns (workflow `fresh` context
//! re-spawns under the same UID by design). A naive "non-empty
//! file ⇒ this exit was a cap-kill" rule fires spurious toasts on
//! every clean exit after a UID's first kill.
//!
//! Per-spawn baseline. The daemon captures the kill log's size at
//! spawn time via [`capture_baseline_for_spawn`] and stores it on
//! the in-memory session record. On reap, [`probe_kill_log_since`]
//! and [`build_last_exit_since`] only consider bytes past that
//! baseline — anything older belongs to a previous instance under
//! the same UID and must not contaminate the current exit's
//! cap-kill flag. The shared `build_last_exit_since` is the single
//! producer for both surfaces named in the doc's acceptance
//! criterion: the attach-stream End frame (attached path) and the
//! `ManifestDiff::Exited` `LastExit.memory_cap_kill` field (detached
//! path).

use std::path::{Path, PathBuf};

use crate::manifest::LastExit;

/// Result of inspecting a session's `<uid>.jsonl` kill log since
/// the per-spawn baseline.
#[derive(Debug, PartialEq, Eq)]
pub struct KillProbe {
    /// `true` iff there's at least one well-formed record past the
    /// baseline offset.
    pub memory_cap_kill: bool,
    /// Byte offset of the latest kill record's first byte within
    /// the file (absolute, not baseline-relative). `None` when no
    /// post-baseline records exist.
    pub kills_file_offset: Option<u64>,
}

/// Capture the current size of `<kills_dir>/<uid>.jsonl` so reap
/// can later distinguish records that landed during *this* spawn
/// from stale ones left by a previous instance under the same UID.
///
/// Ensures the kills directory exists and touches the log file so
/// the reap path can do straight `fs::metadata` + `fs::read` without
/// handling ENOENT. Returns the file's current size (in bytes) —
/// that's the baseline to pass to [`probe_kill_log_since`] /
/// [`build_last_exit_since`] when this spawn exits.
///
/// `kills_dir` is `~/.cm/memory_kills` in production; tests pass a
/// tempdir.
pub fn capture_baseline_for_spawn(kills_dir: &Path, uid: &str) -> std::io::Result<u64> {
    // Use the shared `ensure_dot_cm_subdir` helper for the
    // directory — it hardens to 0o700 regardless of the process
    // umask. The slice-10c-c review #2 caught the regression
    // where this used plain `create_dir_all` and landed the dir
    // at 0o755 under a default 022 umask, widening access to
    // memory-kill metadata.
    crate::path::ensure_dot_cm_subdir(kills_dir)?;
    let path = kill_log_path(kills_dir, uid);
    // Touch the file so the reap path's metadata/read calls don't
    // have to special-case "doesn't exist yet". File mode is
    // 0o600 (owner-only) for the same security reason as the
    // directory — kill records contain pids, comms, RSS, argv
    // hashes.
    use std::os::unix::fs::OpenOptionsExt;
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true).mode(0o600);
    let _ = opts.open(&path)?;
    // Belt-and-suspenders: if the file existed before the open
    // (so `mode(0o600)` was a no-op per `O_CREAT` semantics),
    // re-assert the mode on disk. Matches the TUI's
    // session_watch's defensive posture.
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(&path)?;
    let mut perms = meta.permissions();
    perms.set_mode(0o600);
    let _ = std::fs::set_permissions(&path, perms);
    let metadata = std::fs::metadata(&path)?;
    Ok(metadata.len())
}

/// Inspect `<kills_dir>/<uid>.jsonl` for records that landed *after*
/// the given baseline offset.
///
/// `baseline` is the file size captured by
/// [`capture_baseline_for_spawn`] at the moment this session was
/// spawned. Bytes at offsets `< baseline` are historical and must
/// be ignored. Bytes at offsets `>= baseline` belong to this
/// spawn's lifetime — if any exist, the daemon's child reaper saw
/// at least one cgroup OOM event for this session.
///
/// On any error (missing file despite spawn-time touch, unreadable
/// path) returns `memory_cap_kill: false` rather than fabricating
/// a kill — false negative is preferable to false positive here,
/// since the toast is user-visible.
pub fn probe_kill_log_since(kills_dir: &Path, uid: &str, baseline: u64) -> KillProbe {
    let path = kill_log_path(kills_dir, uid);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => {
            return KillProbe {
                memory_cap_kill: false,
                kills_file_offset: None,
            };
        }
    };
    if (bytes.len() as u64) <= baseline {
        // No growth since baseline — nothing was killed during
        // this spawn's lifetime, even if older records exist.
        return KillProbe {
            memory_cap_kill: false,
            kills_file_offset: None,
        };
    }
    // The new content lives at `bytes[baseline..]`. Find the
    // offset of the LAST record start within that slice and add
    // `baseline` to get the absolute offset for the surfaced
    // `kills_file_offset`.
    let baseline_usize = baseline as usize;
    let new_content = &bytes[baseline_usize..];
    let last_start_in_new = last_record_offset(new_content) as usize;
    KillProbe {
        memory_cap_kill: true,
        kills_file_offset: Some(baseline + last_start_in_new as u64),
    }
}

/// Construct a [`LastExit`] for a session that just exited.
/// `baseline` is the file-size snapshot captured at spawn time;
/// `code` and `exited_at` come from the daemon's session reaper.
pub fn build_last_exit_since(
    kills_dir: &Path,
    uid: &str,
    baseline: u64,
    code: Option<i32>,
    exited_at: f64,
) -> LastExit {
    let probe = probe_kill_log_since(kills_dir, uid, baseline);
    LastExit {
        code,
        memory_cap_kill: probe.memory_cap_kill,
        kills_file_offset: probe.kills_file_offset,
        exited_at,
    }
}

/// `<kills_dir>/<uid>.jsonl`. Exposed so callers (and tests) that
/// want to read the record body can find it themselves.
pub fn kill_log_path(kills_dir: &Path, uid: &str) -> PathBuf {
    kills_dir.join(format!("{}.jsonl", uid))
}

/// Byte offset of the LAST record's first byte in a JSONL slice.
///
/// JSONL invariant: each record is one line, terminated by `\n`.
/// The final record is everything after the last `\n` *not at the
/// slice's tail*. If the slice is `"{...}\n{...}\n"`, the last
/// record starts at the position after the second-to-last `\n`. If
/// only one record (`"{...}\n"`), the last record starts at offset
/// 0. Empty input returns 0.
fn last_record_offset(bytes: &[u8]) -> u64 {
    if bytes.is_empty() {
        return 0;
    }
    let scan_end = if bytes.last() == Some(&b'\n') {
        bytes.len() - 1
    } else {
        bytes.len()
    };
    match bytes[..scan_end].iter().rposition(|&b| b == b'\n') {
        Some(pos) => (pos + 1) as u64,
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Same env_lock pattern as the rest of the crate's
    /// tempdir-touching tests, for the same reason — bind tests
    /// transiently flip umask, which affects tempdir/create_dir_all
    /// mode if a probe runs concurrently.
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_support::env_lock()
    }

    fn write_log(tmp: &TempDir, uid: &str, content: &str) {
        std::fs::write(tmp.path().join(format!("{}.jsonl", uid)), content)
            .expect("write kill log");
    }

    fn append_log(tmp: &TempDir, uid: &str, content: &str) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(tmp.path().join(format!("{}.jsonl", uid)))
            .expect("open kill log");
        f.write_all(content.as_bytes()).expect("append");
    }

    fn sample_record(rec_id: u32) -> String {
        // Shape mirrors session_watch::write_kill_log_to.
        format!(
            r#"{{"ts":1700000000,"session_uid":"ts-x","pid":{},"comm":"claude","argc":2,"argv_sha256_prefix":"deadbeef","rss_kb":1024,"soft_cap_bytes":104857600,"hard_cap_bytes":209715200}}"#,
            rec_id
        )
    }

    // --- capture_baseline_for_spawn ----------------------------------

    #[test]
    fn baseline_for_fresh_uid_is_zero_and_creates_file() {
        let _g = lock();
        let tmp = TempDir::new().unwrap();
        let baseline = capture_baseline_for_spawn(tmp.path(), "ts-fresh").unwrap();
        assert_eq!(baseline, 0);
        assert!(
            tmp.path().join("ts-fresh.jsonl").is_file(),
            "spawn capture must touch the file so reap doesn't ENOENT",
        );
    }

    #[test]
    fn baseline_for_uid_with_history_matches_existing_size() {
        let _g = lock();
        let tmp = TempDir::new().unwrap();
        // Pre-existing stale content from an earlier instance under
        // this same UID.
        let stale = format!("{}\n", sample_record(1));
        write_log(&tmp, "ts-reused", &stale);

        let baseline = capture_baseline_for_spawn(tmp.path(), "ts-reused").unwrap();
        assert_eq!(
            baseline,
            stale.len() as u64,
            "baseline must equal file size at spawn time",
        );
    }

    #[test]
    fn baseline_creates_kills_dir_if_missing() {
        let _g = lock();
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("nested/kills");
        // `nested` doesn't exist yet — capture must mkdir -p.
        let baseline = capture_baseline_for_spawn(&nested, "ts-x").unwrap();
        assert_eq!(baseline, 0);
        assert!(nested.join("ts-x.jsonl").is_file());
    }

    // --- probe_kill_log_since ----------------------------------------

    #[test]
    fn probe_with_no_growth_returns_no_kill_even_if_file_is_nonempty() {
        // The regression the reviewer caught: a UID with stale
        // historical kills must NOT report `memory_cap_kill: true`
        // on subsequent clean exits.
        let _g = lock();
        let tmp = TempDir::new().unwrap();
        let stale = format!("{}\n", sample_record(1));
        write_log(&tmp, "ts-stale-only", &stale);
        // Spawn captures the post-stale size as baseline.
        let baseline = capture_baseline_for_spawn(tmp.path(), "ts-stale-only").unwrap();
        // No new records during this spawn's lifetime.
        let probe = probe_kill_log_since(tmp.path(), "ts-stale-only", baseline);
        assert_eq!(
            probe,
            KillProbe {
                memory_cap_kill: false,
                kills_file_offset: None,
            },
            "stale kills under a reused UID must not flag this exit",
        );
    }

    #[test]
    fn probe_with_fresh_kill_after_baseline_reports_cap_kill() {
        // The complementary happy path: a fresh kill landed during
        // this spawn — surface it.
        let _g = lock();
        let tmp = TempDir::new().unwrap();
        let baseline = capture_baseline_for_spawn(tmp.path(), "ts-fresh").unwrap();
        // Now during this spawn's lifetime, the cgroup OOM fires:
        let fresh = format!("{}\n", sample_record(2));
        append_log(&tmp, "ts-fresh", &fresh);

        let probe = probe_kill_log_since(tmp.path(), "ts-fresh", baseline);
        assert!(probe.memory_cap_kill);
        assert_eq!(
            probe.kills_file_offset,
            Some(baseline),
            "single fresh record starts at the baseline offset",
        );
    }

    #[test]
    fn probe_with_stale_plus_fresh_only_counts_fresh() {
        // The doc's regression case fully exercised: pre-existing
        // stale record, baseline captured, then a fresh kill lands
        // during the spawn. memory_cap_kill must be true; offset
        // must point at the FRESH record, not the stale one.
        let _g = lock();
        let tmp = TempDir::new().unwrap();
        let stale = format!("{}\n", sample_record(1));
        write_log(&tmp, "ts-mixed", &stale);
        let baseline = capture_baseline_for_spawn(tmp.path(), "ts-mixed").unwrap();
        assert_eq!(baseline, stale.len() as u64);

        // Fresh kill lands.
        let fresh = format!("{}\n", sample_record(2));
        append_log(&tmp, "ts-mixed", &fresh);

        let probe = probe_kill_log_since(tmp.path(), "ts-mixed", baseline);
        assert!(probe.memory_cap_kill);
        assert_eq!(
            probe.kills_file_offset,
            Some(baseline),
            "offset must point at the fresh record, not the stale one",
        );

        // Sanity: read from the offset, confirm it's the fresh record.
        let bytes = std::fs::read(tmp.path().join("ts-mixed.jsonl")).unwrap();
        let tail = &bytes[probe.kills_file_offset.unwrap() as usize..];
        assert!(
            tail.starts_with(fresh.as_bytes()),
            "offset must locate the FRESH record body",
        );
    }

    #[test]
    fn probe_with_multiple_fresh_records_points_at_the_last_one() {
        // If the cgroup fires twice during one spawn (rare but
        // possible — e.g. soft+hard cap), the offset must point at
        // the LATEST record so the toast describes the proximate
        // cause.
        let _g = lock();
        let tmp = TempDir::new().unwrap();
        let baseline = capture_baseline_for_spawn(tmp.path(), "ts-multi").unwrap();

        let r1 = sample_record(1);
        let r2 = sample_record(2);
        let r3 = sample_record(3);
        append_log(&tmp, "ts-multi", &format!("{}\n{}\n{}\n", r1, r2, r3));

        let probe = probe_kill_log_since(tmp.path(), "ts-multi", baseline);
        assert!(probe.memory_cap_kill);
        let expected = baseline + (r1.len() + 1 + r2.len() + 1) as u64;
        assert_eq!(probe.kills_file_offset, Some(expected));
    }

    #[test]
    fn probe_missing_file_returns_no_kill() {
        // Defensive: if the file got deleted between spawn-touch
        // and reap (shouldn't happen, but cheap to handle), do
        // NOT fabricate a kill flag.
        let _g = lock();
        let tmp = TempDir::new().unwrap();
        let probe = probe_kill_log_since(tmp.path(), "ts-deleted", 0);
        assert_eq!(
            probe,
            KillProbe {
                memory_cap_kill: false,
                kills_file_offset: None,
            }
        );
    }

    #[test]
    fn probe_record_without_trailing_newline_still_reports_kill() {
        // Crashed writer leaves the last record unterminated. As
        // long as bytes exist past the baseline, treat as a kill.
        let _g = lock();
        let tmp = TempDir::new().unwrap();
        let baseline = capture_baseline_for_spawn(tmp.path(), "ts-no-nl").unwrap();
        // No trailing newline.
        append_log(&tmp, "ts-no-nl", &sample_record(1));

        let probe = probe_kill_log_since(tmp.path(), "ts-no-nl", baseline);
        assert!(probe.memory_cap_kill);
        assert_eq!(probe.kills_file_offset, Some(baseline));
    }

    // --- build_last_exit_since ---------------------------------------

    #[test]
    fn build_last_exit_with_stale_only_yields_clean_exit() {
        let _g = lock();
        let tmp = TempDir::new().unwrap();
        let stale = format!("{}\n", sample_record(1));
        write_log(&tmp, "ts-respawn", &stale);
        let baseline = capture_baseline_for_spawn(tmp.path(), "ts-respawn").unwrap();

        // Session exits cleanly (no fresh kill records).
        let exit = build_last_exit_since(
            tmp.path(),
            "ts-respawn",
            baseline,
            Some(0),
            1_700_000_002.0,
        );
        assert_eq!(exit.code, Some(0));
        assert!(
            !exit.memory_cap_kill,
            "clean exit under reused UID with stale kill history must NOT flag cap-kill",
        );
        assert_eq!(exit.kills_file_offset, None);
    }

    #[test]
    fn build_last_exit_with_fresh_kill_flags_cap_kill() {
        let _g = lock();
        let tmp = TempDir::new().unwrap();
        let baseline = capture_baseline_for_spawn(tmp.path(), "ts-killed").unwrap();

        // Cgroup fires during this spawn.
        append_log(&tmp, "ts-killed", &format!("{}\n", sample_record(1)));

        let exit = build_last_exit_since(
            tmp.path(),
            "ts-killed",
            baseline,
            Some(137),
            1_700_000_003.0,
        );
        assert_eq!(exit.code, Some(137));
        assert!(exit.memory_cap_kill);
        assert_eq!(exit.kills_file_offset, Some(baseline));
        assert_eq!(exit.exited_at, 1_700_000_003.0);
    }

    #[test]
    fn build_last_exit_passes_through_none_code_for_signal_kills() {
        let _g = lock();
        let tmp = TempDir::new().unwrap();
        let baseline = capture_baseline_for_spawn(tmp.path(), "ts-signal").unwrap();
        let exit = build_last_exit_since(tmp.path(), "ts-signal", baseline, None, 0.0);
        assert_eq!(exit.code, None);
        assert!(!exit.memory_cap_kill);
    }

    // --- kill_log_path -----------------------------------------------

    #[test]
    fn kill_log_path_composes_under_kills_dir() {
        let dir = Path::new("/var/cache/cm/memory_kills");
        let p = kill_log_path(dir, "ts-abc");
        assert_eq!(p, Path::new("/var/cache/cm/memory_kills/ts-abc.jsonl"));
    }

    // --- Perms hardening (slice 10c-c review fix #2) ------------------------

    /// RAII guard duplicated locally because the one in
    /// `crate::path::tests` is `cfg(test)` private. Keeps the
    /// reaper tests self-contained.
    struct UmaskGuard {
        previous: libc::mode_t,
    }
    impl UmaskGuard {
        fn set(mask: libc::mode_t) -> Self {
            let previous = unsafe { libc::umask(mask) };
            Self { previous }
        }
    }
    impl Drop for UmaskGuard {
        fn drop(&mut self) {
            unsafe {
                libc::umask(self.previous);
            }
        }
    }

    #[test]
    fn kills_dir_is_0700_after_capture_baseline_under_permissive_umask() {
        // Named regression: `capture_baseline_for_spawn` used
        // plain create_dir_all, leaving the dir at the umask-
        // implied mode (0o755 under typical 022 umask). The fix
        // routes through `ensure_dot_cm_subdir` which post-create
        // chmods to 0o700.
        let _lock = lock();
        let _umask = UmaskGuard::set(0o000);
        let tmp = TempDir::new().unwrap();
        let kills_dir = tmp.path().join("memory_kills");

        let _ = capture_baseline_for_spawn(&kills_dir, "ts-perm").unwrap();

        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&kills_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            "memory_kills dir must be 0o700 after capture_baseline_for_spawn (got 0o{:o})",
            mode,
        );
    }

    #[test]
    fn kill_log_file_is_0600_after_capture_baseline_under_permissive_umask() {
        // The kill-log file holds per-session OOM events. Under
        // a permissive umask, OpenOptions defaults would have
        // produced 0o666 (& umask) = 0o666 with 0o000 umask. Our
        // OpenOptions.mode(0o600) + post-create set_permissions
        // belt-and-suspenders must surface as 0o600 on disk.
        let _lock = lock();
        let _umask = UmaskGuard::set(0o000);
        let tmp = TempDir::new().unwrap();
        let kills_dir = tmp.path().join("memory_kills");

        let _ = capture_baseline_for_spawn(&kills_dir, "ts-file-perm").unwrap();

        use std::os::unix::fs::PermissionsExt;
        let log_path = kill_log_path(&kills_dir, "ts-file-perm");
        let mode = std::fs::metadata(&log_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "kill-log file must be 0o600 after capture_baseline_for_spawn (got 0o{:o})",
            mode,
        );
    }

    #[test]
    fn capture_baseline_corrects_pre_existing_file_mode_drift() {
        // If a kill-log file was created with permissive perms by
        // an earlier (broken) writer, the post-create
        // set_permissions in capture_baseline must tighten it.
        let _lock = lock();
        let _umask = UmaskGuard::set(0o000);
        let tmp = TempDir::new().unwrap();
        let kills_dir = tmp.path().join("memory_kills");
        std::fs::create_dir_all(&kills_dir).unwrap();
        let log_path = kill_log_path(&kills_dir, "ts-drift");
        // Create the file at 0o666 first.
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::fs::OpenOptionsExt;
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .mode(0o666)
            .open(&log_path)
            .unwrap();
        let mode_before =
            std::fs::metadata(&log_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode_before, 0o666, "test setup: pre-existing file at 0o666");

        // Capture must re-tighten.
        let _ = capture_baseline_for_spawn(&kills_dir, "ts-drift").unwrap();
        let mode_after =
            std::fs::metadata(&log_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode_after, 0o600,
            "pre-existing 0o666 file must be tightened to 0o600 (got 0o{:o})",
            mode_after,
        );
    }
}
