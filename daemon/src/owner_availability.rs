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

/// How much a request to Owner matters, least to most.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Urgency {
    Fyi,
    Decision,
    Blocking,
    Emergency,
}

impl Urgency {
    pub const ALL: [Urgency; 4] = [Urgency::Fyi, Urgency::Decision, Urgency::Blocking, Urgency::Emergency];

    pub fn parse(s: &str) -> Option<Urgency> {
        Self::ALL.into_iter().find(|u| u.as_str() == s)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Urgency::Fyi => "fyi",
            Urgency::Decision => "decision",
            Urgency::Blocking => "blocking",
            Urgency::Emergency => "emergency",
        }
    }
}

/// The least urgency a level delivers immediately (Owner decision
/// 2026-10-06): away → emergency, around → blocking, focused → decision,
/// on-call → everything.
pub fn bar(level: Level) -> Urgency {
    match level {
        Level::Away => Urgency::Emergency,
        Level::Around => Urgency::Blocking,
        Level::Focused => Urgency::Decision,
        Level::OnCall => Urgency::Fyi,
    }
}

/// Unset delivers everything (legacy behavior).
pub fn delivers(level: Option<Level>, urgency: Urgency) -> bool {
    level.is_none_or(|l| urgency >= bar(l))
}

/// The least reachable level at which `urgency` is delivered.
pub fn release_level(urgency: Urgency) -> Level {
    Level::ALL
        .into_iter()
        .find(|l| urgency >= bar(*l))
        .unwrap_or(Level::OnCall)
}

/// `(level, event_id)` from the projection; `(None, None)` when unset or
/// unreadable.
pub fn current(cm_root: &Path) -> (Option<Level>, Option<String>) {
    let record = std::fs::read(projection_path(cm_root))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or(Value::Null);
    (
        record["level"].as_str().and_then(Level::parse),
        record["event_id"].as_str().map(str::to_owned),
    )
}

fn applied_path(cm_root: &Path) -> PathBuf {
    cm_root.join("owner-availability-applied.json")
}

/// Delivery-worker hook: when the replicated level changed since this daemon
/// last applied it (also after restarts and for changes that arrived from
/// the hub), release held Owner requests the new level delivers and wake the
/// sessions that coordinate work. Runs on every host; cheap when unchanged
/// (one small file read).
pub fn tick(state: &std::sync::Arc<std::sync::Mutex<crate::state::DaemonState>>) {
    let root = state.lock().unwrap_or_else(|p| p.into_inner()).messaging_root.clone();
    let (level, event_id) = current(&root);
    let applied: Value = std::fs::read(applied_path(&root))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let first_run = applied.is_null();
    if !first_run && applied["event_id"].as_str() == event_id.as_deref() {
        return;
    }
    let previous = applied["level"].as_str().and_then(Level::parse);
    // A failing release (unreadable/unwritable held store) retries with
    // backoff rather than on every 2 s tick.
    static RETRY: std::sync::Mutex<(u64, u64)> = std::sync::Mutex::new((0, 0)); // (next_at, delay)
    let now = crate::continuous::task::now_unix();
    if RETRY.lock().unwrap_or_else(|p| p.into_inner()).0 > now {
        return;
    }
    let outcome = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        if s.draining {
            return;
        }
        crate::owner_attention::release_for_level(&s, level)
    };
    let outcome = match outcome {
        Ok(o) => {
            *RETRY.lock().unwrap_or_else(|p| p.into_inner()) = (0, 0);
            o
        }
        Err(e) => {
            let mut retry = RETRY.lock().unwrap_or_else(|p| p.into_inner());
            let delay = (retry.1 * 2).clamp(10, 600);
            *retry = (now + delay, delay);
            eprintln!("cm owner availability: release held requests (retry in {delay}s): {e}");
            return;
        }
    };
    // First run after an upgrade adopts the current level without waking
    // anyone (nothing was held under the old daemon); re-setting the same
    // level is a new revision but no change, so it wakes nobody either.
    if !first_run && previous != level {
        wake(state, &root, previous, level, event_id.as_deref().unwrap_or("unset"), &outcome);
    }
    let record = json!({"event_id": event_id, "level": level.map(Level::as_str)});
    if let Err(e) = crate::state::write_json_atomic(&applied_path(&root), &record.to_string(), true) {
        eprintln!("cm owner availability: record applied level: {e}");
    }
}

/// Wake only the sessions this change affects: those with requests that
/// were released or are still held. Everyone else reads the level from
/// `ping().owner_availability` when they next need it (waking every
/// orchestrator per change produced notices Owner paid for in stall alerts).
fn wake(
    state: &std::sync::Arc<std::sync::Mutex<crate::state::DaemonState>>,
    root: &Path,
    previous: Option<Level>,
    level: Option<Level>,
    revision: &str,
    outcome: &crate::owner_attention::ReleaseOutcome,
) {
    let name = |l: Option<Level>| l.map(Level::as_str).unwrap_or("unset");
    let targets: std::collections::BTreeSet<&String> =
        outcome.released.keys().chain(outcome.still_held.keys()).collect();
    if targets.is_empty() {
        return;
    }
    let live: std::collections::BTreeSet<String> = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        s.sessions.keys().cloned().collect()
    };
    let at = Utc::now().format("%H:%MZ");
    // The marker must be in the text: the consumer confirms delivery by
    // finding it in the agent's transcript.
    let marker = format!("[cm-owner-availability {revision}]");
    for uid in targets.into_iter().filter(|u| live.contains(*u)) {
        let released = outcome.released.get(uid).copied().unwrap_or(0);
        let held = outcome.still_held.get(uid).copied().unwrap_or(0);
        let text = format!(
            "{marker} Owner availability: {}\u{2192}{} at {at}. {released} of your requests were released; {held} still held. Check ping().owner_availability before asking Owner; continue your task.",
            name(previous),
            name(level)
        );
        if let Err(e) = crate::notifications::publish(
            root,
            uid,
            &format!("owner-availability:{revision}"),
            "owner",
            &text,
            &marker,
        ) {
            eprintln!("cm owner availability: wake {uid}: {e}");
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

/// Recompute `age_s` of an exposure value (e.g. one a viewer received over
/// RPC) from its `changed_at`, for display.
pub fn with_current_age(exposure: &Value) -> Value {
    let mut v = exposure.clone();
    if let Some(t) = v["changed_at"].as_str().and_then(|t| DateTime::parse_from_rfc3339(t).ok()) {
        v["age_s"] = json!((Utc::now() - t.with_timezone(&Utc)).num_seconds().max(0));
    }
    v
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
    fn bars_follow_the_owner_mapping() {
        use Urgency::*;
        for (level, delivered) in [
            (Level::Away, vec![Emergency]),
            (Level::Around, vec![Blocking, Emergency]),
            (Level::Focused, vec![Decision, Blocking, Emergency]),
            (Level::OnCall, vec![Fyi, Decision, Blocking, Emergency]),
        ] {
            for u in Urgency::ALL {
                assert_eq!(delivers(Some(level), u), delivered.contains(&u), "{level:?} {u:?}");
            }
        }
        assert!(Urgency::ALL.into_iter().all(|u| delivers(None, u)));
        assert_eq!(release_level(Emergency), Level::Away);
        assert_eq!(release_level(Blocking), Level::Around);
        assert_eq!(release_level(Decision), Level::Focused);
        assert_eq!(release_level(Fyi), Level::OnCall);
    }

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
