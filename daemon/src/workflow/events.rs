//! `events.jsonl` writer + tailer for a workflow run.
//!
//! Agents talk to the workflow runner by calling the `workflow_transition` /
//! `workflow_done` MCP tools (see `mcp_server/server.py`), which append one
//! JSON object per line to `~/.cm/workflow-runs/<run-id>/events.jsonl`. The
//! TUI's controller tails the file via [`read_new`] to react to dynamic
//! transitions.
//!
//! The [`WorkflowEventsWriter`] (10d-2a) is the daemon-side append path
//! that 10d-2b's `workflow_transition` / `workflow_done` dispatch arms
//! will write through. At 10d-2a there are no callers yet — this is
//! scaffolding so 10d-2b is a pure dispatch-arm slice, no I/O code.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom, Write};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::workflow::run;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub ts: f64,
    pub run_id: String,
    pub role: String,
    pub tool: String,
    pub args: serde_json::Value,
    /// 10d-2c-1 review round-1 (P1 #1/#2): origin tag for the
    /// TUI's tail-observer state-machine. Two values matter
    /// in Phase 1:
    /// - `"daemon"` — daemon's `workflow_transition` /
    ///   `workflow_done` handlers wrote the event AND ALREADY
    ///   applied the state-mutation half. The TUI tail must
    ///   NOT re-mutate; only deliver the activation prompt.
    /// - `"tui-mcp"` — Python MCP-server-side `_append_event`
    ///   wrote the event (TUI-local SpawnTarget fallback path
    ///   from 10d-2b). State NOT yet mutated; TUI tail must
    ///   still call `fire_transition` / `finish_run` to apply
    ///   the mutation locally.
    ///
    /// Defaults to `""` (absent on the wire) so pre-2c-1
    /// events on disk parse fine. An absent / unknown source is
    /// treated as `"tui-mcp"` (pre-2c-1 behavior) for the
    /// tail-observer's branch — same fallback as the
    /// `daemon_socket_pinned()`-driven Python tools take.
    #[serde(default)]
    pub source: String,
    /// 10d-2c-1 review round-7 (F2): outgoing role for a
    /// `workflow_transition` event, captured PRE-MUTATION by
    /// the daemon's `workflow_transition` closure under flock.
    ///
    /// Why: after the daemon's state mutation, `state.json`'s
    /// `active_role` is already the NEW role (`to`). The TUI's
    /// tail observer can no longer derive the outgoing role
    /// from `active_role` — that read would give `to`, not the
    /// actual prior role. Worse on TUI restart: TUI loads
    /// state.json fresh (active_role = reviewer), then
    /// processes the daemon-source worker→reviewer event, and
    /// records `from_role = reviewer` (wrong) in history.
    ///
    /// The TUI's daemon-routed tail handler reads `from_role`
    /// from the event itself; the TuiLocal path (Python
    /// `_append_event`) doesn't populate it and the TUI falls
    /// back to deriving from in-memory `active_role` (which is
    /// correct for TuiLocal because state hasn't mutated when
    /// the tail processes the event).
    ///
    /// `None` for `workflow_done` events (the active role is
    /// being torn down, no "next role"); `None` for pre-round-7
    /// events on disk (backward-compat via `#[serde(default)]`).
    #[serde(default)]
    pub from_role: Option<String>,
    /// 10d-2c-1 review round-15: post-mutation iteration value,
    /// captured under flock by the daemon's `workflow_transition`
    /// / `workflow_done` closure. The TUI's history append uses
    /// this to record per-event activation iteration — pre-r15
    /// it read `self.iteration` from current state.json, which
    /// caused multiple queued events to all record the LATEST
    /// iteration (the daemon's final state) rather than the
    /// per-event activation iteration.
    ///
    /// `0` for pre-r15 events on disk (backward-compat via
    /// `#[serde(default)]`); TUI falls back to `self.iteration`
    /// when `event.iteration == 0`.
    #[serde(default)]
    pub iteration: u32,
}

#[derive(Clone, Debug)]
pub enum EventKind {
    Transition { to: String, prompt: String },
    Done { reason: String },
    Unknown,
}

impl Event {
    pub fn kind(&self) -> EventKind {
        match self.tool.as_str() {
            "workflow_transition" => {
                let to = self
                    .args
                    .get("to")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let prompt = self
                    .args
                    .get("prompt")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                EventKind::Transition { to, prompt }
            }
            "workflow_done" => {
                let reason = self
                    .args
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                EventKind::Done { reason }
            }
            _ => EventKind::Unknown,
        }
    }
}

/// 10d-2c-1 review round-10: per-event variant of [`read_new`].
/// Returns each parsed event paired with the byte offset
/// **immediately after** that event's line (i.e., the offset to
/// persist if processing through that event succeeds).
///
/// Used by the TUI's tail to advance `events_offset` per-event
/// rather than once at batch end. Pre-round-10 the tail
/// advanced to the batch-final offset on every successful event
/// — a Failed event in the middle of a batch was permanently
/// skipped because earlier successes had already advanced past
/// it (and later successes would advance past it on the next
/// tick). Round-10's per-event offset + stop-at-first-failure
/// keeps the failed event re-readable.
///
/// 10d-2c-1 review round-12 (F2): returns a tuple
/// `(events, final_consumed_offset)`. `final_consumed_offset`
/// is the byte position after the last newline-terminated line
/// consumed (parsed or malformed-but-consumed). The caller
/// uses this to advance `events_offset` past malformed lines
/// that don't surface as Events — pre-r12 a malformed line in
/// events.jsonl wedged offset at 0 forever because
/// `events_with_offsets.is_empty()` triggered the static-idle
/// path (which doesn't advance offset).
pub fn read_new_with_offsets(run_id: &str, offset: u64) -> (Vec<(Event, u64)>, u64) {
    let path = run::events_path(run_id);
    let Ok(mut f) = File::open(&path) else {
        return (Vec::new(), offset);
    };
    let file_len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if offset >= file_len {
        return (Vec::new(), offset);
    }
    if f.seek(SeekFrom::Start(offset)).is_err() {
        return (Vec::new(), offset);
    }
    let mut reader = BufReader::new(f);
    let mut out: Vec<(Event, u64)> = Vec::new();
    let mut consumed: u64 = 0;
    let mut buf = String::new();
    loop {
        buf.clear();
        match reader.read_line(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                // Same torn-record + recovery semantics as
                // `read_new` — see that function for the round-5
                // / round-6 commentary on un-terminated tails.
                if !buf.ends_with('\n') {
                    let line = buf.trim();
                    if !line.is_empty() {
                        if let Ok(ev) = serde_json::from_str::<Event>(line) {
                            consumed += n as u64;
                            out.push((ev, offset + consumed));
                        }
                    }
                    break;
                }
                // Round-12 (F2): advance `consumed` BEFORE the
                // parse attempt. Newline-terminated lines are
                // permanently consumed (the writer never goes
                // back to fix a malformed line). Pre-r12 only
                // parsed-successfully lines were counted via
                // `out.push((ev, offset + consumed))`; the
                // caller had no way to learn about
                // malformed-but-consumed bytes.
                consumed += n as u64;
                let line = buf.trim();
                if line.is_empty() {
                    continue;
                }
                if let Ok(ev) = serde_json::from_str::<Event>(line) {
                    out.push((ev, offset + consumed));
                }
                // Malformed JSON: bytes consumed (offset
                // advances via the final_consumed_offset
                // return), no event surfaced.
            }
            Err(_) => break,
        }
    }
    (out, offset + consumed)
}

/// Read new events for `run_id` starting at `offset`. Returns the parsed events
/// plus the new byte offset to persist. Malformed lines are skipped silently
/// (they still advance the offset so we don't loop).
pub fn read_new(run_id: &str, offset: u64) -> (Vec<Event>, u64) {
    let path = run::events_path(run_id);
    let Ok(mut f) = File::open(&path) else {
        return (Vec::new(), offset);
    };
    let file_len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if offset >= file_len {
        return (Vec::new(), offset);
    }
    if f.seek(SeekFrom::Start(offset)).is_err() {
        return (Vec::new(), offset);
    }
    let mut reader = BufReader::new(f);
    let mut events = Vec::new();
    let mut consumed: u64 = 0;
    let mut buf = String::new();
    loop {
        buf.clear();
        match reader.read_line(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                // Round 5 correctness: only advance past
                // newline-terminated records. The writer
                // (`WorkflowEventsWriter::append_event`) appends
                // concurrently with the tailer; a poll that hits
                // EOF mid-record returns partial bytes here. If
                // we advanced past them, the now-finished record
                // would be permanently skipped on the next poll
                // because we'd resume past its bytes — the
                // `workflow_transition` / `workflow_done` event
                // would be lost. Leave the unterminated tail in
                // the offset window so the next poll re-reads
                // from the same offset (the writer will have
                // completed the record by then, or not — either
                // way we retry the same byte range, which is
                // safe because the writer never rewrites bytes
                // it already emitted).
                //
                // Round 6 recovery: BEFORE giving up at EOF on
                // an un-terminated tail, try parsing the buffer
                // as a complete `Event`. If a daemon crash
                // between the writer's JSON write and its
                // newline write left a complete JSON object
                // on disk without a trailing `\n`, the round-5
                // hold-offset behavior would refuse to ever
                // advance past it — even though the event IS
                // recoverable. The round-6 single-buffer write
                // closes the same-process crash window, but
                // this fallback recovers legacy / external-
                // process leftovers without losing the event.
                // If the parse fails (truly torn JSON), fall
                // back to round-5's hold-and-retry.
                if !buf.ends_with('\n') {
                    let line = buf.trim();
                    if !line.is_empty() {
                        if let Ok(ev) = serde_json::from_str::<Event>(line) {
                            events.push(ev);
                            consumed += n as u64;
                        }
                    }
                    break;
                }
                consumed += n as u64;
                let line = buf.trim();
                if line.is_empty() {
                    continue;
                }
                if let Ok(ev) = serde_json::from_str::<Event>(line) {
                    events.push(ev);
                }
            }
            Err(_) => break,
        }
    }
    (events, offset + consumed)
}

/// 10d-2a: daemon-side append writer for `events.jsonl`. Used
/// by 10d-2b's `workflow_transition` / `workflow_done` dispatch
/// arms — the file becomes a daemon-owned append log instead of
/// the rendezvous between MCP-server `_append_event` writes and
/// the TUI's tail loop. The TUI's tailer ([`read_new`]) keeps
/// working unchanged because the on-disk JSON shape doesn't
/// change.
///
/// **Concurrency:** per-run [`Mutex`] keyed by run_id. A single
/// `O_APPEND` `write_all` is atomic for sub-pipe-buf payloads on
/// Linux, but our events can be larger (the `prompt` field on a
/// `workflow_transition` is unbounded), so two concurrent
/// appends could interleave. The mutex serializes appends for a
/// given run; cross-run appends remain concurrent. Acceptable
/// since per-run write volume is low (a handful of events per
/// minute at most).
///
/// **Durability:** `flush` + `sync_all` on each append. The file
/// is the control plane — `workflow_transition` returning to the
/// caller has to guarantee the event is visible to the TUI's
/// tailer (and to a future reboot recovery path), not buffered.
pub struct WorkflowEventsWriter;

/// Per-run write locks. `OnceLock` so it's `Sync` without
/// `lazy_static!`; the inner `HashMap` is locked briefly to
/// fetch (or install) the per-run mutex.
static WRITER_LOCKS: OnceLock<Mutex<HashMap<String, std::sync::Arc<Mutex<()>>>>> =
    OnceLock::new();

fn writer_lock_for(run_id: &str) -> std::sync::Arc<Mutex<()>> {
    let map = WRITER_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = map.lock().unwrap_or_else(|p| p.into_inner());
    guard
        .entry(run_id.to_string())
        .or_insert_with(|| std::sync::Arc::new(Mutex::new(())))
        .clone()
}

impl WorkflowEventsWriter {
    /// Append `event` to `~/.cm/workflow-runs/<event.run_id>/events.jsonl`.
    /// Creates the run directory at mode `0o700` (tightening if it
    /// already exists with looser perms — same shape as the
    /// `~/.cm/daemon.sock` parent hardening). The file is created
    /// at mode `0o600` and re-chmoded to `0o600` on every append so
    /// a pre-existing file from a less-strict writer can't leak the
    /// control plane. Fsyncs on success.
    ///
    /// **Why these modes:** `events.jsonl` is the workflow control
    /// plane — `workflow_transition` / `workflow_done` events on it
    /// drive role activations and prompts. Group-readable would
    /// leak prompt contents; group-writable would let a local group
    /// user forge transitions.
    ///
    /// **TODO (out of scope for 10d-2a):** TUI-side
    /// `cm_daemon::workflow::run::save` (which the TUI re-exports
    /// and uses today) creates the same `~/.cm/workflow-runs/<id>/`
    /// directory with `fs::create_dir_all` under the inherited
    /// umask. As 10d-2c flips workflow ownership to the daemon,
    /// either (a) make `run::save` go through
    /// `path::ensure_dot_cm_subdir`, or (b) decommission TUI-side
    /// `state.json` writes entirely. Either way, this writer's
    /// tightening covers the migration boundary on first daemon
    /// access.
    pub fn append_event(event: &Event) -> io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::io::AsRawFd;

        // Defense-in-depth #2 (round 2): reject containment-unsafe
        // run_ids before they hit the filesystem. Caller is trusted
        // today (always produced by `run::new_run_id` or MCP's
        // `uuid.uuid4().hex`), but a future caller could pass an
        // untrusted id; validating here means no path-traversal
        // hole regardless of caller.
        run::validate_run_id(&event.run_id)?;

        let lock = writer_lock_for(&event.run_id);
        let _guard = lock.lock().unwrap_or_else(|p| p.into_inner());

        // Defense-in-depth #5 (round 3): refuse to traverse any
        // ancestor symlink between the trusted root and the per-run
        // dir BEFORE any `create_dir_all` / chmod could follow one
        // into attacker-controlled territory. `O_NOFOLLOW` on the
        // final open below covers only the last component;
        // `ensure_dot_cm_subdir` would silently follow a symlinked
        // ancestor (it uses `metadata`, not `symlink_metadata`).
        //
        // Round 4 correctness: derive the trusted root from
        // `path::dot_cm_dir()` (the SAME helper `run::runs_dir`
        // uses) instead of re-deriving from `HOME`. Without this,
        // a no-HOME container env — where the rest of the daemon
        // works because `runs_dir` falls back to `/tmp/.cm/...` —
        // would fail every workflow append.
        let trusted_root = crate::path::dot_cm_dir();
        let dir = run::run_dir(&event.run_id);
        crate::path::verify_no_symlinks_in_path(&dir, &trusted_root)?;

        // Defense-in-depth #1 (round 2): tighten the PARENT
        // `~/.cm/workflow-runs` to `0o700` first, then the per-run
        // subdir. If the parent is owner-only, no other user can
        // pre-seed anything inside — that closes the entire
        // symlink-prep-then-write attack class. `ensure_dot_cm_subdir`
        // both creates and tightens drift.
        crate::path::ensure_dot_cm_subdir(&run::runs_dir())?;
        crate::path::ensure_dot_cm_subdir(&dir)?;

        let path = run::events_path(&event.run_id);

        // Defense-in-depth #3 (round 2): `O_NOFOLLOW` refuses to
        // open `events.jsonl` if it's a symlink. Belt-and-suspenders
        // alongside the parent-dir hardening — even if the dir
        // somehow had loose perms once, we won't follow a symlink
        // placed by another user.
        let f = OpenOptions::new()
            .create(true)
            .append(true)
            // `read(true)` upgrades the fd from `O_WRONLY|O_APPEND`
            // to `O_RDWR|O_APPEND` so `file_ends_unterminated`
            // below can pread the last byte. Writes still go to
            // EOF under `O_APPEND`; the read access only affects
            // `pread` capability, not write semantics.
            .read(true)
            // `mode` only takes effect on CREATE; the fchmod below
            // tightens drift on an existing file.
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        // Defense-in-depth #4 (round 2): `fchmod` on the open fd,
        // NOT `set_permissions(path, ...)`. Closes the TOCTOU
        // window between `open` and chmod — even if an attacker
        // somehow swapped `path` to a symlink between our open
        // and our chmod, fchmod follows the fd, not the path.
        // (`set_permissions` on Unix is `chmod(path)`, which
        // follows symlinks.)
        let rc = unsafe { libc::fchmod(f.as_raw_fd(), 0o600) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }

        // Round 6 correctness: build the entire payload (optional
        // leading newline + JSON + trailing newline) in ONE buffer
        // and issue exactly ONE `write_all`. The pre-fix code
        // wrote JSON and `\n` as two separate calls — a daemon
        // crash between them left a complete JSON object on disk
        // without its terminating newline, which the (round-5)
        // hold-offset tailer would correctly refuse to advance
        // past, but which would also catastrophically concatenate
        // onto the next event's bytes when the writer ran again.
        // For events under PIPE_BUF (~4KB on Linux), single
        // write_all to an O_APPEND fd is atomic at the kernel
        // level — gives same-process crash safety. Larger events
        // remain a torn-record hazard; see the NOTES "Known costs"
        // entry on torn-records-above-PIPE_BUF when one lands.
        //
        // Defensive prepend: if the file's last byte is not a
        // newline (a torn record left over from a prior crash),
        // start the buffer with `\n` so this new event doesn't
        // concatenate onto the un-terminated tail. The (round-5)
        // tailer would otherwise see one giant invalid line. The
        // tailer's parse-on-EOF below recovers the prior event
        // separately when its bytes ARE a complete JSON object.
        let needs_leading_newline = file_ends_unterminated(&f)?;
        let mut buf = Vec::with_capacity(
            event_payload_estimate(event, needs_leading_newline),
        );
        if needs_leading_newline {
            buf.push(b'\n');
        }
        serde_json::to_writer(&mut buf, event).map_err(io::Error::other)?;
        buf.push(b'\n');

        let mut f = f;
        f.write_all(&buf)?;
        f.flush()?;
        f.sync_all()?;
        Ok(())
    }
}

/// Cheap upper-bound for `Vec::with_capacity` — avoids growing
/// the buffer while building the payload. Conservative; over-
/// allocation is fine for the rare workflow_transition with a
/// large prompt.
fn event_payload_estimate(event: &Event, leading_newline: bool) -> usize {
    // Rough: prompt + reasonable overhead for keys + struct
    // fields. Falls back to a sane default if `args` doesn't
    // surface a prompt.
    let args_hint = event
        .args
        .get("prompt")
        .and_then(|v| v.as_str())
        .map(|s| s.len())
        .unwrap_or(0);
    256 + args_hint + if leading_newline { 1 } else { 0 } + 1
}

/// Read the file's last byte (without disturbing the O_APPEND
/// fd's position) and return `true` if it's not `\n` — i.e.,
/// the file ends with an unterminated record. Empty files
/// return `false` (no torn tail to guard against).
fn file_ends_unterminated(f: &std::fs::File) -> io::Result<bool> {
    use std::os::unix::fs::FileExt;
    let len = f.metadata()?.len();
    if len == 0 {
        return Ok(false);
    }
    let mut last = [0u8; 1];
    f.read_at(&mut last, len - 1)?;
    Ok(last[0] != b'\n')
}

// 10d-2c-1 review round-3 (F2): `validate_run_id` moved to
// `crate::workflow::run::validate_run_id` so EVERY filesystem-
// touching entry point (load_one, save, modify, try_modify,
// append_event) shares one validator. The validation must
// happen BEFORE any path is constructed from the run_id —
// otherwise a malformed id like `../../../etc/passwd` reaches
// the disk through `run::run_dir`.
//
// `append_event` calls the shared validator at the top, same
// as before.

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn with_temp_home<F: FnOnce()>(f: F) -> tempfile::TempDir {
        // Serialize HOME env mutation across the whole crate — cargo
        // runs tests from different modules in parallel, so a local
        // `static LOCK` would only protect this module's tests.
        let _guard = crate::test_support::env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let orig = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", tmp.path()); }
        f();
        if let Some(o) = orig {
            unsafe { std::env::set_var("HOME", o); }
        }
        tmp
    }

    #[test]
    fn reads_new_events_incrementally() {
        let _tmp = with_temp_home(|| {
            let run_id = "wf_evtest";
            let dir = run::run_dir(run_id);
            std::fs::create_dir_all(&dir).unwrap();
            let path = run::events_path(run_id);

            {
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .unwrap();
                writeln!(
                    f,
                    r#"{{"id":"a","ts":1.0,"run_id":"wf_evtest","role":"manager","tool":"workflow_transition","args":{{"to":"worker","prompt":"try again"}}}}"#
                )
                .unwrap();
            }

            let (events, offset) = read_new(run_id, 0);
            assert_eq!(events.len(), 1);
            match events[0].kind() {
                EventKind::Transition { to, prompt } => {
                    assert_eq!(to, "worker");
                    assert_eq!(prompt, "try again");
                }
                _ => panic!("expected transition"),
            }
            assert!(offset > 0);

            // Append a done event and confirm only the new one comes back.
            {
                let mut f = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap();
                writeln!(
                    f,
                    r#"{{"id":"b","ts":2.0,"run_id":"wf_evtest","role":"manager","tool":"workflow_done","args":{{"reason":"ok"}}}}"#
                )
                .unwrap();
            }

            let (events2, offset2) = read_new(run_id, offset);
            assert_eq!(events2.len(), 1);
            assert!(offset2 > offset);
            match events2[0].kind() {
                EventKind::Done { reason } => assert_eq!(reason, "ok"),
                _ => panic!("expected done"),
            }
        });
    }

    #[test]
    fn absent_file_is_noop() {
        let _tmp = with_temp_home(|| {
            let (events, offset) = read_new("wf_nonexistent", 0);
            assert!(events.is_empty());
            assert_eq!(offset, 0);
        });
    }

    /// 10d-2a round 5: a poll that hits EOF mid-record must NOT
    /// advance the offset past the partial bytes — otherwise when
    /// the writer completes the record, the now-finished event is
    /// permanently skipped. The tailer leaves unterminated tails
    /// in the offset window so the next poll re-reads from the
    /// same offset (idempotent — the writer never rewrites bytes
    /// it already emitted).
    #[test]
    fn read_new_preserves_offset_on_unterminated_partial_line() {
        let _tmp = with_temp_home(|| {
            let run_id = "wf_partial";
            let dir = run::run_dir(run_id);
            std::fs::create_dir_all(&dir).unwrap();
            let path = run::events_path(run_id);

            // Seed a partial line (no newline) — simulates the
            // tailer racing the writer mid-append.
            std::fs::write(
                &path,
                br#"{"id":"partial","ts":1.0,"run_id":"wf_partial","role":"worker","tool":"workflow_transition","args":{"to":"reviewer","prompt":"truncat"#,
            )
            .unwrap();

            let (events, offset) = read_new(run_id, 0);
            assert!(
                events.is_empty(),
                "torn record must not yield an event, got {:?}",
                events,
            );
            assert_eq!(
                offset, 0,
                "offset must NOT advance past unterminated bytes (would lose the \
                 event when the writer completes the record); got offset = {}",
                offset,
            );

            // Now the "writer" completes the record. Re-poll from
            // the saved offset — the full event must come through.
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(br#"ed"}}"#).unwrap();
            f.write_all(b"\n").unwrap();
            drop(f);

            let (events2, offset2) = read_new(run_id, offset);
            assert_eq!(events2.len(), 1, "completed record must come through");
            assert_eq!(events2[0].id, "partial");
            match events2[0].kind() {
                EventKind::Transition { to, prompt } => {
                    assert_eq!(to, "reviewer");
                    assert_eq!(prompt, "truncated");
                }
                _ => panic!("expected transition"),
            }
            assert!(offset2 > offset, "offset advances past completed record");
        });
    }

    /// 10d-2a round 5: a mix of complete records followed by a
    /// torn tail — the tailer yields the complete records, then
    /// stops at the torn line WITHOUT advancing past it. Next
    /// poll picks up the now-completed record.
    #[test]
    fn read_new_yields_complete_records_and_holds_partial_tail() {
        let _tmp = with_temp_home(|| {
            let run_id = "wf_mixed";
            let dir = run::run_dir(run_id);
            std::fs::create_dir_all(&dir).unwrap();
            let path = run::events_path(run_id);

            // Two complete records + a partial third.
            std::fs::write(
                &path,
                br#"{"id":"a","ts":1.0,"run_id":"wf_mixed","role":"worker","tool":"workflow_transition","args":{"to":"reviewer","prompt":"first"}}
{"id":"b","ts":2.0,"run_id":"wf_mixed","role":"worker","tool":"workflow_transition","args":{"to":"manager","prompt":"second"}}
{"id":"c","ts":3.0,"run_id":"wf_mixed","role":"worker","tool":"workflow_transition","args":{"to":"worker","prompt":"par"#,
            )
            .unwrap();

            // Compute the offset where the partial line begins —
            // it's where the second '\n' lands in the file.
            let bytes = std::fs::read(&path).unwrap();
            let newline_positions: Vec<usize> = bytes
                .iter()
                .enumerate()
                .filter_map(|(i, &b)| if b == b'\n' { Some(i) } else { None })
                .collect();
            assert_eq!(newline_positions.len(), 2, "expect two newlines in seed");
            let partial_start = (newline_positions[1] + 1) as u64;

            let (events, offset) = read_new(run_id, 0);
            assert_eq!(events.len(), 2, "two complete records must yield");
            assert_eq!(events[0].id, "a");
            assert_eq!(events[1].id, "b");
            assert_eq!(
                offset, partial_start,
                "offset advances to the start of the partial third line, \
                 not past its torn bytes",
            );

            // Writer completes record c. Re-poll from the saved
            // offset — only c comes through (a and b are not
            // re-yielded).
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(br#"tial"}}"#).unwrap();
            f.write_all(b"\n").unwrap();
            drop(f);

            let (events2, offset2) = read_new(run_id, offset);
            assert_eq!(events2.len(), 1);
            assert_eq!(events2[0].id, "c");
            match events2[0].kind() {
                EventKind::Transition { to, prompt } => {
                    assert_eq!(to, "worker");
                    assert_eq!(prompt, "partial");
                }
                _ => panic!("expected transition"),
            }
            assert!(offset2 > offset);
        });
    }

    /// 10d-2a round 6: a daemon crash between the writer's JSON
    /// write and its newline write (the pre-fix two-write
    /// sequence) leaves a COMPLETE JSON object on disk without a
    /// trailing `\n`. The tailer's parse-on-EOF fallback recovers
    /// the event rather than holding the offset forever — the
    /// round-5 hold-offset behavior was correct for in-flight
    /// writes but would never advance past a crash leftover.
    ///
    /// The round-6 single-buffer write_all eliminates this
    /// failure mode for fresh writes, but parse-on-EOF still
    /// matters for legacy files (the round-5 fix was deployed
    /// before this round-6 writer change).
    #[test]
    fn read_new_recovers_complete_json_without_trailing_newline() {
        let _tmp = with_temp_home(|| {
            let run_id = "wf_crash_leftover";
            let dir = run::run_dir(run_id);
            std::fs::create_dir_all(&dir).unwrap();
            let path = run::events_path(run_id);

            // A complete JSON object, no trailing newline —
            // simulates a daemon crash between the legacy two
            // writes.
            std::fs::write(
                &path,
                br#"{"id":"crash","ts":1.0,"run_id":"wf_crash_leftover","role":"worker","tool":"workflow_done","args":{"reason":"crashed"}}"#,
            )
            .unwrap();
            let file_len = std::fs::metadata(&path).unwrap().len();

            let (events, offset) = read_new(run_id, 0);
            assert_eq!(
                events.len(),
                1,
                "complete JSON without trailing newline must be recovered",
            );
            assert_eq!(events[0].id, "crash");
            match events[0].kind() {
                EventKind::Done { reason } => assert_eq!(reason, "crashed"),
                _ => panic!("expected done"),
            }
            assert_eq!(
                offset, file_len,
                "offset advances past the recovered event so it isn't re-yielded",
            );
        });
    }

    /// 10d-2a round 6: the writer's defensive prepend prevents
    /// concatenation onto an un-terminated tail. Setup: write a
    /// partial line (no newline) directly, then call
    /// `append_event` for a new event. The resulting file must
    /// have the new event on its own line — NOT concatenated
    /// onto the partial tail.
    #[test]
    fn append_event_prepends_newline_when_file_ends_unterminated() {
        let _tmp = with_temp_home(|| {
            let run_id = "wf_prepend";
            let dir = run::run_dir(run_id);
            std::fs::create_dir_all(&dir).unwrap();
            let path = run::events_path(run_id);

            // Seed an un-terminated tail (simulating a torn record
            // from a prior crash).
            std::fs::write(&path, b"torn-leftover-no-newline").unwrap();

            let ev = make_event(
                run_id,
                "fresh-1",
                1.0,
                "workflow_done",
                serde_json::json!({"reason": "ok"}),
            );
            WorkflowEventsWriter::append_event(&ev).expect("append ok");

            // The new event's bytes must not be concatenated onto
            // the torn tail. The file should look like:
            //   `torn-leftover-no-newline\n<JSON for fresh-1>\n`
            let contents = std::fs::read_to_string(&path).unwrap();
            assert!(
                contents.starts_with("torn-leftover-no-newline\n"),
                "writer must insert a newline before the new event's bytes; \
                 got contents = {:?}",
                contents,
            );
            // Lines: ["torn-leftover-no-newline", "<json>", ""]
            // (split by "\n" produces a trailing empty from the
            // final newline).
            let lines: Vec<&str> = contents.split('\n').collect();
            assert_eq!(lines.len(), 3);
            assert_eq!(lines[0], "torn-leftover-no-newline");
            let parsed: Event = serde_json::from_str(lines[1])
                .expect("second line is the new event's JSON, parseable on its own");
            assert_eq!(parsed.id, "fresh-1");
            assert!(lines[2].is_empty(), "trailing newline");
        });
    }

    /// 10d-2a round 6: when the file already ends with `\n`,
    /// the writer does NOT spuriously add a leading newline.
    /// Regression sentinel against an over-tightening of the
    /// defensive prepend.
    #[test]
    fn append_event_does_not_prepend_when_file_ends_with_newline() {
        let _tmp = with_temp_home(|| {
            let run_id = "wf_no_prepend";
            let dir = run::run_dir(run_id);
            std::fs::create_dir_all(&dir).unwrap();
            let path = run::events_path(run_id);

            // Seed a complete (newline-terminated) record.
            std::fs::write(
                &path,
                br#"{"id":"prior","ts":1.0,"run_id":"wf_no_prepend","role":"worker","tool":"workflow_done","args":{"reason":"prior"}}
"#,
            )
            .unwrap();

            let ev = make_event(
                run_id,
                "next",
                2.0,
                "workflow_done",
                serde_json::json!({"reason": "next"}),
            );
            WorkflowEventsWriter::append_event(&ev).expect("append ok");

            // Should be exactly two lines + trailing newline,
            // with no spurious blank line in between.
            let contents = std::fs::read_to_string(&path).unwrap();
            let lines: Vec<&str> = contents.split('\n').collect();
            assert_eq!(
                lines.len(),
                3,
                "two records + trailing empty (from final newline), got: {:?}",
                lines,
            );
            assert!(lines[0].contains("\"prior\""));
            assert!(lines[1].contains("\"next\""));
            assert!(lines[2].is_empty());
            // No blank line means lines[1] doesn't start empty —
            // the writer didn't spuriously prepend `\n`.
            assert!(!lines[1].is_empty());
        });
    }

    /// 10d-2a round 6: even concurrent appends from the writer
    /// itself (no torn tail) produce one well-formed line per
    /// event. Pin against a regression that would split the
    /// JSON and newline across two writes again.
    #[test]
    fn append_event_writes_one_well_formed_line_per_call() {
        let _tmp = with_temp_home(|| {
            let run_id = "wf_single_line";
            for i in 0..5 {
                let ev = make_event(
                    run_id,
                    &format!("evt-{}", i),
                    i as f64,
                    "workflow_transition",
                    serde_json::json!({"to": "next", "prompt": "p"}),
                );
                WorkflowEventsWriter::append_event(&ev).expect("append ok");
            }
            let path = run::events_path(run_id);
            let contents = std::fs::read_to_string(&path).unwrap();
            // Exactly 5 newlines, no blank lines, every line
            // parses as Event.
            assert!(
                contents.ends_with('\n'),
                "file ends with newline (no torn record): {:?}",
                contents,
            );
            let lines: Vec<&str> = contents.lines().collect();
            assert_eq!(lines.len(), 5);
            for (i, line) in lines.iter().enumerate() {
                let ev: Event = serde_json::from_str(line)
                    .unwrap_or_else(|e| panic!("line {} parse failed: {:?} ({})", i, e, line));
                assert_eq!(ev.id, format!("evt-{}", i));
            }
        });
    }

    fn make_event(run_id: &str, id: &str, ts: f64, tool: &str, args: serde_json::Value) -> Event {
        Event {
            id: id.to_string(),
            ts,
            run_id: run_id.to_string(),
            role: "worker".to_string(),
            tool: tool.to_string(),
            args,
            source: String::new(),
            from_role: None,
            iteration: 0,
        }
    }

    /// 10d-2a: `WorkflowEventsWriter::append_event` creates the run
    /// directory, writes a single valid-JSON line per event, and the
    /// existing tailer ([`read_new`]) reads what was written.
    #[test]
    fn writer_creates_dir_and_appends_well_formed_jsonl() {
        let _tmp = with_temp_home(|| {
            let run_id = "wf_writer_basic";
            // Pre-state: directory does not exist.
            let dir = run::run_dir(run_id);
            assert!(!dir.exists(), "precondition: run dir absent");

            let ev1 = make_event(
                run_id,
                "evt-1",
                1.0,
                "workflow_transition",
                serde_json::json!({"to": "reviewer", "prompt": "diff lgtm?"}),
            );
            let ev2 = make_event(
                run_id,
                "evt-2",
                2.0,
                "workflow_done",
                serde_json::json!({"reason": "approved"}),
            );

            WorkflowEventsWriter::append_event(&ev1).expect("first append ok");
            WorkflowEventsWriter::append_event(&ev2).expect("second append ok");

            assert!(dir.exists(), "run dir created on first append");
            let path = run::events_path(run_id);
            assert!(path.exists(), "events.jsonl created");

            // Read raw lines and check shape.
            let contents = std::fs::read_to_string(&path).expect("read events.jsonl");
            let lines: Vec<&str> = contents.lines().collect();
            assert_eq!(lines.len(), 2, "exactly two lines");
            for line in &lines {
                serde_json::from_str::<Event>(line).expect("each line parses as Event");
            }

            // Existing tailer reads what we wrote — wire shape unchanged.
            let (events, offset) = read_new(run_id, 0);
            assert_eq!(events.len(), 2);
            assert!(offset > 0);
            assert!(matches!(
                events[0].kind(),
                EventKind::Transition { to, prompt }
                    if to == "reviewer" && prompt == "diff lgtm?",
            ));
            assert!(matches!(
                events[1].kind(),
                EventKind::Done { reason } if reason == "approved",
            ));
        });
    }

    /// 10d-2a: concurrent appends from many threads to the same
    /// run produce well-formed JSONL — every byte sequence belongs
    /// to exactly one event, no torn lines, no interleaved bytes.
    /// Per-run mutex is the contract.
    #[test]
    fn writer_concurrent_appends_well_formed_jsonl() {
        let _tmp = with_temp_home(|| {
            let run_id = "wf_writer_concurrent";
            const THREADS: usize = 8;
            const EVENTS_PER_THREAD: usize = 25;
            // Pick a prompt size larger than PIPE_BUF (4096) so
            // we exercise the "single write may not be atomic"
            // failure mode the per-run lock guards against. If
            // the lock is broken, expect to see interleaving here.
            let payload: String = "x".repeat(5000);

            let mut handles = Vec::new();
            for t in 0..THREADS {
                let payload = payload.clone();
                let run_id_owned = run_id.to_string();
                handles.push(std::thread::spawn(move || {
                    for i in 0..EVENTS_PER_THREAD {
                        let ev = Event {
                            id: format!("t{}-{}", t, i),
                            ts: (t * EVENTS_PER_THREAD + i) as f64,
                            run_id: run_id_owned.clone(),
                            role: format!("role-{}", t),
                            tool: "workflow_transition".to_string(),
                            args: serde_json::json!({"to": "next", "prompt": payload}),
                            source: String::new(),
                            from_role: None,
                            iteration: 0,
                        };
                        WorkflowEventsWriter::append_event(&ev).expect("append ok");
                    }
                }));
            }
            for h in handles {
                h.join().expect("thread joined");
            }

            let path = run::events_path(run_id);
            let contents = std::fs::read_to_string(&path).expect("read");
            let lines: Vec<&str> = contents.lines().collect();
            assert_eq!(
                lines.len(),
                THREADS * EVENTS_PER_THREAD,
                "every event lands on its own line",
            );
            // Every line must parse — no torn JSON.
            let mut ids = std::collections::HashSet::new();
            for line in &lines {
                let ev: Event = serde_json::from_str(line)
                    .unwrap_or_else(|e| panic!("torn or invalid line: {}: {:?}", e, line));
                assert!(
                    ids.insert(ev.id.clone()),
                    "duplicate event id (suggests interleaving or write loss): {}",
                    ev.id,
                );
            }
            assert_eq!(ids.len(), THREADS * EVENTS_PER_THREAD);
        });
    }

    /// 10d-2a security: fresh-directory creation produces the run
    /// directory at mode `0o700` and `events.jsonl` at `0o600`.
    /// Pre-fix the inherited umask (commonly `0o002`) would leave
    /// directory at `0o775` and file at `0o664` — local group users
    /// could read prompts or forge transitions on the workflow
    /// control plane.
    #[test]
    fn writer_creates_dir_at_0700_and_file_at_0600() {
        use std::os::unix::fs::PermissionsExt;
        let _tmp = with_temp_home(|| {
            // Force a permissive umask so we'd see the bug if the
            // fix didn't override it.
            let saved_umask = unsafe { libc::umask(0o002) };
            let run_id = "wf_perms_fresh";

            let ev = make_event(
                run_id,
                "evt-1",
                1.0,
                "workflow_transition",
                serde_json::json!({"to": "reviewer", "prompt": "secret"}),
            );
            WorkflowEventsWriter::append_event(&ev).expect("append ok");
            unsafe { libc::umask(saved_umask); }

            let dir = run::run_dir(run_id);
            let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                dir_mode, 0o700,
                "run dir must be 0o700, got 0o{:o}",
                dir_mode,
            );
            let path = run::events_path(run_id);
            let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                file_mode, 0o600,
                "events.jsonl must be 0o600, got 0o{:o}",
                file_mode,
            );
        });
    }

    /// 10d-2a security: a pre-existing run directory or
    /// `events.jsonl` from a less-strict writer (e.g. TUI-side
    /// `run::save` creating `state.json` first under default
    /// umask) gets tightened on first daemon append.
    #[test]
    fn writer_tightens_existing_loose_perms_on_append() {
        use std::os::unix::fs::PermissionsExt;
        let _tmp = with_temp_home(|| {
            let run_id = "wf_perms_drift";
            let dir = run::run_dir(run_id);
            std::fs::create_dir_all(&dir).expect("create dir");
            // Simulate a permissive pre-existing dir (e.g. TUI
            // wrote state.json under umask 0o002).
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o775))
                .expect("set loose dir perms");
            let path = run::events_path(run_id);
            // Pre-seed events.jsonl with one entry and loose perms
            // — exercise the file-side tightening on append.
            std::fs::write(&path, b"")
                .expect("seed empty events.jsonl");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o664))
                .expect("set loose file perms");

            let ev = make_event(
                run_id,
                "evt-1",
                1.0,
                "workflow_done",
                serde_json::json!({"reason": "ok"}),
            );
            WorkflowEventsWriter::append_event(&ev).expect("append ok");

            let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                dir_mode, 0o700,
                "pre-existing loose dir must be tightened to 0o700, got 0o{:o}",
                dir_mode,
            );
            let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                file_mode, 0o600,
                "pre-existing loose file must be tightened to 0o600, got 0o{:o}",
                file_mode,
            );
        });
    }

    /// 10d-2a round 2 defense-in-depth #1: parent
    /// `~/.cm/workflow-runs` directory is tightened to `0o700`
    /// on first daemon append, even if it pre-existed with
    /// looser perms. Closes the pre-seed-symlink-inside-parent
    /// attack class — if the parent is owner-only, no other
    /// user can place anything in it.
    #[test]
    fn writer_tightens_parent_workflow_runs_dir_to_0700() {
        use std::os::unix::fs::PermissionsExt;
        let _tmp = with_temp_home(|| {
            let parent = run::runs_dir();
            std::fs::create_dir_all(&parent).expect("create parent");
            std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755))
                .expect("set loose parent perms");

            let ev = make_event(
                "wf_parent_drift",
                "evt-1",
                1.0,
                "workflow_transition",
                serde_json::json!({"to": "reviewer", "prompt": "x"}),
            );
            WorkflowEventsWriter::append_event(&ev).expect("append ok");

            let parent_mode = std::fs::metadata(&parent).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                parent_mode, 0o700,
                "parent workflow-runs dir must be tightened to 0o700, got 0o{:o}",
                parent_mode,
            );
        });
    }

    /// 10d-2a round 2 defense-in-depth #2: containment-unsafe
    /// run_ids are rejected with `InvalidInput` BEFORE any
    /// filesystem operation. No file or directory is created
    /// for a bad run_id.
    #[test]
    fn writer_rejects_unsafe_run_ids() {
        let _tmp = with_temp_home(|| {
            // The validator should reject these regardless of
            // filesystem state, but as a belt-and-suspenders
            // check we also assert no file lands outside the
            // expected path.
            let bad = [
                "",
                ".",
                "..",
                "../etc",
                "foo/bar",
                "with\0null",
                "wf with space",
                "wf;rm",
                "wf$(echo)",
                // Way too long.
                &"x".repeat(200),
            ];
            for id in bad {
                let ev = Event {
                    id: "evt-x".to_string(),
                    ts: 1.0,
                    run_id: id.to_string(),
                    role: "worker".to_string(),
                    tool: "workflow_done".to_string(),
                    args: serde_json::json!({"reason": "ok"}),
                    source: String::new(),
                    from_role: None,
                    iteration: 0,
                };
                let result = WorkflowEventsWriter::append_event(&ev);
                assert!(
                    result.is_err(),
                    "unsafe run_id {:?} must be rejected",
                    id,
                );
                let err = result.unwrap_err();
                assert_eq!(
                    err.kind(),
                    io::ErrorKind::InvalidInput,
                    "unsafe run_id {:?} must error with InvalidInput, got: {:?}",
                    id,
                    err.kind(),
                );
            }
            // Nothing should have been created under the parent.
            let parent = run::runs_dir();
            if parent.exists() {
                let children: Vec<_> = std::fs::read_dir(&parent)
                    .unwrap()
                    .filter_map(|e| e.ok())
                    .collect();
                assert!(
                    children.is_empty(),
                    "no run dirs should be created for unsafe ids, got: {:?}",
                    children.iter().map(|e| e.path()).collect::<Vec<_>>(),
                );
            }
        });
    }

    /// 10d-2a round 2 defense-in-depth #2: well-formed
    /// `wf_<base36>` and uuid-hex run_ids are accepted. Pinned
    /// alongside the rejection test so a future tightening of
    /// the validator (e.g. requiring a `wf_` prefix) doesn't
    /// silently break MCP's `uuid.uuid4().hex` callers.
    #[test]
    fn writer_accepts_well_formed_run_ids() {
        let _tmp = with_temp_home(|| {
            let good = [
                "wf_abc123",
                "wf_ABC-123_xyz",
                // uuid.uuid4().hex shape from MCP server.
                "0123456789abcdef0123456789abcdef",
                // Single char (edge case at the lower bound).
                "a",
            ];
            for id in good {
                let ev = Event {
                    id: format!("evt-{}", id),
                    ts: 1.0,
                    run_id: id.to_string(),
                    role: "worker".to_string(),
                    tool: "workflow_done".to_string(),
                    args: serde_json::json!({"reason": "ok"}),
                    source: String::new(),
                    from_role: None,
                    iteration: 0,
                };
                WorkflowEventsWriter::append_event(&ev)
                    .unwrap_or_else(|e| panic!("good run_id {:?} must accept, got: {:?}", id, e));
            }
        });
    }

    /// 10d-2a round 2 defense-in-depth #3: `O_NOFOLLOW` refuses
    /// to open `events.jsonl` if it's a symlink. The pre-fix
    /// path would have followed the symlink, appending to (and
    /// chmoding) an attacker-controlled target.
    #[test]
    fn writer_rejects_events_jsonl_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let _tmp = with_temp_home(|| {
            let run_id = "wf_symlink";
            let dir = run::run_dir(run_id);
            std::fs::create_dir_all(&dir).expect("create dir");

            // Create a symlink target outside the run dir and
            // point events.jsonl at it.
            let target_dir = tempfile::tempdir().expect("target dir");
            let target = target_dir.path().join("attacker-target");
            std::fs::write(&target, b"original\n").expect("seed target");
            let target_mode_before =
                std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;

            let events_path = run::events_path(run_id);
            std::os::unix::fs::symlink(&target, &events_path).expect("symlink");

            let ev = make_event(
                run_id,
                "evt-1",
                1.0,
                "workflow_transition",
                serde_json::json!({"to": "reviewer", "prompt": "secret"}),
            );
            let result = WorkflowEventsWriter::append_event(&ev);
            assert!(
                result.is_err(),
                "symlinked events.jsonl must be refused, instead the writer returned Ok",
            );

            // Target's content and perms must be untouched.
            let target_after = std::fs::read_to_string(&target).expect("read target");
            assert_eq!(
                target_after, "original\n",
                "symlink target must not be appended to",
            );
            let target_mode_after =
                std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                target_mode_before, target_mode_after,
                "symlink target's perms must not change (no chmod-by-path on the symlink)",
            );
        });
    }

    /// 10d-2a round 2 defense-in-depth #4: chmod targets the
    /// open file descriptor (`fchmod`), not the path. End-state
    /// check: after a successful append, the on-disk file mode
    /// is exactly `0o600`. This test mainly documents the
    /// intent — proving `fchmod` was used rather than
    /// `chmod(path)` is not directly assertable from outside,
    /// but the symlink-rejection test above plus this mode
    /// check together pin the round-2 behavior.
    #[test]
    fn writer_chmod_lands_at_0o600_on_open_fd() {
        use std::os::unix::fs::PermissionsExt;
        let _tmp = with_temp_home(|| {
            let run_id = "wf_fchmod";
            let dir = run::run_dir(run_id);
            std::fs::create_dir_all(&dir).expect("create dir");
            let path = run::events_path(run_id);
            // Pre-seed an `events.jsonl` (regular file, not symlink)
            // with permissive perms — exercises the existing-file
            // tightening path through fchmod.
            std::fs::write(&path, b"").expect("seed file");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o664))
                .expect("loose perms");

            let ev = make_event(
                run_id,
                "evt-1",
                1.0,
                "workflow_done",
                serde_json::json!({"reason": "ok"}),
            );
            WorkflowEventsWriter::append_event(&ev).expect("append ok");

            let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                file_mode, 0o600,
                "fchmod must land at 0o600, got 0o{:o}",
                file_mode,
            );
        });
    }

    /// 10d-2a round 3 defense-in-depth #5: a symlink at the
    /// run-id-named component itself is refused. Pre-fix
    /// `ensure_dot_cm_subdir` would follow it (via `metadata` +
    /// `set_permissions`) into attacker-controlled territory.
    /// The pre-flight `verify_no_symlinks_in_path` walk catches
    /// it.
    #[test]
    fn writer_rejects_run_id_directory_symlink() {
        let _tmp = with_temp_home(|| {
            let runs_dir = run::runs_dir();
            std::fs::create_dir_all(&runs_dir).expect("create parent");
            // Symlink "bogus" → an attacker-controlled tempdir.
            let attacker_dir = tempfile::tempdir().expect("attacker dir");
            let run_id = "wf_symlink_dir";
            let link_path = runs_dir.join(run_id);
            std::os::unix::fs::symlink(attacker_dir.path(), &link_path)
                .expect("symlink the run-id dir");

            let attacker_target_before: Vec<_> = std::fs::read_dir(attacker_dir.path())
                .unwrap()
                .filter_map(|e| e.ok().map(|e| e.path()))
                .collect();

            let ev = make_event(
                run_id,
                "evt-1",
                1.0,
                "workflow_transition",
                serde_json::json!({"to": "reviewer", "prompt": "secret"}),
            );
            let result = WorkflowEventsWriter::append_event(&ev);
            assert!(
                result.is_err(),
                "symlinked run-id dir must be refused, instead returned Ok",
            );
            assert_eq!(
                result.unwrap_err().kind(),
                io::ErrorKind::PermissionDenied,
                "symlink at run-id dir must error with PermissionDenied",
            );
            // Attacker-controlled target must be untouched — no
            // events.jsonl written there.
            let attacker_target_after: Vec<_> = std::fs::read_dir(attacker_dir.path())
                .unwrap()
                .filter_map(|e| e.ok().map(|e| e.path()))
                .collect();
            assert_eq!(
                attacker_target_before, attacker_target_after,
                "attacker-controlled target must not have been written into",
            );
        });
    }

    /// 10d-2a round 3 defense-in-depth #5: a symlink at an
    /// ANCESTOR of the run dir (e.g. `~/.cm/workflow-runs`
    /// itself replaced with a symlink) is also refused. The
    /// walk must catch symlinks anywhere between `~/.cm` and
    /// the target, not just at the last component.
    #[test]
    fn writer_rejects_ancestor_symlink_above_run_dir() {
        let _tmp = with_temp_home(|| {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap();
            let dot_cm = home.join(".cm");
            std::fs::create_dir_all(&dot_cm).expect("create ~/.cm");
            // Replace what would be `~/.cm/workflow-runs` with a
            // symlink to an attacker-controlled tempdir.
            let attacker_dir = tempfile::tempdir().expect("attacker dir");
            let runs_dir_path = run::runs_dir();
            // Sanity: it should not exist yet.
            assert!(!runs_dir_path.exists());
            std::os::unix::fs::symlink(attacker_dir.path(), &runs_dir_path)
                .expect("symlink the workflow-runs ancestor");

            let ev = make_event(
                "wf_anc",
                "evt-1",
                1.0,
                "workflow_done",
                serde_json::json!({"reason": "ok"}),
            );
            let result = WorkflowEventsWriter::append_event(&ev);
            assert!(
                result.is_err(),
                "ancestor symlink at workflow-runs/ must be refused",
            );
            assert_eq!(
                result.unwrap_err().kind(),
                io::ErrorKind::PermissionDenied,
            );
            // Attacker tempdir must be empty (no run dir created
            // under it, no events.jsonl written).
            let after: Vec<_> = std::fs::read_dir(attacker_dir.path())
                .unwrap()
                .filter_map(|e| e.ok().map(|e| e.path()))
                .collect();
            assert!(
                after.is_empty(),
                "attacker tempdir must remain empty, got: {:?}",
                after,
            );
        });
    }

    /// 10d-2a round 3 defense-in-depth #5: real (non-symlink)
    /// ancestors traverse normally. Belt-and-suspenders against
    /// over-tightening that breaks the happy path.
    #[test]
    fn writer_traverses_real_ancestors_normally() {
        let _tmp = with_temp_home(|| {
            let run_id = "wf_real_ancestors";
            let ev = make_event(
                run_id,
                "evt-1",
                1.0,
                "workflow_transition",
                serde_json::json!({"to": "next", "prompt": "p"}),
            );
            WorkflowEventsWriter::append_event(&ev).expect("append ok");
            // Sanity: file landed where expected, no symlink in
            // the chain, content present.
            let path = run::events_path(run_id);
            assert!(path.exists());
            let contents = std::fs::read_to_string(&path).unwrap();
            assert!(contents.contains("evt-1"));
            // Walk the chain ourselves to confirm no component
            // accidentally became a symlink (regression sentinel).
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap();
            for component in [
                home.join(".cm"),
                home.join(".cm/workflow-runs"),
                home.join(".cm/workflow-runs").join(run_id),
            ] {
                let meta = std::fs::symlink_metadata(&component)
                    .unwrap_or_else(|e| panic!("symlink_metadata({:?}): {}", component, e));
                assert!(
                    !meta.file_type().is_symlink(),
                    "component {:?} unexpectedly became a symlink",
                    component,
                );
            }
        });
    }

    /// 10d-2a round 4: with `HOME` unset, `runs_dir()` falls back
    /// to `/tmp/.cm/workflow-runs` (matching the daemon socket bind
    /// path's same fallback). The writer's symlink-rejection walk
    /// must use the SAME resolved root — `path::dot_cm_dir()` — so
    /// a no-HOME container env where the rest of the daemon works
    /// doesn't break every workflow append.
    ///
    /// Pre-fix the writer called `var_os("HOME").ok_or(NotFound)?`
    /// and surfaced `HOME not set` as an `io::Error` to the agent;
    /// post-fix the append succeeds at the `/tmp/.cm/...` fallback.
    #[test]
    fn writer_succeeds_with_home_unset_via_tmp_fallback() {
        // Same env-mutation discipline as `with_temp_home`: hold
        // the crate-wide env lock, save HOME, scrub it, run, then
        // restore. Don't piggyback on `with_temp_home` because that
        // SETS HOME — we want it unset.
        let _guard = crate::test_support::env_lock();
        let orig = std::env::var_os("HOME");
        unsafe { std::env::remove_var("HOME"); }

        // Use a unique run_id so this test doesn't collide with
        // anything pre-existing at /tmp/.cm/workflow-runs/. Tag
        // with PID + nanos to be safe.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let run_id = format!("wf_no_home_test_{}_{}", std::process::id(), nanos);

        let ev = make_event(
            &run_id,
            "evt-1",
            1.0,
            "workflow_transition",
            serde_json::json!({"to": "reviewer", "prompt": "container fallback"}),
        );

        let result = WorkflowEventsWriter::append_event(&ev);

        // The trusted root must resolve to /tmp/.cm, the run dir to
        // /tmp/.cm/workflow-runs/<run_id>, and the file must land
        // at /tmp/.cm/workflow-runs/<run_id>/events.jsonl.
        let expected_root = std::path::PathBuf::from("/tmp/.cm");
        let expected_runs_dir = expected_root.join("workflow-runs");
        let expected_run_dir = expected_runs_dir.join(&run_id);
        let expected_events = expected_run_dir.join("events.jsonl");

        // Inspect runs_dir BEFORE restoring HOME (post-restore it
        // would re-derive from the real HOME and not match).
        let resolved_runs_dir = run::runs_dir();
        let resolved_trusted_root = crate::path::dot_cm_dir();

        // Clean up the per-run dir we just wrote, regardless of
        // the assert outcome — don't leak files into /tmp across
        // test runs.
        let cleanup = || {
            let _ = std::fs::remove_file(&expected_events);
            let _ = std::fs::remove_dir(&expected_run_dir);
            // Don't try to remove the parent /tmp/.cm/workflow-runs
            // or /tmp/.cm itself — other tests / other processes
            // may legitimately use them on this machine.
        };

        // Restore HOME before any assertion so a failure doesn't
        // leave the env lock held with a wedged HOME for follow-on
        // tests on the same thread.
        if let Some(o) = orig {
            unsafe { std::env::set_var("HOME", o); }
        }

        match result {
            Ok(()) => {
                assert_eq!(
                    resolved_runs_dir, expected_runs_dir,
                    "runs_dir should fall back to /tmp/.cm/workflow-runs when HOME unset",
                );
                assert_eq!(
                    resolved_trusted_root, expected_root,
                    "dot_cm_dir trusted root should match runs_dir's parent",
                );
                assert!(
                    expected_events.exists(),
                    "events.jsonl should exist at {:?}",
                    expected_events,
                );
                let contents = std::fs::read_to_string(&expected_events)
                    .expect("read /tmp/.cm/... events.jsonl");
                assert!(
                    contents.contains("container fallback"),
                    "event content present at fallback path",
                );
                cleanup();
            }
            Err(e) => {
                cleanup();
                panic!(
                    "append_event must succeed via /tmp fallback when HOME unset, \
                     instead got: {:?}",
                    e,
                );
            }
        }
    }

    /// 10d-2a: the daemon's `WorkflowRun` round-trips through JSON
    /// using the existing `state.json` shape — verified by writing
    /// a daemon-side run to its on-disk `state.json` via
    /// `run::save` and reading it back via `run::load_one`.
    /// Confirms the daemon and TUI share the same on-disk format,
    /// so when 10d-2c flips ownership the daemon can read runs
    /// the TUI persisted (and vice versa during the transitional
    /// state).
    #[test]
    fn workflow_run_state_round_trips_through_state_json() {
        let _tmp = with_temp_home(|| {
            let mut role_sessions = std::collections::BTreeMap::new();
            role_sessions.insert(
                "worker".to_string(),
                run::RoleBinding {
                    session_label: "claude".into(),
                    current_session_id: Some("sid-1".into()),
                },
            );
            role_sessions.insert(
                "reviewer".to_string(),
                run::RoleBinding {
                    session_label: "reviewer".into(),
                    current_session_id: None,
                },
            );
            let mut role_baselines = std::collections::BTreeMap::new();
            role_baselines.insert(
                "worker".to_string(),
                run::MessageBaseline { user_count: 3, assistant_count: 5 },
            );
            let mut role_plans = std::collections::BTreeMap::new();
            role_plans.insert("worker".to_string(), "plan: do X then Y".to_string());

            let original = run::WorkflowRun::new(
                "wf_round_trip".to_string(),
                "feedback".to_string(),
                "/tmp/repo".to_string(),
                role_sessions,
                "worker".to_string(),
                role_baselines,
                Some("ship the thing".to_string()),
                role_plans,
            );

            run::save(&original).expect("save state.json ok");
            let recovered = run::load_one("wf_round_trip").expect("load_one returns Some");

            // Spot-check key fields the 10d-2 driver will read.
            assert_eq!(recovered.run_id, original.run_id);
            assert_eq!(recovered.workflow_name, original.workflow_name);
            assert_eq!(recovered.task_key, original.task_key);
            assert_eq!(recovered.active_role, original.active_role);
            assert_eq!(recovered.iteration, original.iteration);
            assert_eq!(recovered.history.len(), original.history.len());
            assert_eq!(recovered.role_sessions.len(), original.role_sessions.len());
            assert_eq!(recovered.role_baselines.len(), original.role_baselines.len());
            assert_eq!(recovered.goal, original.goal);
            assert_eq!(recovered.role_plans, original.role_plans);
            assert!(matches!(recovered.status, run::RunStatus::Running));
            assert!(!recovered.paused);
        });
    }
}
