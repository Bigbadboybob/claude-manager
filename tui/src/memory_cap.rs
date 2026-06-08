//! Per-session memory cap types. See DESIGN_MEMORY_CAP.md for the full
//! rationale; the short version: agents and their tool subprocesses run
//! inside a transient `systemd-run --user --scope` cgroup with
//! `MemoryHigh` (soft) and `MemoryMax` (hard) limits, plus a watcher
//! that picks the largest non-agent PID to kill on soft-cap breach.
//!
//! Caps are opt-in via env vars (see `Config::memory_cap_for`) and
//! gated on the startup preflight probe (see `preflight.rs`).

use std::path::PathBuf;

/// Resolved memory limits for a single capped session. Constructed
/// when *both* the user has configured a cap (via
/// `CM_SESSION_MEM_SOFT_<TYPE>` / `CM_SESSION_MEM_HARD_<TYPE>`)
/// *and* startup preflight succeeded. Passing `Some(MemoryCap)` to
/// `Session::new` always means "wrap this spawn"; `None` always means
/// "no wrap".
#[derive(Clone, Debug)]
pub struct MemoryCap {
    /// `MemoryHigh` — soft threshold. The watcher reacts on breach.
    pub soft_bytes: u64,
    /// `MemoryMax` — hard backstop. Kernel kills if userspace can't recover.
    pub hard_bytes: u64,
    /// Used as the systemd scope unit name (`cm-sess-<uid>.scope`) and
    /// keyed in the kill log filename (`~/.cm/memory_kills/<uid>.jsonl`).
    pub session_uid: String,
    /// Parent cgroup path discovered by preflight, e.g.
    /// `/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice`.
    /// The session's own cgroup is `<cgroup_prefix>/cm-sess-<uid>.scope`.
    pub cgroup_prefix: PathBuf,
}

/// Result of the startup preflight probe. Cached for the lifetime of
/// the TUI run; every `Session::new` consults this synchronously.
#[derive(Clone, Debug)]
pub enum MemoryCapAvailability {
    /// Preflight succeeded; caps can be applied. `cgroup_prefix` is the
    /// resolved parent-of-scope path discovered at preflight, e.g.
    /// `/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice`.
    /// A session's cgroup is `<prefix>/cm-sess-<uid>.scope`.
    Available { cgroup_prefix: PathBuf },
    /// Preflight failed; no session is wrapped. The reason is logged
    /// once at startup so the user can fix their environment.
    Unavailable { reason: String },
}

impl MemoryCapAvailability {
    pub fn is_available(&self) -> bool {
        matches!(self, MemoryCapAvailability::Available { .. })
    }
}

/// Parse `"6G" | "512M" | "1024K" | "67108864"` into a byte count.
/// Suffixes use powers of 2 (K=1024, M=1024², G=1024³, T=1024⁴) —
/// same convention systemd uses for `-p MemoryHigh=`.
pub fn parse_bytes(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let bytes = s.as_bytes();
    let last = bytes[bytes.len() - 1];
    let (num_str, multiplier) = match last {
        b'K' | b'k' => (&s[..s.len() - 1], 1024u64),
        b'M' | b'm' => (&s[..s.len() - 1], 1024u64 * 1024),
        b'G' | b'g' => (&s[..s.len() - 1], 1024u64 * 1024 * 1024),
        b'T' | b't' => (&s[..s.len() - 1], 1024u64 * 1024 * 1024 * 1024),
        _ => (s, 1u64),
    };
    let n: u64 = num_str.trim().parse().ok()?;
    n.checked_mul(multiplier)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bytes_suffixes() {
        assert_eq!(parse_bytes("6G"), Some(6u64 * 1024 * 1024 * 1024));
        assert_eq!(parse_bytes("10G"), Some(10u64 * 1024 * 1024 * 1024));
        assert_eq!(parse_bytes("512M"), Some(512u64 * 1024 * 1024));
        assert_eq!(parse_bytes("1024K"), Some(1024u64 * 1024));
        assert_eq!(parse_bytes("1T"), Some(1024u64 * 1024 * 1024 * 1024));
    }

    #[test]
    fn parse_bytes_lowercase_suffix() {
        assert_eq!(parse_bytes("6g"), Some(6u64 * 1024 * 1024 * 1024));
        assert_eq!(parse_bytes("64m"), Some(64u64 * 1024 * 1024));
    }

    #[test]
    fn parse_bytes_no_suffix() {
        assert_eq!(parse_bytes("67108864"), Some(67108864));
        assert_eq!(parse_bytes("0"), Some(0));
    }

    #[test]
    fn parse_bytes_whitespace() {
        assert_eq!(parse_bytes("  6G  "), Some(6u64 * 1024 * 1024 * 1024));
    }

    #[test]
    fn parse_bytes_invalid() {
        assert_eq!(parse_bytes(""), None);
        assert_eq!(parse_bytes("   "), None);
        assert_eq!(parse_bytes("garbage"), None);
        assert_eq!(parse_bytes("6X"), None);
        assert_eq!(parse_bytes("G"), None);
    }
}
