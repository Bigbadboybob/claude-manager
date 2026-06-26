//! `runs.jsonl` append-only audit log for a continuous task.
//!
//! Each fire of a continuous task records one [`RunLogLine`] to
//! `~/.cm/continuous-tasks/<task-id>/runs.jsonl`. In Phase 2 (the trigger
//! funnel, manual-only) the only event written is `"fired"`; later phases write
//! `"orphaned"` / `"supervised_restart"` / `"stuck"` / `"exit"`.
//!
//! This is a structural TWIN of `daemon/src/workflow/events.rs`'s
//! `WorkflowEventsWriter::append_event` machinery: a per-id IN-PROCESS `Mutex`
//! serializes appends (a line can exceed `PIPE_BUF`), an ancestor-symlink walk
//! plus `O_NOFOLLOW` + `fchmod(0o600)` harden the path, and a single buffered
//! `write_all` + `flush` + `sync_all` makes each append durable and torn-record
//! safe.
//!
//! Unlike `events.jsonl`, `runs.jsonl` is a WRITE-ONLY audit in Phase 2 — there
//! is no tail-by-offset reader and no broadcaster. The live update path rides
//! `manifest.watch` (the Phase-1 `continuous_task_id` on `ManifestDiff::Added`
//! / `Exited`), not a runlog subscriber. So none of the `read_new` /
//! `EventKind` / `WorkflowEventWatcher` surface is copied here.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::continuous::task;

/// One line in a task's `runs.jsonl`. `ts` is f64 epoch-seconds, mirroring
/// `workflow::events::Event.ts`. `event` is the audit verb
/// (`"fired"|"orphaned"|"supervised_restart"|"stuck"|"exit"` — Phase 2 writes
/// `"fired"`). The optional fields are `#[serde(default)]` so a partial line or
/// a future shape parses fine.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunLogLine {
    pub seq: u64,
    pub ts: f64,
    pub task_id: String,
    pub event: String,
    #[serde(default)]
    pub fire_token: Option<String>,
    #[serde(default)]
    pub session_uid: Option<String>,
    #[serde(default)]
    pub run_mode: Option<String>,
    #[serde(default)]
    pub trigger_source: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub detail: Option<serde_json::Value>,
}

/// Per-task write locks. `OnceLock` so it's `Sync` without `lazy_static!`; the
/// inner `HashMap` is locked briefly to fetch (or install) the per-task mutex.
/// Twin of `events::WRITER_LOCKS`.
static RUNLOG_LOCKS: OnceLock<Mutex<HashMap<String, std::sync::Arc<Mutex<()>>>>> = OnceLock::new();

fn runlog_lock_for(task_id: &str) -> std::sync::Arc<Mutex<()>> {
    let map = RUNLOG_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = map.lock().unwrap_or_else(|p| p.into_inner());
    guard
        .entry(task_id.to_string())
        .or_insert_with(|| std::sync::Arc::new(Mutex::new(())))
        .clone()
}

/// Daemon-side append writer for `runs.jsonl`. Twin of
/// `workflow::events::WorkflowEventsWriter`.
pub struct ContinuousRunLog;

impl ContinuousRunLog {
    /// GC hook: drop the per-task writer-lock entry once a task is deleted.
    /// Twin of `WorkflowEventsWriter::release_writer_lock`. Idempotent; safe for
    /// an unknown task_id (no-op). Not required for Phase 2 — `continuous.delete`
    /// is the natural caller in a later slice.
    ///
    /// Caller must hold no other reference to the lock when calling — otherwise
    /// the next `runlog_lock_for(task_id)` would mint a new lock and concurrent
    /// writes against the stale `Arc` would race.
    pub fn release_runlog_lock(task_id: &str) {
        let map = match RUNLOG_LOCKS.get() {
            Some(m) => m,
            None => return, // no writes ever happened; nothing to GC
        };
        let mut guard = map.lock().unwrap_or_else(|p| p.into_inner());
        guard.remove(task_id);
    }

    /// Append `line` to `~/.cm/continuous-tasks/<line.task_id>/runs.jsonl`.
    /// Creates the task directory (and its parent) at mode `0o700`, the file at
    /// `0o600` (re-chmoded on every append so a pre-existing looser file can't
    /// leak the audit). Fsyncs on success.
    ///
    /// **Why these modes:** the runs.jsonl records prompts / session uids /
    /// trigger provenance. Group-readable would leak them; group-writable would
    /// let a local group user forge audit entries.
    pub fn append(line: &RunLogLine) -> io::Result<()> {
        Self::append_inner(line)
    }

    fn append_inner(line: &RunLogLine) -> io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::io::AsRawFd;

        // Reject containment-unsafe task_ids before they hit the filesystem.
        // Validation BEFORE any path is constructed from the task_id — same
        // fail-loud, two-layers-deep invariant as `task::save` / `load_one`.
        task::validate_task_id(&line.task_id)?;

        let lock = runlog_lock_for(&line.task_id);
        let _guard = lock.lock().unwrap_or_else(|p| p.into_inner());

        // Refuse to traverse any ancestor symlink between the trusted root and
        // the per-task dir BEFORE any `create_dir_all` / chmod could follow one
        // into attacker-controlled territory. `O_NOFOLLOW` on the final open
        // below covers only the last component; `ensure_dot_cm_subdir` would
        // silently follow a symlinked ancestor (it uses `metadata`, not
        // `symlink_metadata`). Derive the trusted root from `path::dot_cm_dir()`
        // (the SAME helper `task::tasks_dir` uses) so a no-HOME container env
        // doesn't break the append.
        let trusted_root = crate::path::dot_cm_dir();
        let dir = task::task_dir(&line.task_id);
        crate::path::verify_no_symlinks_in_path(&dir, &trusted_root)?;

        // Tighten the PARENT `~/.cm/continuous-tasks` to `0o700` first, then the
        // per-task subdir. If the parent is owner-only, no other user can
        // pre-seed anything inside — closing the symlink-prep-then-write attack
        // class. `ensure_dot_cm_subdir` both creates and tightens drift.
        crate::path::ensure_dot_cm_subdir(&task::tasks_dir())?;
        crate::path::ensure_dot_cm_subdir(&dir)?;

        let path = task::runs_log_path(&line.task_id);

        // `O_NOFOLLOW` refuses to open `runs.jsonl` if it's a symlink.
        // `read(true)` upgrades the fd to `O_RDWR|O_APPEND` so
        // `file_ends_unterminated` below can pread the last byte; writes still
        // go to EOF under `O_APPEND`. `mode` only takes effect on CREATE; the
        // fchmod below tightens drift on an existing file.
        let f = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        // `fchmod` on the open fd, NOT `set_permissions(path, ...)`. Closes the
        // TOCTOU window between `open` and chmod — fchmod follows the fd, not
        // the path (`set_permissions` on Unix is `chmod(path)`, which follows
        // symlinks).
        let rc = unsafe { libc::fchmod(f.as_raw_fd(), 0o600) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }

        // Build the entire payload (optional leading newline + JSON + trailing
        // newline) in ONE buffer and issue exactly ONE `write_all`. For lines
        // under PIPE_BUF (~4KB on Linux), a single write_all to an O_APPEND fd
        // is atomic at the kernel level — same-process crash safety. The
        // defensive prepend: if the file's last byte is not a newline (a torn
        // record left from a prior crash), start the buffer with `\n` so this
        // new line doesn't concatenate onto the un-terminated tail.
        let needs_leading_newline = file_ends_unterminated(&f)?;
        let mut buf = Vec::with_capacity(payload_estimate(line, needs_leading_newline));
        if needs_leading_newline {
            buf.push(b'\n');
        }
        serde_json::to_writer(&mut buf, line).map_err(io::Error::other)?;
        buf.push(b'\n');

        let mut f = f;
        f.write_all(&buf)?;
        f.flush()?;
        f.sync_all()?;
        Ok(())
    }
}

/// Cheap upper-bound for `Vec::with_capacity` — avoids growing the buffer while
/// building the payload. Conservative; over-allocation is fine for the rare
/// line with a large `detail`. Twin of `events::event_payload_estimate`.
fn payload_estimate(line: &RunLogLine, leading_newline: bool) -> usize {
    let detail_hint = line
        .detail
        .as_ref()
        .and_then(|v| serde_json::to_string(v).ok())
        .map(|s| s.len())
        .unwrap_or(0);
    256 + detail_hint + if leading_newline { 1 } else { 0 } + 1
}

/// Read the file's last byte (without disturbing the O_APPEND fd's position)
/// and return `true` if it's not `\n` — i.e., the file ends with an
/// unterminated record. Empty files return `false`. Verbatim twin of
/// `events::file_ends_unterminated`.
fn file_ends_unterminated(f: &File) -> io::Result<bool> {
    use std::os::unix::fs::FileExt;
    let len = f.metadata()?.len();
    if len == 0 {
        return Ok(false);
    }
    let mut last = [0u8; 1];
    f.read_at(&mut last, len - 1)?;
    Ok(last[0] != b'\n')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_temp_home<F: FnOnce()>(f: F) -> tempfile::TempDir {
        let _guard = crate::test_support::env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let orig = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }
        f();
        if let Some(o) = orig {
            unsafe {
                std::env::set_var("HOME", o);
            }
        }
        tmp
    }

    fn make_line(task_id: &str, seq: u64) -> RunLogLine {
        RunLogLine {
            seq,
            ts: seq as f64,
            task_id: task_id.to_string(),
            event: "fired".to_string(),
            fire_token: Some(format!("ft_{:x}", seq)),
            session_uid: Some(format!("ts-{:x}-0", seq)),
            run_mode: Some("fresh".to_string()),
            trigger_source: Some("operator".to_string()),
            status: Some("running".to_string()),
            detail: None,
        }
    }

    /// Fresh-directory creation produces the task directory at `0o700` and
    /// `runs.jsonl` at `0o600`, even under a permissive umask.
    #[test]
    fn append_creates_dir_0700_and_file_0600() {
        use std::os::unix::fs::PermissionsExt;
        let _tmp = with_temp_home(|| {
            let saved_umask = unsafe { libc::umask(0o002) };
            let task_id = "ct_perms";
            ContinuousRunLog::append(&make_line(task_id, 1)).expect("append ok");
            unsafe {
                libc::umask(saved_umask);
            }

            let dir = task::task_dir(task_id);
            let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o700, "task dir must be 0o700, got 0o{:o}", dir_mode);

            let path = task::runs_log_path(task_id);
            let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                file_mode, 0o600,
                "runs.jsonl must be 0o600, got 0o{:o}",
                file_mode,
            );
        });
    }

    /// Each append writes exactly one well-formed JSON line — pin against a
    /// regression that splits JSON and newline across two writes.
    #[test]
    fn append_writes_one_well_formed_line_per_call() {
        let _tmp = with_temp_home(|| {
            let task_id = "ct_single_line";
            for i in 0..5 {
                ContinuousRunLog::append(&make_line(task_id, i)).expect("append ok");
            }
            let path = task::runs_log_path(task_id);
            let contents = std::fs::read_to_string(&path).unwrap();
            assert!(
                contents.ends_with('\n'),
                "file ends with newline (no torn record): {:?}",
                contents,
            );
            let lines: Vec<&str> = contents.lines().collect();
            assert_eq!(lines.len(), 5);
            for (i, line) in lines.iter().enumerate() {
                let parsed: RunLogLine = serde_json::from_str(line)
                    .unwrap_or_else(|e| panic!("line {} parse failed: {:?} ({})", i, e, line));
                assert_eq!(parsed.seq, i as u64);
                assert_eq!(parsed.event, "fired");
            }
        });
    }

    /// The defensive prepend prevents concatenation onto an un-terminated tail
    /// left by a prior crash.
    #[test]
    fn append_prepends_newline_when_file_ends_unterminated() {
        let _tmp = with_temp_home(|| {
            let task_id = "ct_prepend";
            let dir = task::task_dir(task_id);
            std::fs::create_dir_all(&dir).unwrap();
            let path = task::runs_log_path(task_id);
            std::fs::write(&path, b"torn-leftover-no-newline").unwrap();

            ContinuousRunLog::append(&make_line(task_id, 1)).expect("append ok");

            let contents = std::fs::read_to_string(&path).unwrap();
            assert!(
                contents.starts_with("torn-leftover-no-newline\n"),
                "writer must insert a newline before the new line; got {:?}",
                contents,
            );
            let lines: Vec<&str> = contents.split('\n').collect();
            assert_eq!(lines.len(), 3);
            assert_eq!(lines[0], "torn-leftover-no-newline");
            let parsed: RunLogLine =
                serde_json::from_str(lines[1]).expect("second line parses on its own");
            assert_eq!(parsed.seq, 1);
            assert!(lines[2].is_empty(), "trailing newline");
        });
    }

    /// Concurrent appends from many threads to the same task produce
    /// well-formed JSONL — every byte sequence belongs to exactly one line, no
    /// torn lines, no interleaving. Per-task in-process mutex is the contract.
    #[test]
    fn concurrent_appends_well_formed_jsonl() {
        let _tmp = with_temp_home(|| {
            let task_id = "ct_concurrent";
            const THREADS: usize = 8;
            const PER: usize = 25;
            // A `detail` payload larger than PIPE_BUF (4096) exercises the
            // "single write may not be atomic" failure mode the lock guards.
            let big = "x".repeat(5000);

            let mut handles = Vec::new();
            for t in 0..THREADS {
                let big = big.clone();
                let task_owned = task_id.to_string();
                handles.push(std::thread::spawn(move || {
                    for i in 0..PER {
                        let mut line = make_line(&task_owned, (t * PER + i) as u64);
                        line.fire_token = Some(format!("ft-{}-{}", t, i));
                        line.detail = Some(serde_json::json!({"pad": big}));
                        ContinuousRunLog::append(&line).expect("append ok");
                    }
                }));
            }
            for h in handles {
                h.join().expect("thread joined");
            }

            let path = task::runs_log_path(task_id);
            let contents = std::fs::read_to_string(&path).expect("read");
            let lines: Vec<&str> = contents.lines().collect();
            assert_eq!(
                lines.len(),
                THREADS * PER,
                "every line lands on its own line",
            );
            let mut tokens = std::collections::HashSet::new();
            for line in &lines {
                let parsed: RunLogLine = serde_json::from_str(line)
                    .unwrap_or_else(|e| panic!("torn or invalid line: {}: {:?}", e, line));
                let ft = parsed.fire_token.clone().expect("fire_token present");
                assert!(
                    tokens.insert(ft.clone()),
                    "duplicate fire_token (suggests interleaving or write loss): {}",
                    ft,
                );
            }
            assert_eq!(tokens.len(), THREADS * PER);
        });
    }

    /// Containment-unsafe task_ids are rejected with `InvalidInput` BEFORE any
    /// filesystem operation.
    #[test]
    fn append_rejects_unsafe_task_id() {
        let _tmp = with_temp_home(|| {
            for bad in ["", ".", "..", "../etc", "foo/bar"] {
                let mut line = make_line("ok", 1);
                line.task_id = bad.to_string();
                let r = ContinuousRunLog::append(&line);
                assert!(r.is_err(), "unsafe task_id {:?} must be rejected", bad);
                assert_eq!(
                    r.unwrap_err().kind(),
                    io::ErrorKind::InvalidInput,
                    "unsafe task_id {:?} must error with InvalidInput",
                    bad,
                );
            }
        });
    }
}
