//! Brain health signals for `daemon.log` and Owner.
//!
//! On 2026-10-15 Owner's keypresses took minutes to arrive and TUI
//! reconnects took 3–5× longer, and nothing recorded why (the suspect was a
//! disk-full episode plus heavy disk load on cm-sessions). These make the
//! next one visible:
//!
//! - slow input: an attach-input write that took over 1 s to reach the PTY
//!   ([`note_input`]);
//! - stalls: a 250 ms ticker that logs when it fires over 1 s late — the
//!   whole brain was not scheduled, or was blocked in the kernel;
//! - reconnects: a slow screen render ([`note_attach`]) and one summary line
//!   per burst of attaches;
//! - disk: an Owner alert when `/` or `/home` reaches 90%, once, re-armed
//!   below 85%.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::state::DaemonState;

const SLOW_INPUT: Duration = Duration::from_secs(1);
const TICK: Duration = Duration::from_millis(250);
const STALL: Duration = Duration::from_secs(1);
const SLOW_RENDER: Duration = Duration::from_millis(250);
/// Attaches less than this apart belong to one burst.
const BURST_GAP: Duration = Duration::from_secs(5);
const DISK_EVERY: Duration = Duration::from_secs(60);
const DISK_ALERT_PCT: f64 = 90.0;
const DISK_REARM_PCT: f64 = 85.0;
const DISK_MOUNTS: [&str; 2] = ["/", "/home"];

/// An attach-input frame reached the PTY: log it when it was slow, split
/// into the state-lock wait and the write itself.
pub fn note_input(uid: &str, bytes: usize, lock_wait: Duration, write: Duration) {
    let total = lock_wait + write;
    if total >= SLOW_INPUT {
        eprintln!(
            "cm-daemon: slow input: {uid} {bytes} bytes took {} ms to reach the PTY \
             (state lock {} ms, write {} ms)",
            total.as_millis(),
            lock_wait.as_millis(),
            write.as_millis(),
        );
    }
}

#[derive(Default)]
struct Burst {
    started: Option<Instant>,
    last: Option<Instant>,
    attaches: usize,
    bytes: usize,
    kinds: std::collections::BTreeMap<&'static str, usize>,
    render: Duration,
    slowest_render: Duration,
}

static BURST: Mutex<Burst> = Mutex::new(Burst {
    started: None,
    last: None,
    attaches: 0,
    bytes: 0,
    kinds: std::collections::BTreeMap::new(),
    render: Duration::ZERO,
    slowest_render: Duration::ZERO,
});

/// One `attach.direct` was bound. `kind` is how its replay was sent
/// (`screen`, `resume`, `ring`); `render` is the screen render time.
pub fn note_attach(uid: &str, kind: &'static str, replay_bytes: usize, render: Option<Duration>) {
    if let Some(render) = render.filter(|r| *r >= SLOW_RENDER) {
        eprintln!(
            "cm-daemon: slow attach render: {uid} took {} ms ({replay_bytes} bytes sent)",
            render.as_millis()
        );
    }
    let now = Instant::now();
    let mut b = BURST.lock().unwrap_or_else(|p| p.into_inner());
    if b.last.is_some_and(|last| now - last >= BURST_GAP) {
        flush(&mut b);
    }
    b.started.get_or_insert(now);
    b.last = Some(now);
    b.attaches += 1;
    b.bytes += replay_bytes;
    *b.kinds.entry(kind).or_default() += 1;
    let render = render.unwrap_or_default();
    b.render += render;
    b.slowest_render = b.slowest_render.max(render);
}

/// Log a finished burst (more than one attach) and reset.
fn flush(b: &mut Burst) {
    if b.attaches > 1 {
        let span = b.last.zip(b.started).map_or(Duration::ZERO, |(l, s)| l - s);
        let kinds: Vec<String> = b.kinds.iter().map(|(k, n)| format!("{n} {k}")).collect();
        eprintln!(
            "cm-daemon: attach burst: {} attaches over {:.1} s ({}), {} replay bytes, \
             render {} ms total / {} ms slowest",
            b.attaches,
            span.as_secs_f64(),
            kinds.join(", "),
            b.bytes,
            b.render.as_millis(),
            b.slowest_render.as_millis(),
        );
    }
    *b = Burst::default();
}

fn flush_idle_burst(now: Instant) {
    let mut b = BURST.lock().unwrap_or_else(|p| p.into_inner());
    if b.last.is_some_and(|last| now - last >= BURST_GAP) {
        flush(&mut b);
    }
}

/// Start the health ticker. Best-effort: without it the daemon runs as
/// before, just quieter.
pub fn spawn(state: Arc<Mutex<DaemonState>>) {
    let spawned = std::thread::Builder::new()
        .name("cm-health".into())
        .spawn(move || {
            let mut disk = DiskWatch::default();
            let mut next_disk = Instant::now();
            let mut expected = Instant::now() + TICK;
            loop {
                std::thread::sleep(expected.saturating_duration_since(Instant::now()));
                let now = Instant::now();
                let late = now.saturating_duration_since(expected);
                if late >= STALL {
                    eprintln!(
                        "cm-daemon: brain stall: the health tick ran {} ms late",
                        late.as_millis()
                    );
                }
                expected = now + TICK;
                flush_idle_burst(now);
                if now >= next_disk {
                    next_disk = now + DISK_EVERY;
                    disk.check(&state, &DISK_MOUNTS);
                }
            }
        });
    if let Err(e) = spawned {
        eprintln!("cm-daemon: health ticker not started: {e}");
    }
}

/// Used percentage of the filesystem holding `path` (as `df` counts it:
/// blocks reserved for root count as unavailable) and its device.
fn usage(path: &str) -> Option<(f64, u64, u64)> {
    let c = std::ffi::CString::new(path).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 || st.f_blocks == 0 {
        return None;
    }
    let used = (st.f_blocks - st.f_bfree) as f64;
    let avail = st.f_bavail as f64;
    let pct = used * 100.0 / (used + avail).max(1.0);
    let meta = std::fs::metadata(path).ok()?;
    use std::os::unix::fs::MetadataExt;
    Some((pct, st.f_bavail as u64 * st.f_frsize as u64, meta.dev()))
}

#[derive(Default)]
struct DiskWatch {
    /// Devices already alerted on, until they drop below the re-arm level.
    alerted: std::collections::HashSet<u64>,
}

impl DiskWatch {
    fn check(&mut self, state: &Arc<Mutex<DaemonState>>, mounts: &[&str]) {
        let mut seen = std::collections::HashSet::new();
        for path in mounts {
            let Some((pct, free, dev)) = usage(path) else { continue };
            if !seen.insert(dev) {
                continue; // `/home` on the root filesystem
            }
            if let Some(message) = self.observe(path, dev, pct, free) {
                let s = state.lock().unwrap_or_else(|p| p.into_inner());
                if let Err(e) = crate::owner_attention::escalate(&s, disk_escalation(&message)) {
                    eprintln!("cm-daemon: disk alert: {e}");
                }
            }
        }
    }

    /// The alert to send for this reading, if any: the first reading at or
    /// over [`DISK_ALERT_PCT`]; nothing more until it drops below
    /// [`DISK_REARM_PCT`].
    fn observe(&mut self, path: &str, dev: u64, pct: f64, free: u64) -> Option<String> {
        if pct < DISK_REARM_PCT {
            if self.alerted.remove(&dev) {
                eprintln!("cm-daemon: disk: {path} back to {pct:.0}% used; alert re-armed");
            }
            return None;
        }
        if pct < DISK_ALERT_PCT || !self.alerted.insert(dev) {
            return None;
        }
        let host = hostname();
        let message = format!(
            "{host}: {path} is {pct:.0}% full ({:.1} GB free). Sessions and the daemon fail \
             when it fills; free space now.",
            free as f64 / 1e9
        );
        eprintln!("cm-daemon: disk: {message}");
        Some(message)
    }
}

fn disk_escalation(message: &str) -> crate::owner_attention::Escalation {
    crate::owner_attention::Escalation {
        source: "disk".into(),
        dedupe_key: "system:disk".into(),
        urgency: crate::owner_availability::Urgency::Emergency,
        summary: message.into(),
        session_uid: None,
        task_id: None,
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| h.trim().to_string())
        .unwrap_or_else(|_| "this host".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_alert_fires_once_at_90_and_rearms_below_85() {
        let mut w = DiskWatch::default();
        assert!(w.observe("/", 1, 89.9, 5_000_000_000).is_none());
        let first = w.observe("/", 1, 90.0, 3_000_000_000).expect("alert at 90%");
        assert!(first.contains("/ is 90% full (3.0 GB free)"), "{first}");
        assert!(w.observe("/", 1, 97.0, 1).is_none(), "once per episode");
        assert!(w.observe("/", 1, 86.0, 1).is_none(), "still above the re-arm level");
        assert!(w.observe("/home", 2, 95.0, 1).is_some(), "another filesystem alerts on its own");
        assert!(w.observe("/", 1, 84.0, 1).is_none());
        assert!(w.observe("/", 1, 91.0, 1).is_some(), "re-armed below 85%");
    }

    #[test]
    fn root_usage_is_readable_and_home_on_the_same_filesystem_is_one_device() {
        let (pct, _, dev) = usage("/").expect("statvfs /");
        assert!((0.0..=100.0).contains(&pct));
        if let Some((_, _, home)) = usage("/home") {
            let same = std::fs::metadata("/home").map(|m| {
                use std::os::unix::fs::MetadataExt;
                m.dev()
            });
            assert_eq!(same.ok(), Some(home));
            let _ = dev;
        }
    }

    #[test]
    fn disk_alert_reaches_owner_attention_as_one_emergency() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = DaemonState::new();
        s.daemon_sessions_path = Some(tmp.path().join("daemon-sessions.json"));
        s.messaging_root = tmp.path().to_path_buf();
        std::fs::write(tmp.path().join("daemon-id"), uuid::Uuid::new_v4().to_string()).unwrap();
        let first = crate::owner_attention::escalate(&s, disk_escalation("host: / is 91% full")).unwrap();
        assert!(matches!(first, crate::owner_attention::GateOutcome::Delivered { .. }));
        let again = crate::owner_attention::escalate(&s, disk_escalation("host: / is 93% full")).unwrap();
        assert!(matches!(again, crate::owner_attention::GateOutcome::Coalesced { .. }));
        let alerts = crate::owner_attention::snapshot(&s).unwrap();
        assert_eq!(alerts["system:disk"].urgency.as_deref(), Some("emergency"));
    }

    #[test]
    fn attach_bursts_summarise_after_a_gap() {
        let mut b = Burst::default();
        let t0 = Instant::now();
        b.started = Some(t0);
        b.last = Some(t0 + Duration::from_secs(2));
        b.attaches = 3;
        b.kinds.insert("screen", 3);
        flush(&mut b);
        assert_eq!(b.attaches, 0, "a flushed burst resets");
    }
}
