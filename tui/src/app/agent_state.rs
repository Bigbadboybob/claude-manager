//! Engine-reported session state (`agent_state`, see doc/SESSION_STATE.md).
//!
//! The daemon publishes one `agent_state` per session on `manifest.watch`
//! (`agent_states` in the snapshot, `Updated {entry: {agent_state}}` diffs).
//! The TUI keeps it beside, not inside, `TerminalSession`, keyed by
//! (host, uid), and prefers it over the PTY heuristic wherever it is present.
//! `SessionStatus` stays the authority for prompt delivery and border
//! animation; rows from an older daemon simply have no entry.
use super::*;
use crate::hosts::HostId;
use serde_json::Value;

/// What the engine says the session is doing. Unrecognized states read as
/// `Unknown` (never idle), per the contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Activity {
    Working,
    Background,
    Waiting,
    Errored,
    Idle,
    Starting,
    Exited,
    Unknown,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AgentStateView {
    pub activity: Activity,
    /// Unix seconds the current state began (idle_since while idle).
    pub since: f64,
    pub error_kind: Option<String>,
    pub waiting_for: Option<String>,
    pub jobs: usize,
    /// The daemon's object as received, passed through to TUI-served reads.
    pub raw: Value,
}

impl AgentStateView {
    pub(crate) fn parse(v: &Value) -> Self {
        let activity = match v["state"].as_str() {
            Some("working") => Activity::Working,
            Some("working-background") => Activity::Background,
            Some("waiting-on-human") => Activity::Waiting,
            Some("errored") => Activity::Errored,
            Some("idle") => Activity::Idle,
            Some("starting") => Activity::Starting,
            Some("exited") => Activity::Exited,
            _ => Activity::Unknown,
        };
        Self {
            activity,
            since: v["since"].as_f64().unwrap_or(0.0),
            error_kind: v["detail"]["error_kind"].as_str().map(str::to_owned),
            waiting_for: v["detail"]["waiting_for"].as_str().map(str::to_owned),
            jobs: v["background"]["jobs"].as_array().map_or(0, Vec::len),
            raw: v.clone(),
        }
    }

    /// Age of the current state as of `now_unix`.
    fn age(&self, now_unix: f64) -> Duration {
        Duration::from_secs_f64((now_unix - self.since).max(0.0))
    }
}

fn now_unix() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// A-g priority tier for a row, lowest first; None = not a candidate.
/// alerts, then waiting-on-human, then errored, then idle.
pub(super) fn attention_tier(has_alert: bool, hidden: bool, activity: Option<Activity>, pty_idle: bool) -> Option<u8> {
    if has_alert {
        return Some(0);
    }
    if hidden {
        return None;
    }
    match activity {
        Some(Activity::Waiting) => Some(1),
        Some(Activity::Errored) => Some(2),
        Some(Activity::Idle) => Some(3),
        Some(_) => None,
        None => pty_idle.then_some(3),
    }
}

impl App {
    pub(crate) fn agent_view(&self, ts: &TerminalSession) -> Option<&AgentStateView> {
        self.agent_states.get(&(ts.host_id.clone(), ts.uid.clone()))
    }

    /// The daemon's `agent_state` object for a row, or null.
    pub(crate) fn agent_state_wire(&self, ts: &TerminalSession) -> Value {
        self.agent_view(ts).map_or(Value::Null, |v| v.raw.clone())
    }

    /// The engine's activity when the daemon reports one, else None.
    pub(crate) fn agent_activity(&self, ts: &TerminalSession) -> Option<Activity> {
        self.agent_view(ts).map(|v| v.activity)
    }

    /// True when the session is quiet at its prompt: the engine's idle when
    /// reported (waiting/errored are separate states), else the PTY heuristic.
    pub(crate) fn display_idle(&self, ts: &TerminalSession) -> bool {
        match self.agent_activity(ts) {
            Some(a) => a == Activity::Idle,
            None => ts.status == SessionStatus::Idle,
        }
    }

    /// Idle-age bucket from the engine's `since` when reported, else the
    /// TUI's own idle stamp.
    pub(crate) fn display_idle_bucket(&self, ts: &TerminalSession, now: Instant) -> Option<IdleAgeBucket> {
        if ts.session.exited || !self.display_idle(ts) {
            return None;
        }
        Some(match self.agent_view(ts) {
            Some(v) => idle_age_bucket(Some(v.age(now_unix()))),
            None => idle_age_bucket_at(ts.idle_since, now),
        })
    }

    /// Row glyph for engine states the PTY heuristic cannot express
    /// (working-background, waiting-on-human, errored, unknown). None leaves
    /// the caller's existing working/idle rendering in place.
    pub(crate) fn agent_indicator(&self, ts: &TerminalSession) -> Option<(&'static str, Style)> {
        if ts.session.exited {
            return None;
        }
        Some(match self.agent_activity(ts)? {
            Activity::Background => ("\u{25d0}", Style::default().fg(theme::OK).add_modifier(Modifier::DIM)),
            Activity::Waiting => ("?", Style::default().fg(theme::ATTN).add_modifier(Modifier::BOLD)),
            Activity::Errored => ("\u{2717}", Style::default().fg(theme::ERROR).add_modifier(Modifier::BOLD)),
            Activity::Unknown => ("\u{00b7}", Style::default().fg(theme::DIM)),
            Activity::Working | Activity::Idle | Activity::Starting | Activity::Exited => return None,
        })
    }

    /// Whether the row should render as running (spinner): the engine's
    /// working/starting when reported, else the PTY heuristic.
    pub(crate) fn display_running(&self, ts: &TerminalSession) -> bool {
        match self.agent_activity(ts) {
            Some(a) => matches!(a, Activity::Working | Activity::Starting),
            None => ts.status == SessionStatus::Running,
        }
    }

    pub(crate) fn apply_agent_state_snapshot(&mut self, host: &HostId, states: &std::collections::BTreeMap<String, Value>) {
        self.agent_states.retain(|(h, uid), _| h != host || states.contains_key(uid));
        for (uid, v) in states {
            self.agent_states.insert((host.clone(), uid.clone()), AgentStateView::parse(v));
        }
        self.needs_redraw = true;
    }

    /// Apply one `Updated {agent_state}` diff. A transition into idle or
    /// waiting-on-human fires notify-on-idle for rows that ask for it (the
    /// PTY-based notification is suppressed while an engine state exists).
    pub(crate) fn apply_agent_state(&mut self, host: &HostId, uid: &str, value: &Value) {
        let key = (host.clone(), uid.to_owned());
        if value.is_null() {
            self.agent_states.remove(&key);
            self.needs_redraw = true;
            return;
        }
        let next = AgentStateView::parse(value);
        let previous = self.agent_states.get(&key).map(|v| v.activity);
        let entered_quiet = matches!(next.activity, Activity::Idle | Activity::Waiting)
            && previous.is_some_and(|p| p != next.activity && !matches!(p, Activity::Idle | Activity::Waiting));
        if entered_quiet {
            if let Some(ts) = self
                .workspaces
                .iter()
                .flat_map(|w| &w.sessions)
                .find(|s| s.uid == uid && &s.host_id == host)
            {
                if ts.notify_on_idle {
                    notify_session_idle(&ts.label);
                }
            }
        }
        if self.agent_states.get(&key) != Some(&next) {
            self.agent_states.insert(key, next);
            self.needs_redraw = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_maps_every_contract_state_and_unknown_never_reads_idle() {
        for (wire, activity) in [
            ("working", Activity::Working),
            ("working-background", Activity::Background),
            ("waiting-on-human", Activity::Waiting),
            ("errored", Activity::Errored),
            ("idle", Activity::Idle),
            ("starting", Activity::Starting),
            ("exited", Activity::Exited),
            ("unknown", Activity::Unknown),
            ("some-future-state", Activity::Unknown),
        ] {
            assert_eq!(AgentStateView::parse(&json!({"state": wire})).activity, activity, "{wire}");
        }
        let v = AgentStateView::parse(&json!({"state":"errored","since":12.5,"detail":{"error_kind":"rate_limit"},
            "background":{"jobs":[{"id":"a"},{"id":"b"}]}}));
        assert_eq!((v.since, v.error_kind.as_deref(), v.jobs), (12.5, Some("rate_limit"), 2));
        // The exited tombstone's short shape parses.
        assert_eq!(AgentStateView::parse(&json!({"state":"exited"})).activity, Activity::Exited);
    }

    #[test]
    fn attention_tiers_order_alerts_waiting_errored_idle() {
        assert_eq!(attention_tier(true, true, Some(Activity::Working), false), Some(0), "alerts override hidden");
        assert_eq!(attention_tier(false, false, Some(Activity::Waiting), false), Some(1));
        assert_eq!(attention_tier(false, false, Some(Activity::Errored), false), Some(2));
        assert_eq!(attention_tier(false, false, Some(Activity::Idle), false), Some(3));
        assert_eq!(attention_tier(false, false, Some(Activity::Working), true), None, "engine state beats a quiet PTY");
        assert_eq!(attention_tier(false, false, Some(Activity::Unknown), true), None);
        assert_eq!(attention_tier(false, false, None, true), Some(3), "no engine state: PTY idle");
        assert_eq!(attention_tier(false, true, Some(Activity::Waiting), false), None, "hidden rows skip");
    }
}
