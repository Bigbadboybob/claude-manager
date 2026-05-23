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
    /// **Has** at least one well-formed record past the baseline
    /// offset. NOT directly `memory_cap_kill` — see
    /// [`is_cap_kill`] for the join with the kernel exit signal.
    /// Slice 10d watcher-fix #1.5: pre-fix this was the answer;
    /// post-fix we need the kill_status + exit_signal combination
    /// to distinguish transient soft-limit breaches (clean exit)
    /// from real cap kills.
    pub has_record: bool,
    /// Most-decisive `kill_status` from records past baseline.
    /// `Some("killed_by_us")` if any record carries that status;
    /// otherwise `Some("protected" | "no_pids" | "already_dead")`
    /// from the latest such record; `Some("killed_by_us")` for
    /// pre-fix-#2 records that lacked the field (forward-compat
    /// — the pre-#2 TUI watcher only wrote records when it
    /// actually killed). `None` when no records past baseline.
    pub kill_status: Option<String>,
    /// Byte offset of the latest kill record's first byte within
    /// the file (absolute, not baseline-relative). `None` when no
    /// post-baseline records exist.
    pub kills_file_offset: Option<u64>,
}

/// Join the watcher's `kill_status` from the most-decisive
/// post-baseline record with the kernel's `WTERMSIG` and the
/// operator-kill flag to classify whether this exit was a
/// memory-cap kill.
///
/// Slice 10d watcher-fix history:
///   - **#1.5**: added the `kill_status × signal` join so
///     transient soft-limit breaches don't fire a toast on
///     clean exit.
///   - **#4**: added the `operator_kill_requested` dimension
///     for `protected`/`no_pids`/`already_dead` rows so a
///     transient breach record + operator A-w doesn't
///     misattribute as cap-kill.
///   - **#5** (this round): extended the operator override to
///     `killed_by_us` rows too. The `killed_by_us` record
///     proves the watcher *attempted* a SIGKILL, not that the
///     watcher's SIGKILL actually delivered the killing
///     signal. In a rare concurrent race — watcher writes
///     `killed_by_us` and the operator's `kill_session` RPC
///     fires before the watcher's signal lands — the
///     operator's pidfd-SIGKILL wins and was the proximate
///     cause. The conservative rule "operator override always
///     wins when set" accepts the rare cap-and-operator-
///     concurrent-race case at the cost of clean attribution
///     elsewhere, in exchange for never claiming cap-kill on
///     an exit the operator initiated.
///
/// Decision table (post-#5):
///   - `killed_by_us` + any exit + `!operator` → `true`
///     (cap-driven kill, no operator involvement).
///   - `killed_by_us` + any exit + `operator` → **`false`**
///     (operator override; rare race accepted — see #5 above).
///   - `protected` / `no_pids` / `already_dead`
///       + signal exit + `!operator` → `true`
///       (kernel `MemoryMax`-driven kill).
///   - `protected` / `no_pids` / `already_dead`
///       + signal exit + `operator` → `false`
///       (operator-driven kill; the breach record is
///       coincidental — don't surface a cap toast on a user-
///       initiated A-w).
///   - `protected` / `no_pids` / `already_dead` + clean exit
///     → `false` (transient soft-limit recovery).
///   - no record → `false` (no breach observed for this spawn).
///
/// Trade-off (#5): if the watcher genuinely SIGKILLed the
/// agent AND an operator A-w fired concurrently (e.g., the
/// user pressed A-w around the same instant the cgroup
/// breached), the toast won't show "cap kill" — it'll show as
/// an ordinary operator kill. We treat that as the safer
/// failure mode: an operator who pressed A-w knows they did,
/// and a missing cap-kill toast is less confusing than a
/// "cap kill" toast on an exit they just initiated.
///
/// Slice 10d watcher-fix #7 (signal tightening): only
/// `signal == Some(SIGKILL)` flips the `protected`/`no_pids`/
/// `already_dead` rows. Pre-fix any signal exit did — a
/// SIGTERM/SIGINT/SIGHUP/SIGABRT from the user (Ctrl-C in a
/// detached agent, daemonized cleanup signal, etc.) would
/// falsely render as cap-kill when a transient soft-limit
/// record happened to sit past baseline. The kernel's
/// `MemoryMax` enforcement sends SIGKILL specifically; other
/// signals are user-driven and not cap-attributable. Constant
/// is read from `libc::SIGKILL` so the comparison stays
/// platform-correct.
pub fn is_cap_kill(
    kill_status: Option<&str>,
    exit_signal: Option<i32>,
    operator_kill_requested: bool,
) -> bool {
    // Operator override (slice 10d watcher-fix #5): when an
    // operator-initiated kill is in flight, attribute the exit
    // to the operator regardless of what record the watcher
    // wrote. The `killed_by_us` race with `kill_session` is
    // real (watcher wrote record but operator's pidfd-SIGKILL
    // actually delivered the signal); the conservative rule
    // accepts that case as non-cap-kill.
    if operator_kill_requested {
        return false;
    }
    match kill_status {
        Some("killed_by_us") => true,
        Some("protected") | Some("no_pids") | Some("already_dead") => {
            // Slice 10d watcher-fix #7: only SIGKILL flips
            // these rows. The kernel's MemoryMax enforcement
            // sends SIGKILL specifically; SIGTERM/SIGINT/etc.
            // are user-driven (Ctrl-C in a detached agent,
            // explicit cleanup signal, …) and must NOT render
            // as cap-kill on a transient breach record.
            exit_signal == Some(libc::SIGKILL)
        }
        // Unknown status string → conservatively treat as
        // non-kill (false-negative preferred over false-positive
        // toast).
        Some(_) => false,
        // No post-baseline record → no breach observed.
        None => false,
    }
}

/// Priority order for picking the "most decisive" kill_status
/// when multiple records exist past baseline. Higher = more
/// decisive (the watcher actually killing trumps anything else
/// — once we KillByUs'd a PID, the exit was caused by us).
fn kill_status_priority(s: &str) -> u8 {
    match s {
        "killed_by_us" => 3,
        "already_dead" => 2,
        "protected" => 1,
        "no_pids" => 1,
        _ => 0,
    }
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
                has_record: false,
                kill_status: None,
                kills_file_offset: None,
            };
        }
    };
    if (bytes.len() as u64) <= baseline {
        // No growth since baseline — nothing was killed during
        // this spawn's lifetime, even if older records exist.
        return KillProbe {
            has_record: false,
            kill_status: None,
            kills_file_offset: None,
        };
    }
    // The new content lives at `bytes[baseline..]`. Walk it
    // line by line, parse each record as JSON, pull out the
    // `kill_status` field (slice 10d watcher-fix #2). Pick
    // the most-decisive status per `kill_status_priority`.
    //
    // Forward-compat: pre-fix-#2 records (TUI local-spawn, or
    // pre-relocation daemon paths) lack the field — default to
    // `"killed_by_us"` because those producers only wrote
    // records when they actually killed.
    let baseline_usize = baseline as usize;
    let new_content = &bytes[baseline_usize..];
    let last_start_in_new = last_record_offset(new_content) as usize;

    let mut best_status: Option<String> = None;
    let mut best_priority: u8 = 0;
    for line in new_content.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        // Best-effort UTF-8 + JSON parse; malformed lines are
        // skipped (a parser bug or partial write shouldn't
        // poison the probe). The default-killed_by_us fallback
        // also kicks in on parse failure, mirroring the forward-
        // compat reasoning above — any well-formed-looking
        // record past baseline is at minimum evidence the
        // producer thought it was a kill.
        let s = match std::str::from_utf8(line) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let status_str = match serde_json::from_str::<serde_json::Value>(s) {
            Ok(v) => v.get("kill_status")
                .and_then(|x| x.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| "killed_by_us".to_string()),
            Err(_) => continue,
        };
        let prio = kill_status_priority(&status_str);
        if prio > best_priority {
            best_priority = prio;
            best_status = Some(status_str);
        } else if best_status.is_none() {
            best_status = Some(status_str);
        }
    }
    KillProbe {
        has_record: true,
        kill_status: best_status,
        kills_file_offset: Some(baseline + last_start_in_new as u64),
    }
}

/// Construct a [`LastExit`] for a session that just exited.
/// `baseline` is the file-size snapshot captured at spawn time;
/// `code`, `signal`, `operator_kill_requested`, and `exited_at`
/// come from the daemon's session reaper / kill_session RPC.
///
/// Slice 10d watcher-fix #1.5 added the `signal` join; #4 added
/// `operator_kill_requested` so an operator-driven A-w (the
/// `kill_session` RPC's pidfd-SIGKILL) doesn't get
/// misattributed when a transient `protected`/`no_pids`/`already_dead`
/// record happens to sit past baseline.
pub fn build_last_exit_since(
    kills_dir: &Path,
    uid: &str,
    baseline: u64,
    code: Option<i32>,
    signal: Option<i32>,
    operator_kill_requested: bool,
    exited_at: f64,
) -> LastExit {
    let probe = probe_kill_log_since(kills_dir, uid, baseline);
    let memory_cap_kill =
        is_cap_kill(probe.kill_status.as_deref(), signal, operator_kill_requested);
    LastExit {
        code,
        memory_cap_kill,
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
        // Pre-fix-#2 records lacked `kill_status` — the
        // probe's forward-compat default treats them as
        // `killed_by_us`, which is what these tests assume.
        format!(
            r#"{{"ts":1700000000,"session_uid":"ts-x","pid":{},"comm":"claude","argc":2,"argv_sha256_prefix":"deadbeef","rss_kb":1024,"soft_cap_bytes":104857600,"hard_cap_bytes":209715200}}"#,
            rec_id
        )
    }

    fn sample_record_with_status(rec_id: u32, status: &str) -> String {
        format!(
            r#"{{"ts":1700000000,"session_uid":"ts-x","pid":{},"comm":"claude","argc":2,"argv_sha256_prefix":"deadbeef","rss_kb":1024,"soft_cap_bytes":104857600,"hard_cap_bytes":209715200,"kill_status":"{}"}}"#,
            rec_id, status
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
        // historical kills must NOT report a post-baseline
        // record on subsequent clean exits.
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
                has_record: false,
                kill_status: None,
                kills_file_offset: None,
            },
            "stale kills under a reused UID must not surface this exit",
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
        assert!(probe.has_record);
        // Pre-#2 record without kill_status → forward-compat
        // default of "killed_by_us".
        assert_eq!(probe.kill_status.as_deref(), Some("killed_by_us"));
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
        assert!(probe.has_record);
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
        assert!(probe.has_record);
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
                has_record: false,
                kill_status: None,
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
        assert!(probe.has_record);
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
            None, // no signal — clean exit
            false, // no operator kill
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

        // Cgroup fires during this spawn. Pre-#2 format (no
        // kill_status field) → forward-compat as killed_by_us.
        append_log(&tmp, "ts-killed", &format!("{}\n", sample_record(1)));

        let exit = build_last_exit_since(
            tmp.path(),
            "ts-killed",
            baseline,
            None,
            Some(9), // SIGKILL — kernel signal
            false,   // no operator kill — this was kernel-driven
            1_700_000_003.0,
        );
        assert_eq!(exit.code, None);
        assert!(exit.memory_cap_kill);
        assert_eq!(exit.kills_file_offset, Some(baseline));
        assert_eq!(exit.exited_at, 1_700_000_003.0);
    }

    #[test]
    fn build_last_exit_passes_through_none_code_for_signal_kills() {
        let _g = lock();
        let tmp = TempDir::new().unwrap();
        let baseline = capture_baseline_for_spawn(tmp.path(), "ts-signal").unwrap();
        let exit = build_last_exit_since(
            tmp.path(),
            "ts-signal",
            baseline,
            None,
            None,
            false,
            0.0,
        );
        assert_eq!(exit.code, None);
        assert!(!exit.memory_cap_kill);
    }

    // ============================================================
    // Slice 10d watcher-fix #1.5: `is_cap_kill` table — pin each
    // row of the kill_status × exit_signal decision matrix.
    // ============================================================

    #[test]
    fn is_cap_kill_killed_by_us_always_true_regardless_of_signal() {
        // The watcher killed the target itself. Whether the
        // resulting waitpid surface was a code (very rare —
        // would require the process catching SIGTERM and
        // doing exit(N)) or a signal (the SIGKILL we sent),
        // the cap-kill flag must be true: WE caused the exit.
        assert!(is_cap_kill(Some("killed_by_us"), Some(9), false));  // SIGKILL
        assert!(is_cap_kill(Some("killed_by_us"), Some(15), false)); // SIGTERM caught + re-exited?
        assert!(is_cap_kill(Some("killed_by_us"), None, false));     // clean exit somehow
    }

    #[test]
    fn is_cap_kill_protected_plus_sigkill_exit_is_true() {
        // Slice 10d watcher-fix #7: only SIGKILL flips
        // `protected`. Pre-fix this test included `Some(15)`
        // (SIGTERM) → updated to only SIGKILL since SIGTERM
        // is not how the kernel's MemoryMax kills.
        assert!(is_cap_kill(Some("protected"), Some(libc::SIGKILL), false));
        // Re-asserted via libc constant rather than the raw 9
        // literal so the comparison stays platform-correct.
    }

    #[test]
    fn is_cap_kill_protected_plus_clean_exit_is_false() {
        // The transient soft-limit breach case — the watcher
        // saw memory.events.high increment but the process
        // recovered (kernel reclaimed pages) and exited
        // cleanly. NOT a cap kill; no toast.
        assert!(!is_cap_kill(Some("protected"), None, false));
    }

    #[test]
    fn is_cap_kill_no_pids_plus_signal_exit_is_true() {
        assert!(is_cap_kill(Some("no_pids"), Some(9), false));
    }

    #[test]
    fn is_cap_kill_no_pids_plus_clean_exit_is_false() {
        // Empty cgroup at breach time, then clean exit — the
        // breach must have been a transient artifact (e.g.
        // scope teardown race).
        assert!(!is_cap_kill(Some("no_pids"), None, false));
    }

    #[test]
    fn is_cap_kill_already_dead_plus_signal_exit_is_true() {
        assert!(is_cap_kill(Some("already_dead"), Some(9), false));
    }

    #[test]
    fn is_cap_kill_already_dead_plus_clean_exit_is_false() {
        // Target exited between our PID enumeration and the
        // signal attempt. If the child then went on to a
        // clean parent-exit, it wasn't a cap kill.
        assert!(!is_cap_kill(Some("already_dead"), None, false));
    }

    #[test]
    fn is_cap_kill_no_record_is_false_regardless_of_signal() {
        // No record past baseline = no breach observed
        // during this spawn. Even if the kernel sent a
        // signal (e.g. operator-issued SIGKILL via A-w), it
        // wasn't a cap kill.
        assert!(!is_cap_kill(None, Some(9), false));
        assert!(!is_cap_kill(None, None, false));
    }

    #[test]
    fn is_cap_kill_unknown_status_string_is_false() {
        // A status field that doesn't match any of our four
        // known strings → defensive `false`. A producer bug
        // shouldn't produce phantom toasts.
        assert!(!is_cap_kill(Some("future_status_we_dont_know"), Some(9), false));
    }

    /// Slice 10d watcher-fix #1.5 priority: when both
    /// `killed_by_us` and lower-priority records exist past
    /// baseline (e.g. a transient `protected` breach followed
    /// by a later `killed_by_us`), the probe must surface
    /// the more-decisive status. This drives the truthiness
    /// in `is_cap_kill`.
    #[test]
    fn probe_picks_killed_by_us_when_mixed_with_lower_priority() {
        let _g = lock();
        let tmp = TempDir::new().unwrap();
        let baseline = capture_baseline_for_spawn(tmp.path(), "ts-mixed-statuses").unwrap();
        let r1 = sample_record_with_status(1, "protected");
        let r2 = sample_record_with_status(2, "killed_by_us");
        append_log(&tmp, "ts-mixed-statuses", &format!("{}\n{}\n", r1, r2));
        let probe = probe_kill_log_since(tmp.path(), "ts-mixed-statuses", baseline);
        assert_eq!(probe.kill_status.as_deref(), Some("killed_by_us"));
    }

    /// Symmetric: when only lower-priority statuses exist,
    /// the probe still surfaces one (so `is_cap_kill` can
    /// combine with the kernel signal).
    #[test]
    fn probe_picks_lower_priority_when_no_killed_by_us() {
        let _g = lock();
        let tmp = TempDir::new().unwrap();
        let baseline = capture_baseline_for_spawn(tmp.path(), "ts-prot-only").unwrap();
        let r1 = sample_record_with_status(1, "protected");
        append_log(&tmp, "ts-prot-only", &format!("{}\n", r1));
        let probe = probe_kill_log_since(tmp.path(), "ts-prot-only", baseline);
        assert_eq!(probe.kill_status.as_deref(), Some("protected"));
    }

    /// build_last_exit_since integration: a `protected`
    /// record + signal exit → memory_cap_kill: true.
    #[test]
    fn build_last_exit_protected_record_plus_signal_is_cap_kill() {
        let _g = lock();
        let tmp = TempDir::new().unwrap();
        let baseline = capture_baseline_for_spawn(tmp.path(), "ts-prot-sig").unwrap();
        append_log(
            &tmp,
            "ts-prot-sig",
            &format!("{}\n", sample_record_with_status(1, "protected")),
        );
        let exit = build_last_exit_since(
            tmp.path(),
            "ts-prot-sig",
            baseline,
            None,
            Some(9),
            false, // no operator kill — pure kernel-driven
            0.0,
        );
        assert!(exit.memory_cap_kill);
    }

    /// build_last_exit_since integration: a `protected`
    /// record + clean exit → memory_cap_kill: false (the
    /// transient soft-limit recovery case).
    #[test]
    fn build_last_exit_protected_record_plus_clean_exit_is_not_cap_kill() {
        let _g = lock();
        let tmp = TempDir::new().unwrap();
        let baseline = capture_baseline_for_spawn(tmp.path(), "ts-prot-clean").unwrap();
        append_log(
            &tmp,
            "ts-prot-clean",
            &format!("{}\n", sample_record_with_status(1, "protected")),
        );
        let exit = build_last_exit_since(
            tmp.path(),
            "ts-prot-clean",
            baseline,
            Some(0),
            None,
            false,
            0.0,
        );
        assert!(
            !exit.memory_cap_kill,
            "transient soft-limit breach + clean exit must NOT flag cap-kill",
        );
    }

    // ============================================================
    // Slice 10d watcher-fix #4: operator-kill flag. Four
    // table rows from the review.
    // ============================================================

    /// Slice 10d watcher-fix #5: the operator-kill override
    /// is conservative — when `operator_kill_requested == true`,
    /// `killed_by_us` is NOT enough to flip the flag.
    ///
    /// Pre-#5 we treated `killed_by_us` as unconditional cap-
    /// kill. But the record only proves the watcher *attempted*
    /// a SIGKILL — not that the watcher's signal actually
    /// delivered the killing blow. In a concurrent race
    /// (watcher writes record → operator's `kill_session` RPC
    /// fires before the watcher's signal lands → operator's
    /// pidfd-SIGKILL wins) the operator was the proximate
    /// cause, and surfacing a cap-kill toast on an operator-
    /// initiated A-w is the wrong failure mode.
    ///
    /// Trade-off: if the watcher GENUINELY caused the death
    /// and an operator A-w fired concurrently (rare), the
    /// toast won't show cap-kill. We accept that case in
    /// exchange for never falsely claiming cap-kill on an
    /// operator-initiated exit.
    #[test]
    fn is_cap_kill_operator_overrides_killed_by_us_in_race() {
        // Non-operator case stays true (pure cap-driven kill).
        assert!(is_cap_kill(Some("killed_by_us"), Some(9), false));
        assert!(is_cap_kill(Some("killed_by_us"), None, false));
        // Operator override — false (conservative). Rare race
        // where the watcher wrote a kill record but the
        // operator's pidfd-SIGKILL actually delivered the
        // killing signal.
        assert!(!is_cap_kill(Some("killed_by_us"), Some(9), true));
        assert!(!is_cap_kill(Some("killed_by_us"), Some(15), true));
        assert!(!is_cap_kill(Some("killed_by_us"), None, true));
    }

    /// Row 2: kernel-driven `MemoryMax` kill — protected record
    /// past baseline + signal exit + no operator kill. This is
    /// what fix #1.5 made true; fix #4 keeps it that way when
    /// `operator_kill_requested == false`.
    #[test]
    fn is_cap_kill_protected_plus_signal_no_operator_is_true() {
        assert!(is_cap_kill(Some("protected"), Some(9), false));
        assert!(is_cap_kill(Some("no_pids"), Some(9), false));
        assert!(is_cap_kill(Some("already_dead"), Some(9), false));
    }

    /// Row 3 — the false-positive #4 closes: operator A-w on a
    /// session that ALSO had a transient `protected`/`no_pids`/
    /// `already_dead` record. The signal exit comes from the
    /// operator's pidfd-SIGKILL, not from the cgroup. Must NOT
    /// render as cap kill.
    #[test]
    fn is_cap_kill_protected_plus_signal_with_operator_kill_is_false() {
        assert!(!is_cap_kill(Some("protected"), Some(9), true));
        assert!(!is_cap_kill(Some("no_pids"), Some(9), true));
        assert!(!is_cap_kill(Some("already_dead"), Some(9), true));
    }

    /// Row 4: clean exit cases are unaffected by the operator
    /// flag — clean exit can't have been driven by a SIGKILL
    /// from any source.
    #[test]
    fn is_cap_kill_clean_exit_is_false_regardless_of_operator_flag() {
        assert!(!is_cap_kill(Some("protected"), None, false));
        assert!(!is_cap_kill(Some("protected"), None, true));
        assert!(!is_cap_kill(Some("no_pids"), None, true));
    }

    /// And: no record + operator kill is still false (no breach
    /// observed; whatever the operator did is irrelevant to the
    /// cap-kill flag).
    #[test]
    fn is_cap_kill_no_record_plus_operator_is_false() {
        assert!(!is_cap_kill(None, Some(9), true));
        assert!(!is_cap_kill(None, None, true));
    }

    // ============================================================
    // Slice 10d watcher-fix #7: SIGKILL-specific rows. Only
    // SIGKILL (== libc::SIGKILL == 9) flips the cap-kill flag
    // on `protected`/`no_pids`/`already_dead` records. Other
    // signals (SIGTERM, SIGINT, SIGHUP, SIGABRT, …) are user-
    // driven and don't reflect cgroup MemoryMax enforcement.
    // ============================================================

    /// Pin: SIGKILL specifically flips `protected` row → true.
    /// (Re-asserts existing behavior with the explicit libc
    /// constant.)
    #[test]
    fn is_cap_kill_protected_plus_sigkill_is_true() {
        assert!(is_cap_kill(Some("protected"), Some(libc::SIGKILL), false));
        assert!(is_cap_kill(Some("no_pids"), Some(libc::SIGKILL), false));
        assert!(is_cap_kill(Some("already_dead"), Some(libc::SIGKILL), false));
    }

    /// Slice 10d watcher-fix #7 named acceptance: SIGTERM
    /// MUST NOT flip cap-kill. Kernel `MemoryMax` sends
    /// SIGKILL specifically; SIGTERM is operator-friendly
    /// cleanup, e.g. service-manager `systemctl stop` or
    /// manual `kill -TERM`.
    #[test]
    fn is_cap_kill_protected_plus_sigterm_is_false() {
        assert!(!is_cap_kill(Some("protected"), Some(libc::SIGTERM), false));
        assert!(!is_cap_kill(Some("no_pids"), Some(libc::SIGTERM), false));
        assert!(!is_cap_kill(Some("already_dead"), Some(libc::SIGTERM), false));
    }

    /// Sweep other common signals — none should flip.
    #[test]
    fn is_cap_kill_protected_plus_non_sigkill_signal_is_false() {
        for signal in &[
            libc::SIGINT,
            libc::SIGHUP,
            libc::SIGQUIT,
            libc::SIGABRT,
            libc::SIGPIPE,
            libc::SIGUSR1,
            libc::SIGUSR2,
        ] {
            assert!(
                !is_cap_kill(Some("protected"), Some(*signal), false),
                "signal {} flipped cap-kill on protected (only SIGKILL should)",
                signal
            );
            assert!(
                !is_cap_kill(Some("no_pids"), Some(*signal), false),
                "signal {} flipped cap-kill on no_pids",
                signal
            );
            assert!(
                !is_cap_kill(Some("already_dead"), Some(*signal), false),
                "signal {} flipped cap-kill on already_dead",
                signal
            );
        }
    }

    /// Operator + SIGKILL is still false (operator override
    /// wins regardless of signal — fix #5a).
    #[test]
    fn is_cap_kill_protected_plus_sigkill_with_operator_is_false() {
        assert!(!is_cap_kill(Some("protected"), Some(libc::SIGKILL), true));
    }

    /// `build_last_exit_since` integration: a `protected`
    /// record + SIGTERM → false (slice 10d watcher-fix #7).
    #[test]
    fn build_last_exit_protected_plus_sigterm_is_not_cap_kill() {
        let _g = lock();
        let tmp = TempDir::new().unwrap();
        let baseline = capture_baseline_for_spawn(tmp.path(), "ts-sigterm").unwrap();
        append_log(
            &tmp,
            "ts-sigterm",
            &format!("{}\n", sample_record_with_status(1, "protected")),
        );
        let exit = build_last_exit_since(
            tmp.path(),
            "ts-sigterm",
            baseline,
            None,
            Some(libc::SIGTERM),
            false,
            0.0,
        );
        assert!(
            !exit.memory_cap_kill,
            "SIGTERM is not how the kernel kills via MemoryMax — must NOT flag",
        );
    }

    /// `build_last_exit_since` integration for fix #4: a
    /// capped session with a `protected` record past baseline,
    /// signal exit, AND operator_kill_requested=true → exit
    /// reports `memory_cap_kill: false` (operator-driven kill).
    #[test]
    fn build_last_exit_protected_plus_operator_kill_is_not_cap_kill() {
        let _g = lock();
        let tmp = TempDir::new().unwrap();
        let baseline = capture_baseline_for_spawn(tmp.path(), "ts-op-kill").unwrap();
        append_log(
            &tmp,
            "ts-op-kill",
            &format!("{}\n", sample_record_with_status(1, "protected")),
        );
        let exit = build_last_exit_since(
            tmp.path(),
            "ts-op-kill",
            baseline,
            None,
            Some(9), // signal-killed
            true,    // by the operator (A-w)
            0.0,
        );
        assert!(
            !exit.memory_cap_kill,
            "operator A-w on a session with a transient protected breach must \
             NOT render as cap-kill — the operator initiated the kill, the \
             breach record is coincidental",
        );
    }

    /// Slice 10d watcher-fix #5: `killed_by_us` past baseline +
    /// operator kill → **NOT** cap-kill (conservative operator
    /// override). The `killed_by_us` record proves the watcher
    /// *attempted* a SIGKILL; in a concurrent race with the
    /// operator's `kill_session` RPC, the operator's pidfd-
    /// SIGKILL can win. We attribute to the operator to avoid
    /// surfacing a cap-kill toast on an operator-initiated
    /// exit.
    ///
    /// Pre-#5 this test asserted `true`; the rename + invert
    /// reflects the conservative-override rule.
    #[test]
    fn build_last_exit_killed_by_us_plus_operator_kill_is_overridden() {
        let _g = lock();
        let tmp = TempDir::new().unwrap();
        let baseline = capture_baseline_for_spawn(tmp.path(), "ts-cap-vs-op").unwrap();
        append_log(
            &tmp,
            "ts-cap-vs-op",
            &format!("{}\n", sample_record_with_status(1, "killed_by_us")),
        );
        let exit = build_last_exit_since(
            tmp.path(),
            "ts-cap-vs-op",
            baseline,
            None,
            Some(9),
            true, // operator A-w fired
            0.0,
        );
        assert!(
            !exit.memory_cap_kill,
            "operator override applies to killed_by_us too — the watcher's \
             record proves it tried to SIGKILL, not that it caused the \
             death; an operator A-w racing with the watcher's signal can \
             win, and we attribute to the operator (conservative)",
        );
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
