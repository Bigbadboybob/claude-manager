//! Per-session memory-cap watcher thread. One thread per capped
//! session; reacts to `memory.events` `high` counter increments by
//! picking the largest non-agent PID in the cgroup and SIGKILLing it.
//! See DESIGN_MEMORY_CAP.md § Components / 3. Watcher thread.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

/// One memory-kill event, emitted by a watcher thread to the main
/// app loop. Drained by `App::drain_memory_kill_events` each tick.
#[derive(Clone, Debug)]
pub enum MemoryKillEvent {
    /// The watcher killed a tool subprocess that was driving the
    /// session over the soft cap. `comm` is sanitized; `argv_sha256_prefix`
    /// is 8 hex chars over the NUL-joined argv.
    Killed {
        ts: SystemTime,
        session_uid: String,
        pid: u32,
        comm: String,
        argc: usize,
        argv_sha256_prefix: String,
        rss_kb: u64,
        soft_cap_bytes: u64,
        hard_cap_bytes: u64,
    },
    /// The watcher fired but every PID in the cgroup is in the
    /// protected set — only agent processes left. We refuse to kill
    /// the agent and leave the kernel `MemoryMax` to handle it.
    KillFailed {
        ts: SystemTime,
        session_uid: String,
        reason: String,
    },
}

// --- Sanitizer ---------------------------------------------------------

/// Sanitize an untrusted byte string for safe embedding in the
/// JSONL log and the activity-feed line. Spec lives in
/// DESIGN_MEMORY_CAP.md § Sanitizer.
///
/// 1. Strip C0 control bytes (`< 0x20` or `== 0x7f`) → `?`.
/// 2. Strip non-UTF-8 byte sequences → `?` per byte.
/// 3. Strip hostile valid-UTF-8 codepoints (C1, bidi, zero-width,
///    line/paragraph separators) → `?`.
/// 4. Cap length to `max_chars`; truncations end with `…`.
pub fn sanitize(input: &[u8], max_chars: usize) -> String {
    let cleaned_bytes: Vec<u8> = input
        .iter()
        .map(|&b| if b < 0x20 || b == 0x7f { b'?' } else { b })
        .collect();
    let s_utf8 = match std::str::from_utf8(&cleaned_bytes) {
        Ok(s) => s.to_string(),
        Err(_) => String::from_utf8_lossy(&cleaned_bytes)
            .chars()
            .map(|c| if c == '\u{FFFD}' { '?' } else { c })
            .collect(),
    };
    let filtered: String = s_utf8
        .chars()
        .map(|c| if is_hostile_codepoint(c) { '?' } else { c })
        .collect();
    if filtered.chars().count() > max_chars {
        let truncated: String = filtered.chars().take(max_chars - 1).collect();
        format!("{}…", truncated)
    } else {
        filtered
    }
}

fn is_hostile_codepoint(c: char) -> bool {
    let n = c as u32;
    if (0x80..=0x9F).contains(&n) {
        return true;
    }
    if matches!(n, 0x202A..=0x202E | 0x2066..=0x2069) {
        return true;
    }
    if matches!(n, 0x200B | 0x200C | 0x200D | 0xFEFF) {
        return true;
    }
    if matches!(n, 0x2028 | 0x2029) {
        return true;
    }
    false
}

// --- /proc readers -----------------------------------------------------

/// Read the PID set currently in `<cgroup_path>/cgroup.procs`. Empty
/// vec on any read error (cgroup gone, transient race, etc).
fn read_cgroup_procs(cgroup_path: &Path) -> Vec<u32> {
    let s = match std::fs::read_to_string(cgroup_path.join("cgroup.procs")) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    s.lines().filter_map(|l| l.trim().parse().ok()).collect()
}

/// Read PPID (`/proc/<pid>/stat` field 4). Returns None if /proc
/// entry vanished mid-read or the format is unexpected.
fn read_ppid(pid: u32) -> Option<u32> {
    let s = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    parse_stat_field(&s, 4)
}

/// Read `starttime` (`/proc/<pid>/stat` field 22, in clock ticks).
fn read_starttime(pid: u32) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    parse_stat_field(&s, 22)
}

/// Field N (1-indexed) of /proc/<pid>/stat. The trick: field 2 is
/// `(comm)` which can contain spaces and parens, so we slice on the
/// last `)` before splitting.
fn parse_stat_field<T: std::str::FromStr>(stat: &str, field: usize) -> Option<T> {
    let close = stat.rfind(')')?;
    let after = stat.get(close + 1..)?.trim_start();
    // After the comm closing paren, fields are space-separated.
    // Index in `after` is field 3 onwards.
    let parts: Vec<&str> = after.split_ascii_whitespace().collect();
    if field < 3 {
        return None;
    }
    parts.get(field - 3)?.parse().ok()
}

/// Read `VmRSS` from `/proc/<pid>/status`, in KiB.
fn read_rss_kb(pid: u32) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{}/status", pid)).ok()?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            for tok in rest.split_ascii_whitespace() {
                if let Ok(n) = tok.parse::<u64>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// Read `/proc/<pid>/comm` (kernel-bounded ≤16 bytes), trimmed.
fn read_comm(pid: u32) -> Option<Vec<u8>> {
    let mut bytes = std::fs::read(format!("/proc/{}/comm", pid)).ok()?;
    while matches!(bytes.last(), Some(b'\n') | Some(b'\r') | Some(0)) {
        bytes.pop();
    }
    Some(bytes)
}

/// Read `/proc/<pid>/cmdline` as a NUL-separated argv.
fn read_cmdline(pid: u32) -> Option<Vec<Vec<u8>>> {
    let bytes = std::fs::read(format!("/proc/{}/cmdline", pid)).ok()?;
    if bytes.is_empty() {
        return Some(Vec::new());
    }
    // cmdline is NUL-separated, often with a trailing NUL. Drop empty
    // trailing entries from a final NUL.
    let mut parts: Vec<Vec<u8>> = bytes.split(|&b| b == 0).map(|s| s.to_vec()).collect();
    while parts.last().map_or(false, |s| s.is_empty()) {
        parts.pop();
    }
    Some(parts)
}

/// Read the `high` counter from `<cgroup>/memory.events`. Returns 0
/// on any read failure (treated as "no breach").
fn read_memory_high_count(cgroup_path: &Path) -> u64 {
    let s = match std::fs::read_to_string(cgroup_path.join("memory.events")) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("high ") {
            return rest.trim().parse().unwrap_or(0);
        }
    }
    0
}

// --- Kill path ---------------------------------------------------------

#[cfg(target_os = "linux")]
mod kill {
    use super::*;
    use std::os::raw::c_int;

    /// One in-flight kill target. `Pidfd` keeps the original process
    /// reachable across PID reuse; `Fallback` uses `kill(2)` guarded by
    /// a starttime+cgroup recheck before each signal.
    pub enum KillTarget {
        Pidfd { fd: c_int },
        Fallback {
            pid: u32,
            starttime: u64,
            cgroup_path: PathBuf,
        },
    }

    pub fn open(pid: u32, cgroup_path: &Path) -> Option<KillTarget> {
        // Try pidfd_open first.
        let fd = unsafe {
            libc::syscall(libc::SYS_pidfd_open, pid as c_int, 0 as c_int) as c_int
        };
        if fd >= 0 {
            return Some(KillTarget::Pidfd { fd });
        }
        // Fallback: snapshot starttime so we can re-verify before each kill.
        let starttime = read_starttime(pid)?;
        Some(KillTarget::Fallback {
            pid,
            starttime,
            cgroup_path: cgroup_path.to_path_buf(),
        })
    }

    pub fn send_signal(target: &KillTarget, sig: c_int) -> bool {
        match target {
            KillTarget::Pidfd { fd } => {
                let ret = unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        *fd,
                        sig,
                        std::ptr::null::<libc::siginfo_t>(),
                        0 as c_int,
                    )
                };
                ret == 0
            }
            KillTarget::Fallback {
                pid,
                starttime,
                cgroup_path,
            } => {
                // Re-verify: starttime matches, cgroup membership intact.
                let cur = match read_starttime(*pid) {
                    Some(s) => s,
                    None => return false,
                };
                if cur != *starttime {
                    return false;
                }
                let in_cgroup = read_cgroup_procs(cgroup_path).contains(pid);
                if !in_cgroup {
                    return false;
                }
                let ret = unsafe { libc::kill(*pid as libc::pid_t, sig) };
                ret == 0
            }
        }
    }

    pub fn close(target: KillTarget) {
        if let KillTarget::Pidfd { fd } = target {
            unsafe {
                libc::close(fd);
            }
        }
    }
}

// --- Kill log ----------------------------------------------------------

/// Write one JSONL kill record into `dir`. Split from the runtime
/// caller (which resolves `~/.cm/memory_kills`) so tests can supply
/// a tempdir without racing other tests on `$HOME`. Best-effort on
/// errors — if we can't write, the kernel still killed the offender,
/// we just lose a forensic record.
fn write_kill_log_to(
    dir: &Path,
    session_uid: &str,
    ts: SystemTime,
    pid: u32,
    comm_sanitized: &str,
    argc: usize,
    argv_sha256_prefix: &str,
    rss_kb: u64,
    soft_cap_bytes: u64,
    hard_cap_bytes: u64,
) {
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(dir) {
            let mut perms = meta.permissions();
            perms.set_mode(0o700);
            let _ = std::fs::set_permissions(dir, perms);
        }
    }

    let path = dir.join(format!("{}.jsonl", session_uid));
    let ts_secs = ts
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let comm_json = serde_json::Value::String(comm_sanitized.to_string()).to_string();
    let line = format!(
        "{{\"ts\":{ts},\"session_uid\":\"{uid}\",\"pid\":{pid},\"comm\":{comm},\"argc\":{argc},\"argv_sha256_prefix\":\"{sha}\",\"rss_kb\":{rss},\"soft_cap_bytes\":{soft},\"hard_cap_bytes\":{hard}}}\n",
        ts = ts_secs,
        uid = session_uid,
        pid = pid,
        comm = comm_json,
        argc = argc,
        sha = argv_sha256_prefix,
        rss = rss_kb,
        soft = soft_cap_bytes,
        hard = hard_cap_bytes,
    );
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    if let Ok(mut f) = opts.open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

fn argv_sha256_prefix(argv: &[Vec<u8>]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let mut first = true;
    for arg in argv {
        if !first {
            hasher.update(&[0u8]);
        }
        hasher.update(arg);
        first = false;
    }
    let digest = hasher.finalize();
    let mut s = String::with_capacity(8);
    for b in digest.iter().take(4) {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

// --- Watcher thread ----------------------------------------------------

const STABILIZE_MS: u64 = 750;
const FOLLOWUP_MS: u64 = 2000;
const POLL_INTERVAL_MS: u64 = 1000;
const SIGTERM_GRACE_MS: u64 = 500;

#[cfg(target_os = "linux")]
pub fn spawn_watcher(
    session_uid: String,
    cgroup_path: PathBuf,
    soft_cap_bytes: u64,
    hard_cap_bytes: u64,
    kill_tx: mpsc::Sender<MemoryKillEvent>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        run_watcher(
            session_uid,
            cgroup_path,
            soft_cap_bytes,
            hard_cap_bytes,
            kill_tx,
        );
    })
}

#[cfg(not(target_os = "linux"))]
pub fn spawn_watcher(
    _session_uid: String,
    _cgroup_path: PathBuf,
    _soft_cap_bytes: u64,
    _hard_cap_bytes: u64,
    _kill_tx: mpsc::Sender<MemoryKillEvent>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(|| {})
}

#[cfg(target_os = "linux")]
fn run_watcher(
    session_uid: String,
    cgroup_path: PathBuf,
    soft_cap_bytes: u64,
    hard_cap_bytes: u64,
    kill_tx: mpsc::Sender<MemoryKillEvent>,
) {
    // Wait for the cgroup to actually appear (systemd-run takes a moment
    // after `tty::new` returns). Short timeout — if it never shows up,
    // bail rather than burning a thread.
    let appear_deadline = Instant::now() + Duration::from_secs(3);
    while !cgroup_path.exists() {
        if Instant::now() >= appear_deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let started = Instant::now();
    let stabilize_until = started + Duration::from_millis(STABILIZE_MS);
    let followup_until = started + Duration::from_millis(FOLLOWUP_MS);

    // Stabilization phase: wait for the agent's launcher to fork its
    // worker(s). At T+STABILIZE_MS, snapshot every PID currently in the
    // cgroup as the protected set.
    while Instant::now() < stabilize_until {
        if !cgroup_path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut protected: HashSet<u32> = read_cgroup_procs(&cgroup_path).into_iter().collect();

    // Follow-up window: admit any new PID whose ppid is already in the
    // protected set (catches wrapper-style launchers that lazily fork
    // their real worker after stabilization). After T+FOLLOWUP_MS, the
    // set is frozen and any new PID is killable.
    while Instant::now() < followup_until {
        std::thread::sleep(Duration::from_millis(100));
        if !cgroup_path.exists() {
            return;
        }
        let current = read_cgroup_procs(&cgroup_path);
        for pid in current {
            if protected.contains(&pid) {
                continue;
            }
            if let Some(ppid) = read_ppid(pid) {
                if protected.contains(&ppid) {
                    protected.insert(pid);
                }
            }
        }
    }

    // Main watch loop: poll memory.events for `high` counter increments.
    let mut last_high = read_memory_high_count(&cgroup_path);
    loop {
        std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
        if !cgroup_path.exists() {
            return;
        }
        let current_high = read_memory_high_count(&cgroup_path);
        if current_high <= last_high {
            continue;
        }
        last_high = current_high;
        handle_breach(
            &session_uid,
            &cgroup_path,
            &protected,
            soft_cap_bytes,
            hard_cap_bytes,
            &kill_tx,
        );
    }
}

#[cfg(target_os = "linux")]
fn handle_breach(
    session_uid: &str,
    cgroup_path: &Path,
    protected: &HashSet<u32>,
    soft_cap_bytes: u64,
    hard_cap_bytes: u64,
    kill_tx: &mpsc::Sender<MemoryKillEvent>,
) {
    let pids = read_cgroup_procs(cgroup_path);
    // Pick the highest-RSS PID not in the protected set.
    let mut best: Option<(u32, u64)> = None;
    let mut any_unprotected = false;
    for pid in &pids {
        if protected.contains(pid) {
            continue;
        }
        any_unprotected = true;
        let rss = match read_rss_kb(*pid) {
            Some(r) => r,
            None => continue,
        };
        match best {
            Some((_, br)) if br >= rss => {}
            _ => best = Some((*pid, rss)),
        }
    }
    if !any_unprotected {
        let _ = kill_tx.send(MemoryKillEvent::KillFailed {
            ts: SystemTime::now(),
            session_uid: session_uid.to_string(),
            reason: "all candidates are agent processes".into(),
        });
        return;
    }
    let (pid, rss_kb) = match best {
        Some(v) => v,
        None => return, // RSS reads all raced; nothing to do this iteration
    };

    // Open kill target before the kill so PID reuse can't sneak in.
    let target = match kill::open(pid, cgroup_path) {
        Some(t) => t,
        None => return,
    };

    // Snapshot what we'll log *before* killing — comm/cmdline of a
    // SIGKILLed process is no longer readable from /proc.
    let comm_raw = read_comm(pid).unwrap_or_default();
    let comm_sanitized = sanitize(&comm_raw, 16);
    let argv = read_cmdline(pid).unwrap_or_default();
    let argc = argv.len();
    let sha = argv_sha256_prefix(&argv);

    // SIGTERM, grace, SIGKILL. Track whether *either* signal actually
    // landed: if the target exited (or its PID got recycled) before we
    // could deliver, both pidfd_send_signal and the fallback's
    // starttime/cgroup recheck refuse, and we must NOT log a kill that
    // didn't happen — CLAUDE.md tells future agents to read the JSONL
    // when puzzled by a SIGKILL, so we keep that file trustworthy.
    let term_ok = kill::send_signal(&target, libc::SIGTERM);
    std::thread::sleep(Duration::from_millis(SIGTERM_GRACE_MS));
    let kill_ok = kill::send_signal(&target, libc::SIGKILL);
    kill::close(target);

    let log_dir = std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".cm/memory_kills"))
        .unwrap_or_else(|| PathBuf::from(".cm/memory_kills"));
    finalize_kill_outcome(
        term_ok || kill_ok,
        session_uid,
        pid,
        comm_sanitized,
        argc,
        sha,
        rss_kb,
        soft_cap_bytes,
        hard_cap_bytes,
        kill_tx,
        &log_dir,
    );
}

/// Emit the kill outcome and (only if a signal actually landed) write
/// the JSONL record. Returns whether the log was written. Extracted
/// from `handle_breach` so tests can drive both branches without
/// running real `kill(2)` against a real PID.
#[cfg_attr(not(test), allow(dead_code))]
fn finalize_kill_outcome(
    delivered: bool,
    session_uid: &str,
    pid: u32,
    comm_sanitized: String,
    argc: usize,
    argv_sha256_prefix: String,
    rss_kb: u64,
    soft_cap_bytes: u64,
    hard_cap_bytes: u64,
    kill_tx: &mpsc::Sender<MemoryKillEvent>,
    log_dir: &Path,
) -> bool {
    let ts = SystemTime::now();
    if !delivered {
        let _ = kill_tx.send(MemoryKillEvent::KillFailed {
            ts,
            session_uid: session_uid.to_string(),
            reason: format!("target PID {} exited before signal could be delivered", pid),
        });
        return false;
    }
    write_kill_log_to(
        log_dir,
        session_uid,
        ts,
        pid,
        &comm_sanitized,
        argc,
        &argv_sha256_prefix,
        rss_kb,
        soft_cap_bytes,
        hard_cap_bytes,
    );
    let _ = kill_tx.send(MemoryKillEvent::Killed {
        ts,
        session_uid: session_uid.to_string(),
        pid,
        comm: comm_sanitized,
        argc,
        argv_sha256_prefix,
        rss_kb,
        soft_cap_bytes,
        hard_cap_bytes,
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_ascii_passthrough() {
        assert_eq!(sanitize(b"rg --json .", 64), "rg --json .");
    }

    #[test]
    fn sanitize_strips_c0() {
        assert_eq!(sanitize(b"a\x1bb", 64), "a?b");
        assert_eq!(sanitize(b"\x07", 64), "?");
        assert_eq!(sanitize(b"\0", 64), "?");
        assert_eq!(sanitize(b"line1\nline2", 64), "line1?line2");
        assert_eq!(sanitize(b"a\tb", 64), "a?b");
        assert_eq!(sanitize(b"a\rb", 64), "a?b");
    }

    #[test]
    fn sanitize_strips_del() {
        assert_eq!(sanitize(b"a\x7fb", 64), "a?b");
    }

    #[test]
    fn sanitize_strips_invalid_utf8() {
        let out = sanitize(&[0x61, 0xFF, 0x62], 64);
        assert_eq!(out, "a?b");
    }

    #[test]
    fn sanitize_strips_c1() {
        let out = sanitize(&[0x61, 0xC2, 0x80, 0x62], 64);
        assert_eq!(out, "a?b");
        let out = sanitize(&[0x61, 0xC2, 0x9F, 0x62], 64);
        assert_eq!(out, "a?b");
    }

    #[test]
    fn sanitize_strips_bidi_overrides() {
        let out = sanitize(&[0x61, 0xE2, 0x80, 0xAE, 0x62], 64);
        assert_eq!(out, "a?b");
        let out = sanitize(&[0x61, 0xE2, 0x81, 0xA6, 0x62], 64);
        assert_eq!(out, "a?b");
    }

    #[test]
    fn sanitize_strips_zero_width() {
        let out = sanitize(&[0x61, 0xE2, 0x80, 0x8B, 0x62], 64);
        assert_eq!(out, "a?b");
        let out = sanitize(&[0x61, 0xEF, 0xBB, 0xBF, 0x62], 64);
        assert_eq!(out, "a?b");
    }

    #[test]
    fn sanitize_strips_line_separators() {
        let out = sanitize(&[0x61, 0xE2, 0x80, 0xA8, 0x62], 64);
        assert_eq!(out, "a?b");
        let out = sanitize(&[0x61, 0xE2, 0x80, 0xA9, 0x62], 64);
        assert_eq!(out, "a?b");
    }

    #[test]
    fn sanitize_caps_length() {
        let out = sanitize(b"abcdefghij", 5);
        assert_eq!(out, "abcd…");
    }

    #[test]
    fn sanitize_passes_normal_unicode() {
        let out = sanitize("café".as_bytes(), 64);
        assert_eq!(out, "café");
    }

    #[test]
    fn argv_sha256_stable() {
        let s1 = argv_sha256_prefix(&[b"rg".to_vec(), b"--json".to_vec(), b".".to_vec()]);
        let s2 = argv_sha256_prefix(&[b"rg".to_vec(), b"--json".to_vec(), b".".to_vec()]);
        assert_eq!(s1, s2);
        assert_eq!(s1.len(), 8);
        // Different argv produces different prefix (with overwhelming probability).
        let s3 = argv_sha256_prefix(&[b"rg".to_vec(), b"--no-json".to_vec(), b".".to_vec()]);
        assert_ne!(s1, s3);
    }

    #[test]
    fn argv_sha256_empty() {
        let s = argv_sha256_prefix(&[]);
        assert_eq!(s.len(), 8);
    }

    #[test]
    fn parse_stat_field_handles_paren_in_comm() {
        // pid 1234, comm "(weird)name", state R, ppid 5678, ...
        // Field 4 (ppid) should be 5678, not anything inside parens.
        let stat = "1234 (weird)name) R 5678 1 1 0 -1 4194304 0 0 0 0 0 0 0 0 20 0 1 0 12345 ...";
        let ppid: Option<u32> = parse_stat_field(stat, 4);
        assert_eq!(ppid, Some(5678));
    }

    #[test]
    fn parse_stat_field_starttime() {
        let stat = "1234 (rg) R 5678 1 1 0 -1 4194304 0 0 0 0 0 0 0 0 20 0 1 0 999888 ...";
        let st: Option<u64> = parse_stat_field(stat, 22);
        assert_eq!(st, Some(999888));
    }

    /// Models the exact race CLAUDE.md warns about: the watcher picked
    /// a PID, but by the time we tried to signal it, both the SIGTERM
    /// and the SIGKILL were rejected (target gone, or PID got recycled
    /// and pidfd_send_signal returned ESRCH, or the fallback's
    /// starttime/cgroup recheck refused). In that case we MUST emit
    /// `KillFailed` and *not* write a JSONL record — the file is the
    /// agent's source of truth on "why did my Bash get SIGKILLed?",
    /// so a fabricated entry would mislead future agents.
    #[test]
    fn finalize_no_delivery_does_not_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tx, rx) = std::sync::mpsc::channel();

        let wrote = finalize_kill_outcome(
            false, // delivered = false (signals refused)
            "test-session-uid-123",
            41892,
            "rg".into(),
            4,
            "a1b2c3d4".into(),
            5_800_000,
            6 * 1024 * 1024 * 1024,
            10 * 1024 * 1024 * 1024,
            &tx,
            dir.path(),
        );
        assert!(!wrote, "must not log when no signal landed");

        // No JSONL file should have been created.
        let log_path = dir.path().join("test-session-uid-123.jsonl");
        assert!(
            !log_path.exists(),
            "JSONL file must not be written when delivery failed"
        );

        // Exactly one event was sent, and it's KillFailed (not Killed).
        let evt = rx.try_recv().expect("event must be sent");
        match evt {
            MemoryKillEvent::KillFailed { reason, session_uid, .. } => {
                assert_eq!(session_uid, "test-session-uid-123");
                assert!(
                    reason.contains("41892") && reason.contains("before signal"),
                    "reason should name the PID and explain why: {}",
                    reason
                );
            }
            MemoryKillEvent::Killed { .. } => {
                panic!("must not emit Killed when signal didn't land");
            }
        }
        assert!(rx.try_recv().is_err(), "no further events");
    }

    /// Counterpart to the above: when a signal *did* land, we DO write
    /// the JSONL record and emit `Killed`. Same helper, different
    /// branch — guards against a future refactor regressing the
    /// "always write" path back into existence.
    #[test]
    fn finalize_with_delivery_writes_log_and_emits_killed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tx, rx) = std::sync::mpsc::channel();

        let wrote = finalize_kill_outcome(
            true,
            "test-session-uid-456",
            42000,
            "stress-ng".into(),
            6,
            "deadbeef".into(),
            900_000,
            512 * 1024 * 1024,
            1024 * 1024 * 1024,
            &tx,
            dir.path(),
        );
        assert!(wrote, "should report log written when delivery succeeded");

        let log_path = dir.path().join("test-session-uid-456.jsonl");
        assert!(log_path.exists(), "JSONL file should exist");
        let body = std::fs::read_to_string(&log_path).expect("read log");
        assert!(body.contains("\"pid\":42000"));
        assert!(body.contains("\"comm\":\"stress-ng\""));
        assert!(body.contains("\"argv_sha256_prefix\":\"deadbeef\""));
        // One line per kill, terminated.
        assert_eq!(body.lines().count(), 1);
        assert!(body.ends_with('\n'));

        match rx.try_recv().expect("event") {
            MemoryKillEvent::Killed {
                pid,
                comm,
                session_uid,
                ..
            } => {
                assert_eq!(pid, 42000);
                assert_eq!(comm, "stress-ng");
                assert_eq!(session_uid, "test-session-uid-456");
            }
            other => panic!("expected Killed, got {:?}", other),
        }
    }
}
