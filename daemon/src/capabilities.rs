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

/// What this brain build implements.
///
/// `lifecycle_guard_v1` means the sweep honours item closes and workspace
/// kinds AND local `home` marks are applied, so it is registered here only
/// by a build with both items' sweep guard ([`crate::lifecycle_guard`]) and
/// org's mark-apply (Swarm-Coord, 2026-10-25): whichever lands second pushes
/// it. Until then the API's per-task gate keeps the central mirror off.
pub fn brain() -> Vec<String> {
    // The record code is shared with the holder (holder-proto), so what the
    // brain preserves is what that crate preserves.
    cm_holder_proto::org::preserved_capabilities()
}

/// The running holder's list (`None` when this brain is not split).
pub fn holder() -> Option<Vec<String>> {
    crate::holder_mode::global().map(|client| client.holder_capabilities())
}

/// One brain generation's record in [`ledger_path`]: which binary ran and
/// what it implemented. The rollback pin's capabilities are read from it.
fn ledger_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".cm/brain-capabilities.json")
}

const LEDGER_KEEP: usize = 16;

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

/// Record this brain generation (once, at boot). Best-effort: a write
/// failure only means the next generation cannot vouch for this one as its
/// rollback pin, which makes it advertise less, never more.
pub fn record_boot() {
    let Some(digest) = exe_digest() else { return };
    let _ = record_in(&ledger_path(), &digest, &brain());
}

fn read_ledger(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|v| v.get("generations").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

fn record_in(path: &Path, digest: &str, caps: &[String]) -> std::io::Result<()> {
    let mut generations = read_ledger(path);
    generations.retain(|g| g.get("exe_sha256").and_then(Value::as_str) != Some(digest));
    generations.push(json!({
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

/// The rollback pin's capabilities: those of the most recent recorded
/// generation that ran a different binary than this one (a crash respawn of
/// the same binary is not a pin change). Empty when nothing was recorded:
/// a pin from before S0b preserves nothing new.
fn pin_from(path: &Path, current: Option<&str>) -> Vec<String> {
    read_ledger(path)
        .iter()
        .rev()
        .find(|g| g.get("exe_sha256").and_then(Value::as_str) != current)
        .and_then(|g| g.get("capabilities").and_then(Value::as_array).cloned())
        .map(|caps| caps.iter().filter_map(|c| c.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

fn intersect(base: Vec<String>, other: Option<&[String]>) -> Vec<String> {
    match other {
        Some(other) => base.into_iter().filter(|c| other.contains(c)).collect(),
        None => base,
    }
}

/// The per-role report and the advertised intersection. `previous_pin` is
/// the holder's answer to "is a rollback pin configured" (`None` when not
/// split, or the status round trip failed: treated as configured, since an
/// unknown pin must not widen the list).
pub fn report(previous_pin: Option<bool>) -> Value {
    let brain = brain();
    let holder = holder();
    let pin = match (holder.is_some(), previous_pin) {
        (false, _) | (true, Some(false)) => None,
        (true, _) => Some(pin_from(&ledger_path(), exe_digest().as_deref())),
    };
    let advertised = intersect(intersect(brain.clone(), holder.as_deref()), pin.as_deref());
    json!({
        "brain": brain,
        "holder": holder,
        "rollback_pin": pin,
        "advertised": advertised,
    })
}

/// What this host advertises, for the heartbeat. One holder status round
/// trip; not for hot paths.
pub fn advertised() -> Vec<String> {
    let previous_pin = crate::holder_mode::global()
        .and_then(|client| client.status().ok())
        .map(|status| status.previous_pin);
    report(previous_pin)["advertised"]
        .as_array()
        .map(|caps| caps.iter().filter_map(|c| c.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_the_designed_four() {
        assert_eq!(KNOWN, ["org_preserve_v1", "lifecycle_guard_v1", "org_stamp_v1", "occupancy_v1"]);
        assert_eq!(brain(), vec![ORG_PRESERVE_V1.to_string()]);
    }

    #[test]
    fn the_pin_is_the_last_other_binary_and_unknown_means_none() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("brain-capabilities.json");
        assert!(pin_from(&ledger, Some("new")).is_empty(), "no record: a pre-S0b pin");
        record_in(&ledger, "old", &[]).unwrap();
        record_in(&ledger, "new", &brain()).unwrap();
        assert!(pin_from(&ledger, Some("new")).is_empty(), "the pin predates S0b");
        // The next deploy pins an S0b build; a crash respawn of the same
        // binary does not count as a different pin.
        record_in(&ledger, "newer", &brain()).unwrap();
        assert_eq!(pin_from(&ledger, Some("newer")), brain());
        record_in(&ledger, "newer", &brain()).unwrap();
        assert_eq!(pin_from(&ledger, Some("newer")), brain());
    }

    #[test]
    fn advertised_is_the_intersection_of_the_roles() {
        let caps = brain();
        assert_eq!(intersect(caps.clone(), None), caps);
        assert!(intersect(caps.clone(), Some(&[])).is_empty(), "an old holder preserves nothing");
        assert_eq!(intersect(caps.clone(), Some(&caps)), caps);
    }

    #[test]
    fn a_monolith_advertises_the_brain_list() {
        // Not split: no holder, no pin to check.
        let report = report(None);
        assert_eq!(report["advertised"], json!(brain()));
        assert!(report["holder"].is_null());
        assert!(report["rollback_pin"].is_null());
    }
}
