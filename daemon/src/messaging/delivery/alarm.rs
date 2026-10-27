//! Owner alarm for a native notification path that silently stopped working.
//!
//! 2026-10-06: a Claude fork/hand-off left the OLD client's MCP server holding
//! the consumer lock; 66 notices sat `submitted` (never observed) for seven
//! hours while every status read reported connected/ready. The agent-side
//! consumer is the part that can be broken, so the daemon watches the shared
//! queue files itself and raises one Owner alert per stall episode.
use crate::state::DaemonState;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// Default stall horizon; `CM_NOTIFY_STALL_ALERT_SECS` overrides it.
pub const DEFAULT_STALL_ALERT_SECS: u64 = 900;
/// How often the independent alarm worker evaluates the queue.
pub const ALARM_INTERVAL_SECS: u64 = 30;

pub fn threshold_secs() -> u64 {
    std::env::var("CM_NOTIFY_STALL_ALERT_SECS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|v: &u64| *v > 0)
        .unwrap_or(DEFAULT_STALL_ALERT_SECS)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stall {
    pub unobserved: usize,
    pub oldest_secs: u64,
    pub reason: String,
}

fn at(e: &Value) -> f64 {
    e["updated_at"]
        .as_f64()
        .or_else(|| e["created_at"].as_f64())
        .unwrap_or(0.0)
}

fn transport_reason(transport: Option<&Value>) -> Option<String> {
    let t = transport?;
    if t["status"] != "degraded" {
        return None;
    }
    t["reason"]
        .as_str()
        .or_else(|| t["reasons"][0].as_str())
        .map(str::to_owned)
}

fn receipt_evidence_allowed(event: &Value) -> bool {
    if event["source"] != "board" { return true; }
    let Some(id) = event["id"].as_str().and_then(|s| s.strip_prefix("board-push:")) else { return false };
    let Some(marker) = event["marker"].as_str() else { return false };
    !id.is_empty() && marker.starts_with("[cm-board ") && marker.ends_with(&format!(" push:{id}]"))
}

/// Same rule as `mcp_server/notifications.py::delivery_health`: submissions
/// newer than the latest observation, unobserved past the horizon. A later
/// observation proves the path works again and ends the episode. A consumer
/// that knows its client drops events (channels disabled) keeps them pending,
/// so pending work under such a degraded transport stalls too.
pub fn assess(events: &[Value], transport: Option<&Value>, now: f64, threshold: u64) -> Option<Stall> {
    let last_observed = events
        .iter()
        .filter(|e| e["status"] == "observed" && receipt_evidence_allowed(e))
        .map(at)
        .fold(0.0, f64::max);
    let reason = transport_reason(transport);
    let blocked = matches!(reason.as_deref(), Some("channels_disabled" | "stale_channel" | "external_background"));
    let stalled: Vec<f64> = events
        .iter()
        .filter(|e| {
            let status = e["status"].as_str().unwrap_or("");
            status == "submitting"
                || (matches!(status, "submitted" | "uncertain") && at(e) > last_observed)
                || (blocked && status == "pending")
        })
        .map(at)
        .collect();
    let oldest = stalled.iter().copied().fold(f64::INFINITY, f64::min);
    if !oldest.is_finite() {
        return None;
    }
    let age = (now - oldest).max(0.0) as u64;
    (age >= threshold).then(|| Stall {
        unobserved: stalled.len(),
        oldest_secs: age,
        reason: reason.unwrap_or_else(|| "unobserved_submissions".into()),
    })
}

fn human(secs: u64) -> String {
    match secs {
        s if s >= 3600 => format!("{}h {}m", s / 3600, (s % 3600) / 60),
        s if s >= 60 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

pub fn message(label: &str, uid: &str, stall: &Stall) -> String {
    let recovery = if stall.reason == "external_background" {
        "Claude lists the bound conversation as background while this notification channel is not its background-host child; inspect the exact native ID, host process, and active work before an operator decides whether to stop that specific session and revive it"
    } else if stall.reason == "channels_disabled" {
        "its Claude client was relaunched without the claude-manager channel (e.g. a bg/fork hand-off); restart/resume it in place with A-R"
    } else {
        "inspect the session and notification_status; reconnect MCP only if the live client still owns the channel, otherwise restart/resume it in place with A-R"
    };
    format!(
        "CM notifications are not reaching {label} ({uid}): {} notice(s) unobserved for {} (reason: {}). Recovery: {recovery}.",
        stall.unobserved,
        human(stall.oldest_secs),
        stall.reason
    )
}

#[derive(Default, Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct Episode {
    pub since: u64,
    pub reason: String,
    /// Set once the Owner alert was raised; never re-raised in this episode.
    pub alert_id: Option<String>,
    /// Absent in pre-independent-key journals.
    pub alert_key: Option<String>,
}

fn state_path(root: &Path) -> PathBuf {
    // Beside (not inside) the per-recipient queue directories, whose *.json
    // files the MCP consumer parses as events.
    root.join("notifications").join("stall-alarms.state")
}

#[cfg(test)]
fn load(root: &Path) -> BTreeMap<String, Episode> {
    snapshot(root).unwrap_or_default()
}

/// Durable stall state used by the viewer. Corruption is unknown rather than
/// an empty healthy snapshot; callers may keep their last known state.
pub fn snapshot(root: &Path) -> io::Result<BTreeMap<String, Episode>> {
    match fs::read(state_path(root)) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e),
    }
}

fn save(root: &Path, episodes: &BTreeMap<String, Episode>) -> io::Result<()> {
    fs::create_dir_all(root.join("notifications"))?;
    crate::messaging::atomic_replace(&state_path(root), &serde_json::to_value(episodes)?)
}

/// One alarm pass over the given live native recipients. File reads run
/// without the daemon lock; Owner-alert writes take it briefly.
pub fn pass(state: &Arc<Mutex<DaemonState>>, root: &Path, uids: &[String], now: f64, threshold: u64) {
    let mut episodes = match snapshot(root) {
        Ok(episodes) => episodes,
        Err(e) => { eprintln!("cm notifications: stall alarm state unavailable: {e}"); return; }
    };
    let before = episodes.clone();
    for uid in uids {
        // Legacy retirement remains narrow; the complete census and
        // retirement share one nonblocking lock and never prove delivery.
        let stall = match crate::notifications::alarm_snapshot(root, uid, now) {
            Ok((events, transport)) => {
                let assessed = assess(&events, transport.as_ref(), now, threshold);
                // Pending work was held because the transport was known to
                // drop it. A missing/changed heartbeat does not prove that
                // those notices reached the recipient.
                if assessed.is_none()
                    && episodes.get(uid).is_some_and(|e| matches!(e.reason.as_str(), "channels_disabled" | "stale_channel" | "external_background"))
                    && events.iter().any(|e| matches!(e["status"].as_str(), Some("pending" | "submitting")))
                {
                    continue;
                }
                assessed
            }
            Err(_) => continue,
        };
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        if s.draining {
            return;
        }
        match stall {
            Some(stall) => {
                let episode = episodes.entry(uid.clone()).or_insert_with(|| Episode {
                    since: now as u64,
                    reason: stall.reason.clone(),
                    alert_id: None,
                    alert_key: None,
                });
                let key = format!("notification-stall:{uid}");
                // A legacy ID means this episode already alerted. Keep that
                // ID/key even if it was acknowledged or replaced; moving it
                // would re-alert an ongoing episode. Only a suppressed legacy
                // episode (no ID) needs its independent key now.
                if episode.alert_key.is_none() {
                    episode.alert_key = Some(if episode.alert_id.is_some() { uid.clone() } else { key.clone() });
                }
                if episode.alert_id.is_none() {
                    let label = s
                        .sessions
                        .get(uid)
                        .map(|x| x.title.clone())
                        .or_else(|| s.tui_sessions.get(uid).and_then(|x| x.label.clone()))
                        .unwrap_or_else(|| uid.clone());
                    let escalation = crate::owner_attention::Escalation {
                        source: key.clone(), dedupe_key: key,
                        urgency: crate::owner_availability::Urgency::Blocking,
                        summary: message(&label, uid, &stall),
                        session_uid: Some(uid.clone()), task_id: None,
                    };
                    match crate::owner_attention::escalate(&s, escalation) {
                        Ok(crate::owner_attention::GateOutcome::Delivered { alert_id: id }
                            | crate::owner_attention::GateOutcome::Coalesced { alert_id: id }) => {
                            eprintln!("cm notifications: stall alert for {uid}: {} unobserved for {}s ({})", stall.unobserved, stall.oldest_secs, stall.reason);
                            episode.alert_id = Some(id);
                            episode.reason = stall.reason.clone();
                        }
                        Err(e) => eprintln!("cm notifications: stall alert for {uid}: {e}"),
                    }
                }
                if episode.reason != stall.reason {
                    if let Some(id) = episode.alert_id.as_deref() {
                        let label = s.sessions.get(uid).map(|x| x.title.clone())
                            .or_else(|| s.tui_sessions.get(uid).and_then(|x| x.label.clone()))
                            .unwrap_or_else(|| uid.clone());
                        let key = episode.alert_key.as_deref().unwrap_or(uid);
                        if let Err(e) = crate::owner_attention::update_system_message(&s, key, id, &message(&label, uid, &stall)) {
                            eprintln!("cm notifications: update stall alert for {uid}: {e}");
                        }
                    }
                    episode.reason = stall.reason;
                }
            }
            None => {
                if let Some(episode) = episodes.remove(uid) {
                    if let Some(id) = episode.alert_id {
                        // Old episodes used the session key; newer ones use a
                        // separate key so an agent's alert cannot mask this one.
                        let key = episode.alert_key.as_deref().unwrap_or(uid);
                        if let Err(e) = crate::owner_attention::withdraw(&s, key, &id) {
                            eprintln!("cm notifications: clear stall alert for {uid}: {e}");
                        }
                    }
                }
            }
        }
    }
    // Sessions that left the live set end their episode without touching
    // alerts: an exited row's alert stays for Owner to acknowledge.
    episodes.retain(|uid, _| uids.contains(uid));
    if episodes != before {
        // Snapshot subscribers take the same state lock: save before
        // broadcasting, so a reconnect cannot miss an episode transition.
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        if let Err(e) = save(root, &episodes) {
            eprintln!("cm notifications: stall alarm state: {e}");
        } else {
            for uid in before.keys().chain(episodes.keys()).collect::<std::collections::BTreeSet<_>>() {
                if before.get(uid) != episodes.get(uid) {
                    s.manifest_watcher.broadcast(crate::manifest::ManifestDiff::Updated {
                        uid: uid.clone(),
                        entry: serde_json::json!({"notification_health": episodes.get(uid)}),
                    });
                }
            }
        }
    }
}

/// A session's own messages pending_sync this long raise a sender-side alarm.
pub const OUTBOX_ALERT_SECS: i64 = 900;

fn outbox_state_path(root: &Path) -> PathBuf {
    root.join("notifications").join("outbox-alarms.state")
}

/// Map key of the host-wide outbox alarm.
pub const OUTBOX_KEY: &str = "outbox";

/// Sender-side alarm (EP A2): messages this host's sessions sent that the hub
/// has not accepted for `threshold` seconds. A stalled hub link affects every
/// sender at once, so this is ONE blocking escalation per host per episode
/// (never one per session), withdrawn when every outbox drains. `outboxes`
/// pairs each live session with its overdue outbox (None when clear).
pub fn outbox_pass(
    state: &Arc<Mutex<DaemonState>>,
    root: &Path,
    outboxes: &[(String, Option<Value>)],
    threshold: i64,
) {
    let mut episode: Option<Episode> = fs::read(outbox_state_path(root))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    let before = episode.clone();
    let stalled: Vec<(&String, &Value)> = outboxes
        .iter()
        .filter_map(|(uid, o)| o.as_ref().map(|o| (uid, o)))
        .filter(|(_, o)| o["oldest_age_s"].as_i64().is_some_and(|a| a >= threshold))
        .collect();
    let s = state.lock().unwrap_or_else(|p| p.into_inner());
    if s.draining {
        return;
    }
    if stalled.is_empty() {
        if let Some(Episode { alert_id: Some(id), .. }) = episode.take() {
            if let Err(e) = crate::owner_attention::withdraw(&s, OUTBOX_KEY, &id) {
                eprintln!("cm messaging: clear outbox alarm: {e}");
            }
        }
    } else {
        let ep = episode.get_or_insert_with(|| Episode {
            since: crate::continuous::task::now_unix(),
            reason: "pending_sync".into(),
            alert_id: None,
            alert_key: None,
        });
        if ep.alert_id.is_none() {
            let oldest = stalled.iter().filter_map(|(_, o)| o["oldest_age_s"].as_i64()).max().unwrap_or(0).max(0) as u64;
            let pending: i64 = stalled.iter().filter_map(|(_, o)| o["pending_sync"].as_i64()).sum();
            let names: Vec<String> = stalled
                .iter()
                .take(5)
                .map(|(uid, _)| s.sessions.get(*uid).map(|x| x.title.clone()).unwrap_or_else(|| (*uid).clone()))
                .collect();
            let summary = format!(
                "Messages from {} session(s) on this host have waited up to {} for the messaging hub ({pending} pending; {}{}). The hub link may be stalled; check chat_open().sync here and the hub daemon.",
                stalled.len(),
                human(oldest),
                names.join(", "),
                if stalled.len() > names.len() { ", ..." } else { "" },
            );
            let e = crate::owner_attention::Escalation {
                source: OUTBOX_KEY.into(),
                dedupe_key: OUTBOX_KEY.into(),
                urgency: crate::owner_availability::Urgency::Blocking,
                summary,
                session_uid: None,
                task_id: None,
            };
            match crate::owner_attention::escalate(&s, e) {
                Ok(crate::owner_attention::GateOutcome::Delivered { alert_id }) => {
                    eprintln!("cm messaging: outbox alarm: {} sender(s), oldest pending {oldest}s", stalled.len());
                    ep.alert_id = Some(alert_id);
                }
                Ok(crate::owner_attention::GateOutcome::Coalesced { .. }) => {}
                Err(e) => eprintln!("cm messaging: outbox alarm: {e}"),
            }
        }
    }
    if episode != before {
        let saved = fs::create_dir_all(root.join("notifications")).and_then(|_| match &episode {
            Some(ep) => crate::messaging::atomic_replace(&outbox_state_path(root), &serde_json::to_value(ep)?),
            None => match fs::remove_file(outbox_state_path(root)) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            },
        });
        if let Err(e) = saved {
            eprintln!("cm messaging: outbox alarm state: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(id: &str, status: &str, at: f64) -> Value {
        json!({"version":1,"id":id,"status":status,"created_at":at,"updated_at":at,"marker":id})
    }

    #[test]
    fn notifications_stall_assessment_follows_latest_observation() {
        let now = 10_000.0;
        // Fresh submission: not yet a stall.
        assert_eq!(assess(&[ev("a", "submitted", now - 60.0)], None, now, 900), None);
        // Unobserved past the horizon.
        let stall = assess(&[ev("a", "submitted", now - 1000.0), ev("b", "uncertain", now - 950.0)], None, now, 900).unwrap();
        assert_eq!((stall.unobserved, stall.oldest_secs, stall.reason.as_str()), (2, 1000, "unobserved_submissions"));
        // A later observation ends the episode; earlier losses stay history.
        assert_eq!(assess(&[ev("a", "submitted", now - 1000.0), ev("b", "observed", now - 10.0)], None, now, 900), None);
        // Consumer-reported reason is carried into the alert.
        let t = json!({"status":"degraded","reason":"transcript_mismatch"});
        assert_eq!(assess(&[ev("a", "submitted", now - 1000.0)], Some(&t), now, 900).unwrap().reason, "transcript_mismatch");
        // Pending only counts when the consumer reports its client drops events.
        assert_eq!(assess(&[ev("a", "pending", now - 5000.0)], None, now, 900), None);
        assert_eq!(assess(&[ev("a", "submitting", now - 899.0)], None, now, 900), None);
        assert!(assess(&[ev("a", "submitting", now - 901.0), ev("b", "observed", now - 1.0)], None, now, 900).is_some());
        let blocked = json!({"status":"degraded","reason":"channels_disabled"});
        let stall = assess(&[ev("a", "pending", now - 5000.0)], Some(&blocked), now, 900).unwrap();
        assert_eq!(stall.reason, "channels_disabled");
        assert!(message("worker", "uid-1", &stall).contains("A-R"));
        let legacy = json!({"id":"board-push:1", "source":"board", "status":"observed", "marker":"[cm-board sfd]", "updated_at":now - 1.0});
        assert!(assess(&[legacy.clone(), ev("later", "submitted", now - 1000.0)], None, now, 900).is_some());
        let mut unique = legacy;
        unique["marker"] = json!("[cm-board sfd push:1]");
        assert!(assess(&[unique, ev("later", "submitted", now - 1000.0)], None, now, 900).is_none());
    }

    #[test]
    fn notifications_stall_alarm_raises_once_per_episode_and_clears() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut daemon = DaemonState::new();
        daemon.daemon_sessions_path = Some(root.join("daemon-sessions.json"));
        let state = Arc::new(Mutex::new(daemon));
        let uid = "ts-stalled".to_string();
        crate::notifications::publish(root, &uid, "chat:one", "chat", "[cm-chat one] wake", "[cm-chat one]").unwrap();
        let queue = crate::notifications::directory(root, &uid);
        let event_file = |id: &str| {
            use sha2::Digest;
            queue.join(format!("{:x}.json", sha2::Sha256::digest(id.as_bytes())))
        };
        let mut event: Value = serde_json::from_slice(&fs::read(event_file("chat:one")).unwrap()).unwrap();
        event["created_at"] = json!(1.0);
        event["status"] = json!("submitted");
        event["updated_at"] = json!(1_000.0);
        crate::messaging::atomic_replace(&event_file("chat:one"), &event).unwrap();
        let uids = vec![uid.clone()];
        let alerts = |s: &Arc<Mutex<DaemonState>>| crate::owner_attention::snapshot(&s.lock().unwrap()).unwrap();

        pass(&state, root, &uids, 1_100.0, 900);
        assert!(alerts(&state).is_empty(), "inside the horizon");
        pass(&state, root, &uids, 2_000.0, 900);
        let key = format!("notification-stall:{uid}");
        let first = alerts(&state)[&key].clone();
        assert!(first.message.contains("ts-stalled") && first.message.contains("16m"), "{}", first.message);
        let transport = queue.join("transport.json");
        crate::messaging::atomic_replace(&transport, &json!({"status":"degraded","reason":"external_background"})).unwrap();
        pass(&state, root, &uids, 2_010.0, 900);
        let refreshed = alerts(&state)[&key].clone();
        assert_eq!(refreshed.id, first.id, "new evidence updates the same alert");
        assert!(refreshed.message.contains("background"));
        fs::remove_file(transport).unwrap();
        // A partial census cannot falsely resolve the episode.
        let bad = queue.join("corrupt.json");
        event["status"] = json!("observed");
        event["updated_at"] = json!(2_050.0);
        crate::messaging::atomic_replace(&event_file("chat:one"), &event).unwrap();
        fs::write(&bad, b"{").unwrap();
        pass(&state, root, &uids, 2_100.0, 900);
        assert_eq!(alerts(&state)[&key].id, first.id);
        assert!(load(root).contains_key(&uid));
        fs::write(&bad, br#"{"id":"bad","status":"bogus","created_at":1}"#).unwrap();
        pass(&state, root, &uids, 2_100.0, 900);
        assert_eq!(alerts(&state)[&key].id, first.id);
        fs::write(&bad, br#"{"id":"bad","status":"submitted"}"#).unwrap();
        pass(&state, root, &uids, 2_100.0, 900);
        assert_eq!(alerts(&state)[&key].id, first.id);
        fs::remove_file(bad).unwrap();
        event["status"] = json!("submitted");
        event["updated_at"] = json!(1_000.0);
        crate::messaging::atomic_replace(&event_file("chat:one"), &event).unwrap();
        // Owner acknowledges; the same episode never re-alerts.
        crate::owner_attention::withdraw(&state.lock().unwrap(), &key, &first.id).unwrap();
        pass(&state, root, &uids, 3_000.0, 900);
        assert!(alerts(&state).is_empty());
        // Observation clears the episode; a new stall raises a new alert.
        event["status"] = json!("observed");
        event["updated_at"] = json!(3_100.0);
        crate::messaging::atomic_replace(&event_file("chat:one"), &event).unwrap();
        pass(&state, root, &uids, 3_200.0, 900);
        assert!(load(root).is_empty());
        crate::notifications::publish(root, &uid, "chat:two", "chat", "[cm-chat two] wake", "[cm-chat two]").unwrap();
        let mut two: Value = serde_json::from_slice(&fs::read(event_file("chat:two")).unwrap()).unwrap();
        two["created_at"] = json!(1.0);
        two["status"] = json!("submitted");
        two["updated_at"] = json!(3_300.0);
        crate::messaging::atomic_replace(&event_file("chat:two"), &two).unwrap();
        pass(&state, root, &uids, 4_300.0, 900);
        let second = alerts(&state)[&key].clone();
        assert_ne!(second.id, first.id);
        // Recovery withdraws exactly the alarm's own alert.
        two["status"] = json!("observed");
        two["updated_at"] = json!(4_400.0);
        crate::messaging::atomic_replace(&event_file("chat:two"), &two).unwrap();
        pass(&state, root, &uids, 4_500.0, 900);
        assert!(alerts(&state).is_empty());
    }

    #[test]
    fn outbox_alarm_is_one_per_host_per_episode_and_withdraws_when_drained() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut daemon = DaemonState::new();
        daemon.daemon_sessions_path = Some(root.join("daemon-sessions.json"));
        daemon.messaging_root = root.to_path_buf();
        let state = Arc::new(Mutex::new(daemon));
        let alerts = |s: &Arc<Mutex<DaemonState>>| crate::owner_attention::snapshot(&s.lock().unwrap()).unwrap();
        let fresh = vec![("ts-a".to_string(), Some(json!({"pending_sync":1,"oldest_age_s":120})))];
        outbox_pass(&state, root, &fresh, OUTBOX_ALERT_SECS);
        assert!(alerts(&state).is_empty(), "inside the horizon");
        // A hub outage stalls every sender: one alert for the host.
        let stuck = vec![
            ("ts-a".to_string(), Some(json!({"pending_sync":3,"oldest_age_s":1800}))),
            ("ts-b".to_string(), Some(json!({"pending_sync":2,"oldest_age_s":1000}))),
            ("ts-c".to_string(), None),
        ];
        outbox_pass(&state, root, &stuck, OUTBOX_ALERT_SECS);
        let all = alerts(&state);
        assert_eq!(all.len(), 1);
        let first = all[OUTBOX_KEY].clone();
        assert!(first.message.contains("2 session(s)") && first.message.contains("30m") && first.message.contains("5 pending"), "{}", first.message);
        assert_eq!(first.urgency.as_deref(), Some("blocking"));
        // Same episode: Owner acknowledged, never re-raised.
        crate::owner_attention::clear_system(&state.lock().unwrap(), OUTBOX_KEY, &first.id).unwrap();
        outbox_pass(&state, root, &stuck, OUTBOX_ALERT_SECS);
        assert!(alerts(&state).is_empty());
        // Drained: the episode ends; a new stall raises again and drains clean.
        let clear = vec![("ts-a".to_string(), None)];
        outbox_pass(&state, root, &clear, OUTBOX_ALERT_SECS);
        outbox_pass(&state, root, &stuck, OUTBOX_ALERT_SECS);
        assert_ne!(alerts(&state)[OUTBOX_KEY].id, first.id);
        outbox_pass(&state, root, &clear, OUTBOX_ALERT_SECS);
        assert!(alerts(&state).is_empty());
    }
    #[test]
    fn notifications_stall_alarm_is_independent_of_an_agent_alert() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut daemon = DaemonState::new();
        daemon.daemon_sessions_path = Some(root.join("daemon-sessions.json"));
        let state = Arc::new(Mutex::new(daemon));
        let uid = "ts-busy".to_string();
        let existing = crate::owner_attention::raise_system(&state.lock().unwrap(), &uid, "Decision needed").unwrap().unwrap();
        crate::notifications::publish(root, &uid, "chat:one", "chat", "[cm-chat one] wake", "[cm-chat one]").unwrap();
        let queue = crate::notifications::directory(root, &uid);
        let file = fs::read_dir(queue).unwrap().flatten().map(|e| e.path()).find(|p| p.extension().is_some_and(|e| e == "json")).unwrap();
        let mut event: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        event["created_at"] = json!(1.0);
        event["status"] = json!("submitted");
        event["updated_at"] = json!(1000.0);
        crate::messaging::atomic_replace(&file, &event).unwrap();
        pass(&state, root, std::slice::from_ref(&uid), 2000.0, 900);
        let alerts = crate::owner_attention::snapshot(&state.lock().unwrap()).unwrap();
        assert_eq!(alerts[&uid].id, existing);
        assert_eq!(alerts[&format!("notification-stall:{uid}")].session_uid, uid);
    }

    #[test]
    fn alarm_preserves_an_ambiguous_confirmable_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut daemon = DaemonState::new();
        daemon.daemon_sessions_path = Some(root.join("daemon-sessions.json"));
        let state = Arc::new(Mutex::new(daemon));
        let uid = "ts-ambiguous".to_string();
        crate::notifications::publish(root, &uid, "one", "chat", "[one] wake", "[one]").unwrap();
        let queue = crate::notifications::directory(root, &uid);
        let file = fs::read_dir(queue).unwrap().flatten().map(|e| e.path()).find(|p| p.extension().is_some_and(|e| e == "json")).unwrap();
        let mut event: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        event["created_at"] = json!(1.0);
        event["status"] = json!("uncertain");
        event["updated_at"] = json!(1000.0);
        crate::messaging::atomic_replace(&file, &event).unwrap();
        let original = fs::read(&file).unwrap();
        pass(&state, root, &[uid], 2000.0, 900);
        assert_eq!(fs::read(file).unwrap(), original);
    }

    #[test]
    fn legacy_episode_preserves_pending_and_acknowledged_alerts_and_migrates_suppression() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut daemon = DaemonState::new();
        daemon.daemon_sessions_path = Some(root.join("daemon-sessions.json"));
        let state = Arc::new(Mutex::new(daemon));
        let uid = "ts-legacy".to_string();
        crate::notifications::publish(root, &uid, "one", "chat", "[one] wake", "[one]").unwrap();
        let queue = crate::notifications::directory(root, &uid);
        let file = fs::read_dir(queue).unwrap().flatten().map(|e| e.path()).find(|p| p.extension().is_some_and(|e| e == "json")).unwrap();
        let mut event: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        event["created_at"] = json!(1.0);
        event["status"] = json!("submitted"); event["updated_at"] = json!(1000.0);
        crate::messaging::atomic_replace(&file, &event).unwrap();
        let id = crate::owner_attention::raise_system(&state.lock().unwrap(), &uid, "legacy stall").unwrap().unwrap();
        save(root, &[(uid.clone(), Episode { since: 1000, reason: "unobserved_submissions".into(), alert_id: Some(id.clone()), alert_key: None })].into_iter().collect()).unwrap();
        pass(&state, root, std::slice::from_ref(&uid), 2000.0, 900);
        let alerts = crate::owner_attention::snapshot(&state.lock().unwrap()).unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[&uid].id, id, "pending legacy alert is retained");
        crate::owner_attention::clear_system(&state.lock().unwrap(), &uid, &id).unwrap();
        let other = crate::owner_attention::raise_system(&state.lock().unwrap(), &uid, "new decision").unwrap().unwrap();
        pass(&state, root, std::slice::from_ref(&uid), 2100.0, 900);
        let alerts = crate::owner_attention::snapshot(&state.lock().unwrap()).unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[&uid].id, other, "acknowledged legacy episode does not re-alert or clear a replacement");
        // A legacy episode suppressed by an unrelated alert had no ID.
        save(root, &[(uid.clone(), Episode { since: 1000, reason: "unobserved_submissions".into(), alert_id: None, alert_key: None })].into_iter().collect()).unwrap();
        pass(&state, root, std::slice::from_ref(&uid), 2200.0, 900);
        let alerts = crate::owner_attention::snapshot(&state.lock().unwrap()).unwrap();
        assert_eq!(alerts[&uid].id, other);
        assert!(alerts.contains_key(&format!("notification-stall:{uid}")));
    }

    #[test]
    fn locked_recipient_does_not_block_later_alarm() {
        use std::os::fd::AsRawFd;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut daemon = DaemonState::new();
        daemon.daemon_sessions_path = Some(root.join("daemon-sessions.json"));
        let state = Arc::new(Mutex::new(daemon));
        for uid in ["first", "second"] {
            crate::notifications::publish(root, uid, "one", "chat", "[one] wake", "[one]").unwrap();
            let queue = crate::notifications::directory(root, uid);
            let file = fs::read_dir(queue).unwrap().flatten().map(|e| e.path()).find(|p| p.extension().is_some_and(|e| e == "json")).unwrap();
            let mut event: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        event["created_at"] = json!(1.0);
            event["status"] = json!("submitted"); event["updated_at"] = json!(1000.0);
            crate::messaging::atomic_replace(&file, &event).unwrap();
        }
        pass(&state, root, &["first".into()], 2000.0, 900);
        let first_id = crate::owner_attention::snapshot(&state.lock().unwrap()).unwrap()["notification-stall:first"].id.clone();
        let first = crate::notifications::directory(root, "first").join("queue.lock");
        let lock = fs::OpenOptions::new().read(true).write(true).open(first).unwrap();
        assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
        pass(&state, root, &["first".into(), "second".into()], 2000.0, 900);
        let alerts = crate::owner_attention::snapshot(&state.lock().unwrap()).unwrap();
        assert_eq!(alerts["notification-stall:first"].id, first_id, "busy queue cannot clear its episode");
        assert!(alerts.contains_key("notification-stall:second"));
    }

    #[test]
    fn missing_transport_does_not_resolve_held_pending_notices() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut daemon = DaemonState::new();
        daemon.daemon_sessions_path = Some(root.join("daemon-sessions.json"));
        let state = Arc::new(Mutex::new(daemon));
        let uid = "ts-held".to_string();
        crate::notifications::publish(root, &uid, "one", "chat", "[one] wake", "[one]").unwrap();
        let queue = crate::notifications::directory(root, &uid);
        let file = fs::read_dir(&queue).unwrap().flatten().map(|e| e.path()).find(|p| p.extension().is_some_and(|e| e == "json")).unwrap();
        let mut event: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        event["created_at"] = json!(1.0);
        event["created_at"] = json!(1000.0);
        crate::messaging::atomic_replace(&file, &event).unwrap();
        let transport = queue.join("transport.json");
        crate::messaging::atomic_replace(&transport, &json!({"status":"degraded", "reason":"external_background"})).unwrap();
        pass(&state, root, std::slice::from_ref(&uid), 2000.0, 900);
        assert!(load(root).contains_key(&uid));
        fs::remove_file(transport).unwrap();
        pass(&state, root, std::slice::from_ref(&uid), 2100.0, 900);
        assert!(load(root).contains_key(&uid));
        event["status"] = json!("observed"); event["updated_at"] = json!(2200.0);
        crate::messaging::atomic_replace(&file, &event).unwrap();
        pass(&state, root, std::slice::from_ref(&uid), 2300.0, 900);
        assert!(load(root).is_empty());
    }

    #[test]
    fn persisted_legacy_board_observation_does_not_clear_alarm_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let uid = "ts-persisted".to_string();
        crate::notifications::publish(root, &uid, "later", "chat", "[later] wake", "[later]").unwrap();
        crate::notifications::publish(root, &uid, "board-push:1", "board", "[cm-board sfd] old", "[cm-board sfd]").unwrap();
        let queue = crate::notifications::directory(root, &uid);
        use sha2::Digest;
        let file = |id: &str| queue.join(format!("{:x}.json", sha2::Sha256::digest(id.as_bytes())));
        let mut later: Value = serde_json::from_slice(&fs::read(file("later")).unwrap()).unwrap();
        later["created_at"] = json!(1.0);
        later["status"] = json!("submitted"); later["updated_at"] = json!(1000.0);
        crate::messaging::atomic_replace(&file("later"), &later).unwrap();
        let mut board: Value = serde_json::from_slice(&fs::read(file("board-push:1")).unwrap()).unwrap();
        board["created_at"] = json!(1.0);
        board["status"] = json!("observed"); board["updated_at"] = json!(1900.0);
        crate::messaging::atomic_replace(&file("board-push:1"), &board).unwrap();
        let original = fs::read(file("board-push:1")).unwrap();
        let mkstate = || {
            let mut daemon = DaemonState::new();
            daemon.daemon_sessions_path = Some(root.join("daemon-sessions.json"));
            Arc::new(Mutex::new(daemon))
        };
        pass(&mkstate(), root, std::slice::from_ref(&uid), 2000.0, 900);
        assert!(load(root).contains_key(&uid));
        pass(&mkstate(), root, std::slice::from_ref(&uid), 2050.0, 900);
        assert!(load(root).contains_key(&uid));
        assert_eq!(fs::read(file("board-push:1")).unwrap(), original);
        let marker = "[cm-board sfd push:2]";
        crate::notifications::publish(root, &uid, "board-push:2", "board", marker, marker).unwrap();
        let mut fresh: Value = serde_json::from_slice(&fs::read(file("board-push:2")).unwrap()).unwrap();
        fresh["created_at"] = json!(1.0);
        fresh["status"] = json!("observed"); fresh["updated_at"] = json!(2100.0);
        crate::messaging::atomic_replace(&file("board-push:2"), &fresh).unwrap();
        pass(&mkstate(), root, std::slice::from_ref(&uid), 2200.0, 900);
        assert!(load(root).is_empty());
    }

    #[test]
    fn untrusted_chronology_neither_clears_episode_nor_retires_receipts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let uid = "ts-clock".to_string();
        let mut daemon = DaemonState::new();
        daemon.daemon_sessions_path = Some(root.join("daemon-sessions.json"));
        let state = Arc::new(Mutex::new(daemon));
        crate::notifications::publish(root, &uid, "one", "chat", "[one] wake", "[one]").unwrap();
        let queue = crate::notifications::directory(root, &uid);
        let file = fs::read_dir(&queue).unwrap().flatten().map(|e| e.path()).find(|p| p.extension().is_some_and(|e| e == "json")).unwrap();
        let mut event: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        event["created_at"] = json!(1000.0);
        event["updated_at"] = json!(1000.0);
        event["status"] = json!("submitted");
        crate::messaging::atomic_replace(&file, &event).unwrap();
        pass(&state, root, std::slice::from_ref(&uid), 2000.0, 900);
        let episode = load(root)[&uid].clone();
        let key = format!("notification-stall:{uid}");
        let alert = crate::owner_attention::snapshot(&state.lock().unwrap()).unwrap()[&key].id.clone();
        // A separate legacy receipt would be retired by a successful census.
        crate::notifications::publish(root, &uid, "old", "chat", "[old] wake", "[old]").unwrap();
        use sha2::Digest;
        let old_file = queue.join(format!("{:x}.json", sha2::Sha256::digest(b"old")));
        let mut old: Value = serde_json::from_slice(&fs::read(&old_file).unwrap()).unwrap();
        old["created_at"] = json!(1000.0); old["updated_at"] = json!(1000.0);
        old["status"] = json!("uncertain"); old["marker"] = json!("absent");
        crate::messaging::atomic_replace(&old_file, &old).unwrap();
        let original = fs::read(&old_file).unwrap();
        for (created, updated) in [(json!(null), json!(2100.0)), (json!(2100.0), json!(2000.0)), (json!(1000.0), json!(3000.0))] {
            event["created_at"] = created; event["updated_at"] = updated; event["status"] = json!("observed");
            crate::messaging::atomic_replace(&file, &event).unwrap();
            pass(&state, root, std::slice::from_ref(&uid), 2500.0, 900);
            assert_eq!(load(root)[&uid], episode);
            assert_eq!(crate::owner_attention::snapshot(&state.lock().unwrap()).unwrap()[&key].id, alert);
            assert_eq!(fs::read(&old_file).unwrap(), original);
        }
        // Once wall time catches up, the forward observation is credible.
        fs::remove_file(old_file).unwrap();
        pass(&state, root, std::slice::from_ref(&uid), 3000.0, 900);
        assert!(load(root).is_empty());
    }
}
