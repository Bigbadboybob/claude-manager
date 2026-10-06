//! Owner availability: how reachable Owner is right now, set by Owner (TUI or
//! `scripts/cm-availability`) and read by every agent through `ping` and
//! `chat_open`.
//!
//! The level is a replicated messaging event (`owner.availability`, actor
//! `owner`, no conversation), published on the messaging hub and delivered to
//! every enrolled host like other conversation-less events. Each daemon
//! reduces the latest one and projects it to a small file beside its store,
//! so reading it (`ping`) needs no messaging lock and no network, and the last
//! known value survives a hub outage and restarts.
//!
//! Unset means legacy behavior: every Owner alert is delivered.
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Availability levels, least to most reachable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Away,
    Around,
    Focused,
    OnCall,
}

impl Level {
    pub const ALL: [Level; 4] = [Level::Away, Level::Around, Level::Focused, Level::OnCall];

    pub fn parse(s: &str) -> Option<Level> {
        Self::ALL.into_iter().find(|l| l.as_str() == s)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Level::Away => "away",
            Level::Around => "around",
            Level::Focused => "focused",
            Level::OnCall => "on-call",
        }
    }
}

/// The messaging event type carrying a level change.
pub const EVENT_TYPE: &str = "owner.availability";
/// Longest accepted note, in characters.
pub const NOTE_MAX_CHARS: usize = 200;

/// Where a daemon's messaging store projects the current level.
pub fn projection_path(cm_root: &Path) -> PathBuf {
    cm_root.join("messages/main/OWNER_AVAILABILITY.json")
}

/// Check an event's level field: a known level, or null for "unset".
pub fn valid_level(v: &Value) -> bool {
    v.is_null() || v.as_str().is_some_and(|s| Level::parse(s).is_some())
}

/// What agents see: `{level, set, changed_at, age_s, previous, note,
/// source}`. `record` is the reduced event state (Null when never set).
pub fn exposure_at(record: &Value, now: DateTime<Utc>) -> Value {
    if record.is_null() || record["level"].is_null() {
        return json!({
            "level": null,
            "set": false,
            "changed_at": record.get("changed_at").cloned().unwrap_or(Value::Null),
            "note": "Owner has not set an availability level; every Owner alert is delivered",
        });
    }
    let age = record["changed_at"]
        .as_str()
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| (now - t.with_timezone(&Utc)).num_seconds().max(0));
    let mut out = json!({
        "level": record["level"],
        "set": true,
        "changed_at": record["changed_at"],
        "age_s": age,
        "previous": record["previous"],
        "source": record["source"],
    });
    if let Some(note) = record["note"].as_str().filter(|n| !n.is_empty()) {
        out["owner_note"] = json!(note);
    }
    out
}

/// Read the projected level for `ping`: lock-free and network-free. A missing
/// or unreadable projection reads as unset.
pub fn exposure(cm_root: &Path) -> Value {
    let record = std::fs::read(projection_path(cm_root))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or(Value::Null);
    exposure_at(&record, Utc::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_order_and_round_trip() {
        assert!(Level::Away < Level::Around && Level::Focused < Level::OnCall);
        for l in Level::ALL {
            assert_eq!(Level::parse(l.as_str()), Some(l));
        }
        assert_eq!(Level::parse("busy"), None);
        assert!(valid_level(&Value::Null) && valid_level(&json!("on-call")) && !valid_level(&json!(3)));
    }

    #[test]
    fn exposure_reports_age_and_unset() {
        let now = DateTime::parse_from_rfc3339("2026-10-06T14:16:00Z").unwrap().with_timezone(&Utc);
        let set = exposure_at(
            &json!({"level":"focused","changed_at":"2026-10-06T14:02:00Z","previous":"away","source":"tui","note":"deep work"}),
            now,
        );
        assert_eq!(set["level"], "focused");
        assert_eq!(set["age_s"], 840);
        assert_eq!(set["owner_note"], "deep work");
        let unset = exposure_at(&Value::Null, now);
        assert_eq!(unset["set"], false);
        assert!(unset["level"].is_null());
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(exposure(tmp.path())["set"], false);
    }
}
