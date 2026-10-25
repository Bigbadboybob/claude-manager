//! Org-model capabilities this host advertises (doc/org-model.md §12).
//!
//! Each role is reported separately: the brain (this binary), the running
//! holder (its hello/rehello reply), and the configured rollback pin (the
//! brain binary the holder would fall back to). The API turns a feature on
//! for a host only when all three have it, so [`advertised`] is their
//! intersection and the per-role lists are reported beside it.
//!
//! Registering a capability is one entry in [`brain`]. S0b registers
//! `org_preserve_v1`; `lifecycle_guard_v1`, `org_stamp_v1` and
//! `occupancy_v1` are reserved names, added by the build that implements
//! them.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub use cm_holder_proto::org::{LIFECYCLE_GUARD_V1, OCCUPANCY_V1, ORG_PRESERVE_V1, ORG_STAMP_V1};

/// Every capability name the org model defines.
pub const KNOWN: [&str; 4] = [ORG_PRESERVE_V1, LIFECYCLE_GUARD_V1, ORG_STAMP_V1, OCCUPANCY_V1];

/// Brain-only capabilities: implemented entirely in the brain, so the holder
/// need not list them, only carry the org object (`org_preserve_v1`); the
/// rollback pin must still have them. Swarm-Coord, 2026-10-25:
/// `lifecycle_guard_v1` (the sweep honours item closes and workspace kinds
/// AND local `home` marks are applied) is pushed here only by a build with
/// both items' sweep guard ([`crate::lifecycle_guard`]) and org's
/// mark-apply, and lights up from the second deploy of such a build. Until
/// then the API's per-task gate keeps the central mirror off.
pub const BRAIN_ONLY: &[&str] = &[];

/// What this brain build implements: the record capabilities it shares with
/// the holder (holder-proto) plus [`BRAIN_ONLY`].
pub fn brain() -> Vec<String> {
    let mut caps = cm_holder_proto::org::preserved_capabilities();
    caps.extend(BRAIN_ONLY.iter().map(|c| c.to_string()));
    caps
}

/// Whether the running holder supports `cap`: it lists it, or `cap` is
/// brain-only and the holder carries the org object.
fn holder_supports(cap: &str, holder: &[String]) -> bool {
    holder.iter().any(|h| h == cap)
        || (BRAIN_ONLY.contains(&cap) && holder.iter().any(|h| h == ORG_PRESERVE_V1))
}

/// The running holder's list (`None` when this brain is not split).
pub fn holder() -> Option<Vec<String>> {
    crate::holder_mode::global().map(|client| client.holder_capabilities())
}

/// One brain generation's record in [`ledger_path`]: which binary ran, in
/// which lineage and epoch, and what it implemented. The rollback pin's
/// capabilities are read from it.
fn ledger_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".cm/brain-capabilities.json")
}

const LEDGER_KEEP: usize = 32;

/// A brain generation's place in its host's history: the `lineage` is the
/// process that outlives brain swaps (the holder in split mode, this pid in a
/// monolith, which re-exec keeps), identified by pid and kernel start time so
/// a reboot starts a new one; `epoch` counts brain generations within it
/// (`holder_epoch` split, `reexec_generation` monolith).
#[derive(Clone, Debug, PartialEq)]
pub struct Epoch {
    pub lineage: String,
    pub epoch: u64,
    pub split: bool,
}

fn lineage_of(pid: libc::pid_t) -> String {
    let start = crate::adopt::proc_starttime(pid).unwrap_or(0);
    format!("{pid}:{start}")
}

/// This brain's lineage and epoch.
pub fn current_epoch(state: &crate::state::DaemonState) -> Epoch {
    match crate::holder_mode::global() {
        Some(client) => Epoch {
            // SAFETY: getppid has no preconditions.
            lineage: lineage_of(unsafe { libc::getppid() }),
            epoch: client.epoch,
            split: true,
        },
        None => Epoch {
            lineage: lineage_of(std::process::id() as libc::pid_t),
            epoch: state.reexec_generation,
            split: false,
        },
    }
}

/// This binary's SHA-256, computed once (the image is tens of MB).
fn exe_digest() -> Option<String> {
    static DIGEST: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    DIGEST
        .get_or_init(|| std::fs::read("/proc/self/exe").ok().map(|bytes| hex_digest(&bytes)))
        .clone()
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// The epoch this brain recorded at boot, for [`report`].
static BOOT_EPOCH: std::sync::OnceLock<Epoch> = std::sync::OnceLock::new();

/// Record this brain generation (once, at boot). Best-effort: a write
/// failure only means a later generation cannot vouch for this one as its
/// rollback pin, which makes it advertise less, never more.
pub fn record_boot(epoch: Epoch) {
    let _ = BOOT_EPOCH.set(epoch.clone());
    let Some(digest) = exe_digest() else { return };
    let _ = record_in(&ledger_path(), &epoch, &digest, &brain());
}

fn read_ledger(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|v| v.get("generations").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

fn record_in(path: &Path, at: &Epoch, digest: &str, caps: &[String]) -> std::io::Result<()> {
    let mut generations = read_ledger(path);
    generations.retain(|g| {
        !(g["lineage"].as_str() == Some(at.lineage.as_str()) && g["epoch"].as_u64() == Some(at.epoch))
    });
    generations.push(json!({
        "lineage": at.lineage,
        "epoch": at.epoch,
        "exe_sha256": digest,
        "capabilities": caps,
        "recorded_at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0),
    }));
    let excess = generations.len().saturating_sub(LEDGER_KEEP);
    generations.drain(..excess);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp = tempfile::NamedTempFile::new_in(path.parent().unwrap_or(Path::new(".")))?;
    std::io::Write::write_all(&mut tmp, serde_json::to_vec(&json!({"generations": generations}))?.as_slice())?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// The rollback pin's capabilities: walk back from `at.epoch - 1` through
/// this lineage's recorded generations while they ran this same binary (a
/// crash respawn keeps the pin); the first one that ran a different binary
/// is the pin. A missing epoch on the way (a generation that recorded
/// nothing, e.g. a pre-S0b brain rolled back to) means the pin is unknown:
/// none. This never trusts a record across a gap, so B (S0b) → rollback to
/// a pre-S0b A → C cannot read B as C's pin.
fn pin_from(path: &Path, at: &Epoch, current: Option<&str>) -> Vec<String> {
    let ledger = read_ledger(path);
    let entry = |epoch: u64| {
        ledger.iter().find(|g| {
            g["lineage"].as_str() == Some(at.lineage.as_str()) && g["epoch"].as_u64() == Some(epoch)
        })
    };
    let mut epoch = at.epoch;
    while epoch > 0 {
        epoch -= 1;
        let Some(g) = entry(epoch) else { return Vec::new() };
        if g["exe_sha256"].as_str() != current {
            return g["capabilities"]
                .as_array()
                .map(|caps| caps.iter().filter_map(|c| c.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
        }
    }
    Vec::new()
}

fn intersect(base: Vec<String>, other: Option<&[String]>) -> Vec<String> {
    match other {
        Some(other) => base.into_iter().filter(|c| other.contains(c)).collect(),
        None => base,
    }
}

/// Whether a rollback pin constrains this brain: in split mode, what the
/// holder said (`None` = unknown, treated as configured); a monolith's only
/// rollback target is the image it replaced, so any generation past a fresh
/// boot has one.
fn has_pin(at: &Epoch, previous_pin: Option<bool>) -> bool {
    if at.split {
        previous_pin != Some(false)
    } else {
        at.epoch > 0
    }
}

/// The per-role report and the advertised intersection. `previous_pin` is
/// the holder's answer to "is a rollback pin configured" (`None` when not
/// split or unknown).
pub fn report(previous_pin: Option<bool>) -> Value {
    let brain = brain();
    let holder = holder();
    let pin = BOOT_EPOCH.get().and_then(|at| {
        has_pin(at, previous_pin).then(|| pin_from(&ledger_path(), at, exe_digest().as_deref()))
    });
    // A brain that has not recorded its boot yet (the first instant after
    // start) cannot vouch for its pin: advertise nothing until it has.
    let pin = if BOOT_EPOCH.get().is_none() { Some(Vec::new()) } else { pin };
    let advertised: Vec<String> = brain
        .iter()
        .filter(|c| holder.as_deref().is_none_or(|h| holder_supports(c, h)))
        .cloned()
        .collect();
    let advertised = intersect(advertised, pin.as_deref());
    json!({
        "brain": brain,
        "holder": holder,
        "rollback_pin": pin,
        "advertised": advertised,
    })
}

/// The holder's `previous_pin`, cached: `daemon.health` refreshes it, and
/// [`advertised`] asks the holder only when the cache is older than
/// [`PIN_CACHE`].
static PIN_STATUS: std::sync::Mutex<Option<(std::time::Instant, bool)>> = std::sync::Mutex::new(None);
const PIN_CACHE: std::time::Duration = std::time::Duration::from_secs(300);

/// Record a fresh holder `previous_pin` (from `daemon.health`'s status trip).
pub fn note_previous_pin(previous_pin: bool) {
    *PIN_STATUS.lock().unwrap_or_else(|p| p.into_inner()) = Some((std::time::Instant::now(), previous_pin));
}

/// What this host advertises, for the heartbeat.
pub fn advertised() -> Vec<String> {
    let previous_pin = crate::holder_mode::global().and_then(|client| {
        let cached = *PIN_STATUS.lock().unwrap_or_else(|p| p.into_inner());
        match cached {
            Some((at, pin)) if at.elapsed() < PIN_CACHE => Some(pin),
            _ => client.status().ok().map(|status| {
                note_previous_pin(status.previous_pin);
                status.previous_pin
            }),
        }
    });
    report(previous_pin)["advertised"]
        .as_array()
        .map(|caps| caps.iter().filter_map(|c| c.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(epoch: u64) -> Epoch {
        Epoch { lineage: "100:5".into(), epoch, split: true }
    }

    #[test]
    fn names_are_the_designed_four() {
        assert_eq!(KNOWN, ["org_preserve_v1", "lifecycle_guard_v1", "org_stamp_v1", "occupancy_v1"]);
        assert_eq!(brain(), vec![ORG_PRESERVE_V1.to_string()]);
    }

    #[test]
    fn the_pin_is_the_previous_other_binary_in_this_lineage() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("brain-capabilities.json");
        assert!(pin_from(&ledger, &at(2), Some("b")).is_empty(), "nothing recorded: a pre-S0b pin");
        // epoch 1 ran pre-S0b A, which records nothing; B (S0b) at 2.
        record_in(&ledger, &at(2), "b", &brain()).unwrap();
        assert!(pin_from(&ledger, &at(2), Some("b")).is_empty(), "the pin (A) recorded nothing");
        // A crash respawn of B at 3 keeps the same (unknown) pin.
        record_in(&ledger, &at(3), "b", &brain()).unwrap();
        assert!(pin_from(&ledger, &at(3), Some("b")).is_empty());
        // C (S0b) deployed at 4 pins B.
        record_in(&ledger, &at(4), "c", &brain()).unwrap();
        assert_eq!(pin_from(&ledger, &at(4), Some("c")), brain());
        // Another lineage (after a reboot) never reads this one.
        let other = Epoch { lineage: "200:9".into(), epoch: 5, split: true };
        assert!(pin_from(&ledger, &other, Some("c")).is_empty());
    }

    /// Review #2: B (S0b) at 2, rolled back to a pre-S0b A at 3 (which
    /// records nothing), then C at 4: the gap at 3 means C's pin is unknown,
    /// never B.
    #[test]
    fn a_rollback_to_a_pre_s0b_brain_is_a_gap_not_the_older_record() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("brain-capabilities.json");
        record_in(&ledger, &at(2), "b", &brain()).unwrap();
        record_in(&ledger, &at(4), "c", &brain()).unwrap();
        assert!(pin_from(&ledger, &at(4), Some("c")).is_empty());
    }

    #[test]
    fn brain_only_capabilities_need_the_holder_to_carry_the_org_object() {
        let carrying = vec![ORG_PRESERVE_V1.to_string()];
        assert!(holder_supports(ORG_PRESERVE_V1, &carrying));
        assert!(!holder_supports(ORG_PRESERVE_V1, &[]), "a pre-S0b holder");
        for cap in BRAIN_ONLY {
            assert!(holder_supports(cap, &carrying));
            assert!(!holder_supports(cap, &[]));
        }
        assert!(!holder_supports(LIFECYCLE_GUARD_V1, &carrying) || BRAIN_ONLY.contains(&LIFECYCLE_GUARD_V1));
    }

    #[test]
    fn advertised_is_the_intersection_of_the_roles() {
        let caps = brain();
        assert_eq!(intersect(caps.clone(), None), caps);
        assert!(intersect(caps.clone(), Some(&[])).is_empty(), "an old holder preserves nothing");
        assert_eq!(intersect(caps.clone(), Some(&caps)), caps);
    }

    /// Review #4: a monolith applies the pin rule past a fresh boot; split
    /// mode follows the holder (unknown = configured).
    #[test]
    fn who_has_a_pin() {
        let mono = |epoch| Epoch { lineage: "1:1".into(), epoch, split: false };
        assert!(!has_pin(&mono(0), None), "a fresh monolith boot has no older image");
        assert!(has_pin(&mono(3), None), "after a handoff: the image it replaced");
        assert!(!has_pin(&at(1), Some(false)));
        assert!(has_pin(&at(1), Some(true)));
        assert!(has_pin(&at(1), None), "unknown pin status never widens the list");
    }

    #[test]
    fn nothing_is_advertised_before_the_boot_is_recorded() {
        if BOOT_EPOCH.get().is_none() {
            assert_eq!(report(None)["advertised"], json!([]));
        }
    }
}
