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
    /// Reported by the engine itself (presence/hooks/relay), not the
    /// daemon's PTY/transcript fallback.
    pub engine_source: bool,
    /// `stalled_since` is set: working with no visible progress.
    pub stalled: bool,
    /// The current idle/waiting state was entered through a live transition
    /// (so notify-on-idle already had its chance), not seeded by a snapshot.
    pub notified_quiet: bool,
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
            engine_source: matches!(v["source"].as_str(), Some("presence" | "hooks" | "relay")),
            stalled: !v["stalled_since"].is_null(),
            notified_quiet: false,
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

    /// Whether the PTY heuristic's notify-on-idle should stay quiet because
    /// the engine's own state covers it: engine-reported, not stalled or
    /// unknown, and — when quiet — reached by a live transition that already
    /// notified. A seeded idle, an unknown/stalled engine, or the PTY
    /// fallback leaves the PTY notification in place.
    pub(crate) fn engine_covers_idle_notify(&self, host: &HostId, uid: &str) -> bool {
        covers_idle_notify(&self.agent_states, host, uid)
    }
}

/// See [`App::engine_covers_idle_notify`]; a free function so the PTY tick can
/// call it while it holds `workspaces` mutably.
pub(super) fn covers_idle_notify(
    states: &HashMap<(HostId, String), AgentStateView>,
    host: &HostId,
    uid: &str,
) -> bool {
    {
        states.get(&(host.clone(), uid.to_owned())).is_some_and(|v| {
            v.engine_source
                && !v.stalled
                && match v.activity {
                    Activity::Working | Activity::Background => true,
                    Activity::Idle | Activity::Waiting => v.notified_quiet,
                    Activity::Errored | Activity::Starting | Activity::Exited | Activity::Unknown => false,
                }
        })
    }
}

impl App {
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
        if ts.session_type == "bash" && !ts.session.exited {
            return Some(super::symbols::engine("bash"));
        }
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
        let mut next = AgentStateView::parse(value);
        let old = self.agent_states.get(&key);
        let quiet = |a: Activity| matches!(a, Activity::Idle | Activity::Waiting);
        // A live diff into idle/waiting notifies, including the first state
        // seen for a session (snapshots seed silently).
        let entered_quiet = quiet(next.activity) && old.is_none_or(|o| !quiet(o.activity));
        // Moving between idle and waiting keeps the earlier notification.
        next.notified_quiet = entered_quiet
            || (quiet(next.activity) && old.is_some_and(|o| quiet(o.activity) && o.notified_quiet));
        // A working engine that just stalled while the terminal sits quiet
        // gets the notification its stuck state would otherwise suppress.
        let newly_stalled = next.stalled && old.is_some_and(|o| !o.stalled);
        if entered_quiet || newly_stalled {
            if let Some(ts) = self
                .workspaces
                .iter()
                .flat_map(|w| &w.sessions)
                .find(|s| s.uid == uid && &s.host_id == host)
            {
                // First engine state seen while the terminal is already
                // idle: the PTY path has notified for this spell.
                let pty_already = entered_quiet && old.is_none() && ts.status == SessionStatus::Idle;
                if ts.notify_on_idle && !pty_already && (entered_quiet || ts.status == SessionStatus::Idle) {
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

/// The live activity badge: what a session is doing right now, from its
/// `agent_state` alone (no extra polling). The compact form (`mark`, then
/// `short`, at most [`BADGE_MAX`] cells) sits at the right of sidebar rows
/// and org chart cards; `full` is the A-i peek's and the info pane's line,
/// and the only place job labels, the waiting tool and error kinds appear.
/// The marks and styles live in `symbols::doing`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Badge {
    pub kind: symbols::Doing,
    /// The text after the mark: a time, a count, or nothing.
    pub short: String,
    /// A space between mark and text (`⚙ 3m`; the others sit tight: `⚠12m`).
    pub gap: bool,
    pub full: String,
}

/// The widest a compact badge gets, in cells (`⚙ 3m+2`).
pub(crate) const BADGE_MAX: usize = 6;

/// Whether an `error_kind` is a rate or usage limit: Claude's `rate_limit`
/// / `rate_limited`, Codex's `usageLimitExceeded`, quota and HTTP 429
/// spellings.
pub(crate) fn is_rate_limit(kind: &str) -> bool {
    let k: String = kind.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_ascii_lowercase();
    ["ratelimit", "usagelimit", "quota", "toomanyrequests"].iter().any(|n| k.contains(n)) || k == "429"
}

/// `usageLimitExceeded` / `server_error` → `usage limit exceeded` / `server error`.
fn humanize(s: &str) -> String {
    let mut out = String::new();
    let mut prev_lower = false;
    for c in s.chars() {
        if c == '_' || c == '-' {
            out.push(' ');
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower {
            out.push(' ');
        }
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
        out.extend(c.to_lowercase());
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// What a waiting session waits for, in a word or two.
fn waiting_word(waiting_for: &str) -> String {
    match waiting_for {
        "permission_prompt" | "permission" | "Approval" | "approval" => "permission".into(),
        "elicitation_dialog" | "Elicitation" | "elicitation" => "question".into(),
        "UserInput" | "user_input" => "input".into(),
        "ToolCall" | "tool_call" => "tool call".into(),
        "AuthRefresh" | "auth_refresh" => "sign-in".into(),
        other => humanize(other),
    }
}

/// `45s`, `12m`, `3h`, `2d`.
pub(crate) fn short_age(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

/// The badge for an engine state at `now` (Unix seconds), or None when
/// there is nothing to say: idle, starting, unknown and exited sessions
/// keep just their state glyph.
pub(crate) fn badge(v: &AgentStateView, now: f64) -> Option<Badge> {
    use symbols::Doing;
    let detail = &v.raw["detail"];
    let labels: Vec<String> = v.raw["background"]["jobs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|j| j["label"].as_str())
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect();
    let jobs = v.jobs;
    let age = now - v.since;
    let (kind, short, full) = match v.activity {
        Activity::Errored => {
            let kind = v.error_kind.as_deref().unwrap_or("");
            let resumes = detail["resumes_at"].as_f64().filter(|r| *r > now);
            if is_rate_limit(kind) {
                let (short, full) = match resumes {
                    Some(r) => (short_age(r - now), format!("rate limited ({kind}), resumes in {}", short_age(r - now))),
                    None => (String::new(), format!("rate limited ({kind}) for {}", short_age(age))),
                };
                (Doing::RateLimited, short, full)
            } else if kind.is_empty() {
                (Doing::Errored, String::new(), format!("errored {} ago", short_age(age)))
            } else {
                (Doing::Errored, String::new(), format!("error: {} ({kind}), {} ago", humanize(kind), short_age(age)))
            }
        }
        Activity::Waiting => {
            let reason = v.waiting_for.as_deref().map(waiting_word);
            let tool = detail["open_tool"].as_str().filter(|t| !t.is_empty());
            let full = match (reason, tool) {
                (Some(r), Some(t)) => format!("waiting on {r}: {t}"),
                (Some(r), None) => format!("waiting on {r}"),
                (None, Some(t)) => format!("waiting on {t}"),
                (None, None) => "waiting on a human".into(),
            };
            (Doing::Waiting, String::new(), format!("{full} for {}", short_age(age)))
        }
        Activity::Working if detail["retrying"].as_bool() == Some(true) => {
            (Doing::Retrying, String::new(), format!("retrying a failed request, {}", short_age(age)))
        }
        Activity::Working if v.stalled => {
            let stalled_for = v.raw["stalled_since"].as_f64().map_or(age, |t| now - t);
            (Doing::Stalled, short_age(stalled_for), format!("working, no progress for {}", short_age(stalled_for)))
        }
        Activity::Working => {
            let mut full = format!("working {}", short_age(age));
            if jobs > 0 {
                full.push_str(&format!(", {jobs} background"));
                if !labels.is_empty() {
                    full.push_str(&format!(": {}", labels.join(", ")));
                }
            }
            // `⚙ 3m+2`; a longer one loses its space, then its job count.
            let time = short_age(age);
            let with_jobs = if jobs > 0 { format!("{time}+{jobs}") } else { time.clone() };
            let (short, gap) = [(with_jobs.clone(), true), (with_jobs, false), (time, false)]
                .into_iter()
                .find(|(t, gap)| 1 + usize::from(*gap) + t.chars().count() <= BADGE_MAX)
                .unwrap_or_else(|| (String::new(), false));
            return Some(Badge { kind: Doing::Working, short, gap, full });
        }
        Activity::Background => {
            let short = if jobs > 0 { jobs.to_string() } else { String::new() };
            let full = if labels.is_empty() {
                match jobs {
                    0 => "background work".into(),
                    1 => "1 background job".into(),
                    n => format!("{n} background jobs"),
                }
            } else {
                format!("background: {}", labels.join(", "))
            };
            (Doing::Background, short, full)
        }
        Activity::Idle | Activity::Starting | Activity::Exited | Activity::Unknown => return None,
    };
    // Counts and countdowns are short; anything past the cap is dropped
    // rather than cut, the mark staying.
    let short = if 1 + short.chars().count() <= BADGE_MAX { short } else { String::new() };
    Some(Badge { kind, short, gap: false, full })
}

/// The compact badge as spans: the mark, then its text.
pub(crate) fn badge_spans(b: &Badge) -> Vec<Span<'static>> {
    let (mark, mark_style) = symbols::doing(b.kind);
    let mut out = vec![Span::styled(mark, mark_style)];
    if !b.short.is_empty() {
        let text = if b.gap { format!(" {}", b.short) } else { b.short.clone() };
        out.push(Span::styled(text, symbols::doing_text(b.kind)));
    }
    out
}

/// Cells the compact badge takes (never more than [`BADGE_MAX`]).
pub(crate) fn badge_width(b: &Badge) -> usize {
    use unicode_width::UnicodeWidthStr;
    1 + usize::from(b.gap && !b.short.is_empty()) + b.short.width()
}

/// Whether a row with `room` cells shows a `label_w`-cell label and the
/// badge with a blank cell between them. The name has priority: a row too
/// narrow for both drops the badge rather than cut the name.
pub(crate) fn badge_fits(room: usize, label_w: usize, b: &Badge) -> bool {
    label_w + 1 + badge_width(b) <= room
}

impl App {
    /// A session's live activity badge (ignores the F9 toggle; the info
    /// pane always shows it). None for exited sessions and rows without
    /// engine state.
    pub(crate) fn session_badge(&self, ts: &TerminalSession) -> Option<Badge> {
        if ts.session.exited {
            return None;
        }
        badge(self.agent_view(ts)?, now_unix())
    }

    /// The badge for sidebar rows and org chart cards: as
    /// [`App::session_badge`], when the F9 toggle is on.
    pub(crate) fn row_badge(&self, ts: &TerminalSession) -> Option<Badge> {
        self.global_settings.activity_badges().then(|| self.session_badge(ts)).flatten()
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
    fn pty_notify_is_suppressed_only_while_a_fresh_engine_state_covers_it() {
        let host = HostId::new("sessions");
        let mut states = HashMap::new();
        let mut put = |state: Value, notified: bool| {
            let mut v = AgentStateView::parse(&state);
            v.notified_quiet = notified;
            states.insert((host.clone(), "u".to_string()), v);
            covers_idle_notify(&states, &host, "u")
        };
        assert!(put(json!({"state":"working","source":"presence"}), false), "engine working: it will notify on its idle");
        assert!(!put(json!({"state":"working","source":"presence","stalled_since":5.0}), false), "stalled: PTY may notify");
        assert!(!put(json!({"state":"unknown","source":"presence"}), false));
        assert!(!put(json!({"state":"working","source":"pty"}), false), "daemon fallback is not engine state");
        assert!(!put(json!({"state":"idle","source":"hooks"}), false), "seeded idle never notified");
        assert!(put(json!({"state":"idle","source":"hooks"}), true));
        assert!(!covers_idle_notify(&HashMap::new(), &host, "u"), "no state: PTY decides");
    }

    fn b(state: Value, now: f64) -> Option<Badge> {
        badge(&AgentStateView::parse(&state), now)
    }

    /// The compact badge as drawn: mark and text.
    fn text(x: &Badge) -> String {
        badge_spans(x).iter().map(|s| s.content.to_string()).collect()
    }

    fn short(state: Value) -> (symbols::Doing, String) {
        let x = b(state, 1000.0).expect("a badge");
        (x.kind, text(&x))
    }

    /// Owner 2026-10-23: compact badges, no labels; the full line keeps them.
    #[test]
    fn badge_labels_follow_the_engine_state() {
        use symbols::Doing::*;
        // Working: time in the turn, plus a background job count.
        assert_eq!(short(json!({"state":"working","since":820.0})), (Working, "\u{2699} 3m".into()));
        let w = b(json!({"state":"working","since":820.0,"background":{"jobs":[
            {"id":"1","label":"cargo test"},{"id":"2","label":"npm run build"}]}}), 1000.0).unwrap();
        assert_eq!((text(&w).as_str(), w.full.as_str()), ("\u{2699} 3m+2", "working 3m, 2 background: cargo test, npm run build"));
        // Too long for six cells: the space goes first, then the count.
        let jobs = |n: usize| json!((0..n).map(|i| json!({"id": i.to_string()})).collect::<Vec<_>>());
        assert_eq!(short(json!({"state":"working","since":400.0,"background":{"jobs":jobs(2)}})), (Working, "\u{2699}10m+2".into()));
        assert_eq!(short(json!({"state":"working","since":400.0,"background":{"jobs":jobs(12)}})), (Working, "\u{2699}10m".into()));
        assert_eq!(short(json!({"state":"working","since":0.0,"detail":{"retrying":true}})), (Retrying, "\u{21bb}".into()));
        assert_eq!(short(json!({"state":"working","since":0.0,"stalled_since":640.0})), (Stalled, "\u{23f1}6m".into()));
        // Background: the job count, never the job's label.
        let one = b(json!({"state":"working-background","background":{"jobs":[{"id":"1","label":"cargo   test\n-p tui"}]}}), 1000.0).unwrap();
        assert_eq!((text(&one).as_str(), one.full.as_str()), ("\u{29d7}1", "background: cargo test -p tui"));
        assert_eq!(short(json!({"state":"working-background","background":{"jobs":[{"id":"1","label":"a"},{"id":"2","label":"b"}]}})),
            (Background, "\u{29d7}2".into()));
        assert_eq!(short(json!({"state":"working-background","background":{"complete":false,"jobs":[]}})), (Background, "\u{29d7}".into()));
        // Waiting: the mark alone; the tool and reason stay in the full line.
        let p = b(json!({"state":"waiting-on-human","since":940.0,"detail":{"waiting_for":"permission_prompt","open_tool":"Bash"}}), 1000.0).unwrap();
        assert_eq!((p.kind, text(&p).as_str(), p.full.as_str()), (Waiting, "\u{261b}", "waiting on permission: Bash for 1m"));
        for detail in [json!({"waiting_for":"elicitation_dialog"}), json!({"waiting_for":"UserInput"}), json!({})] {
            assert_eq!(short(json!({"state":"waiting-on-human","detail": detail})), (Waiting, "\u{261b}".into()));
        }
        assert!(b(json!({"state":"waiting-on-human","detail":{"waiting_for":"elicitation_dialog"}}), 1000.0).unwrap().full.contains("question"));
        // Other errors: the mark alone, the kind in the full line.
        let e = b(json!({"state":"errored","since":880.0,"detail":{"error_kind":"server_error"}}), 1000.0).unwrap();
        assert_eq!((e.kind, text(&e).as_str(), e.full.as_str()), (Errored, "\u{2715}", "error: server error (server_error), 2m ago"));
        assert_eq!(short(json!({"state":"errored"})), (Errored, "\u{2715}".into()));
        // Nothing to add for quiet or unknown states.
        for state in ["idle", "starting", "unknown", "exited", "something-new"] {
            assert_eq!(b(json!({"state": state}), 1000.0), None, "{state}");
        }
    }

    /// Owner 2026-10-22: a rate limit reads distinctly, so sessions that
    /// need waking are easy to spot.
    #[test]
    fn rate_limits_are_recognized_and_drawn_distinctly() {
        use symbols::Doing::*;
        for kind in ["rate_limit", "rate_limited", "usageLimitExceeded", "RateLimitError", "quota_exceeded", "429", "too_many_requests"] {
            assert!(is_rate_limit(kind), "{kind}");
            assert_eq!(short(json!({"state":"errored","detail":{"error_kind": kind}})).0, RateLimited, "{kind}");
        }
        for kind in ["server_error", "authentication_failed", "api_error", "billing_error", "overloaded", ""] {
            assert!(!is_rate_limit(kind), "{kind}");
        }
        // With a known resume time it counts down; without, the mark alone.
        let r = b(json!({"state":"errored","since":400.0,"detail":{"error_kind":"usageLimitExceeded","resumes_at":1720.0}}), 1000.0).unwrap();
        assert_eq!((text(&r).as_str(), r.full.as_str()), ("\u{26a0}12m", "rate limited (usageLimitExceeded), resumes in 12m"));
        let r = b(json!({"state":"errored","since":400.0,"detail":{"error_kind":"rate_limit","resumes_at":10.0}}), 1000.0).unwrap();
        assert_eq!((text(&r).as_str(), r.full.as_str()), ("\u{26a0}", "rate limited (rate_limit) for 10m"));
        // Its mark is reversed and bold; no other badge's is.
        let (mark, style) = symbols::doing(RateLimited);
        assert_eq!(mark, "\u{26a0}");
        assert!(style.add_modifier.contains(Modifier::REVERSED | Modifier::BOLD));
        for other in [Working, Background, Waiting, Errored, Retrying, Stalled] {
            assert!(!symbols::doing(other).1.add_modifier.contains(Modifier::REVERSED), "{other:?}");
            assert_ne!(symbols::doing(other).0, mark);
        }
    }

    /// No badge is wider than BADGE_MAX, and one only shows beside the
    /// whole name.
    #[test]
    fn badges_stay_compact_and_give_way_to_the_name() {
        let ages = [0.0, 59.0, 600.0, 3599.0, 7200.0, 86_400.0 * 400.0];
        for since in ages {
            for n in [0usize, 1, 2, 9, 10, 99, 1000] {
                let jobs: Vec<Value> = (0..n).map(|i| json!({"id": i.to_string(), "label": "a very long job label indeed"})).collect();
                for state in [
                    json!({"state":"working","since": 1e8 - since,"background":{"jobs":jobs}}),
                    json!({"state":"working-background","since":0.0,"background":{"jobs":jobs}}),
                    json!({"state":"working","since":0.0,"stalled_since": 1e8 - since}),
                    json!({"state":"errored","detail":{"error_kind":"rate_limit","resumes_at": 1e8 + since + 1.0}}),
                ] {
                    let x = b(state.clone(), 1e8).unwrap();
                    let drawn = badge_spans(&x).iter().map(|s| s.width()).sum::<usize>();
                    assert_eq!(drawn, badge_width(&x), "{state}");
                    assert!(drawn <= BADGE_MAX, "{state}: {:?}", text(&x));
                }
            }
        }
        let x = b(json!({"state":"working","since":820.0}), 1000.0).unwrap(); // ⚙ 3m, 4 cells
        assert!(badge_fits(10, 5, &x));
        assert!(!badge_fits(9, 5, &x), "one cell short: the badge goes, the name stays whole");
    }

    fn session(app: &mut App, wi: usize, uid: &str, label: &str) {
        let pty = crate::session::Session::new("/bin/true", &[], 80, 24, None, HashMap::new(), None).unwrap();
        let mut ts = super::super::model::make_simple_session_with_uid(uid.to_string(), label, "codex", pty, None);
        ts.session.exited = false;
        app.workspaces[wi].sessions.push(ts);
    }

    fn rows(buffer: &ratatui::buffer::Buffer) -> Vec<String> {
        let area = buffer.area;
        (0..area.height).map(|y| (0..area.width).map(|x| buffer[(x, y)].symbol().to_string()).collect()).collect()
    }

    #[test]
    fn shell_sidebar_row_has_a_terminal_marker_even_when_activity_is_hidden() {
        use super::super::sections::layout_tests::{bare_ws, sidebar_buffer, test_app};
        let mut app = test_app();
        app.sidebar_view = SidebarView::Task;
        app.workspaces = vec![bare_ws("w")];
        session(&mut app, 0, "ts-1-0", "terminal");
        let ts = &mut app.workspaces[0].sessions[0];
        ts.session_type = "bash".into();
        ts.hidden = true;
        let text = rows(&sidebar_buffer(&mut app, 12));
        let row = text.iter().find(|r| r.contains("terminal")).unwrap();
        assert!(row.contains('$'), "{row}");
        assert!(!row.contains('●'), "shell is not an idle worker: {row}");
    }

    /// The sidebar row: the compact badge right-aligned, dropped before the
    /// name would be cut, and F9's toggle (previewed live) turning it off.
    #[test]
    fn sidebar_rows_show_the_badge_and_f9_turns_it_off() {
        use super::super::sections::layout_tests::{bare_ws, sidebar_buffer, test_app};
        let mut app = test_app();
        app.sidebar_view = SidebarView::Task;
        app.workspaces = vec![bare_ws("w")];
        session(&mut app, 0, "ts-1-0", "worker");
        session(&mut app, 0, "ts-2-0", "a-rather-long-session-label-here");
        session(&mut app, 0, "ts-3-0", "quiet");
        session(&mut app, 0, "ts-4-0", "builder");
        let host = HostId::local();
        let now = now_unix();
        app.apply_agent_state(&host, "ts-1-0", &json!({"state":"errored","since":now - 60.0,
            "detail":{"error_kind":"rate_limited","resumes_at":now + 750.0}}));
        app.apply_agent_state(&host, "ts-2-0", &json!({"state":"working-background","since":now,
            "background":{"jobs":[{"id":"1","label":"cargo test --workspace --all-features"}]}}));
        app.apply_agent_state(&host, "ts-3-0", &json!({"state":"idle","since":now}));
        app.apply_agent_state(&host, "ts-4-0", &json!({"state":"working-background","since":now,
            "background":{"jobs":[{"id":"1","label":"cargo test --workspace"},{"id":"2","label":"npm run build"}]}}));
        let text = rows(&sidebar_buffer(&mut app, 12));
        let row = |needle: &str| text.iter().find(|r| r.contains(needle)).cloned().unwrap_or_else(|| panic!("{needle}\n{}", text.join("\n")));
        // Right-aligned against the border, with a short countdown.
        let rl = row("worker");
        assert!(rl.trim_end_matches('\u{2502}').trim_end().ends_with("\u{26a0}12m"), "{rl}");
        // A job count, never the job's label.
        let bl = row("builder");
        assert!(bl.trim_end_matches('\u{2502}').trim_end().ends_with("\u{29d7}2"), "{bl}");
        assert!(!text.join("\n").contains("cargo"), "{}", text.join("\n"));
        // The long name doesn't leave room for both: the badge goes and
        // the name is not cut for it.
        let long = row("a-rather");
        assert!(!long.contains('\u{29d7}'), "{long}");
        // Idle: no badge.
        assert!(!row("quiet").contains(['\u{2699}', '\u{29d7}', '\u{26a0}']));
        // F9 → ↓ → Space previews the toggle off; Esc restores it.
        let key = |c| CrosstermEvent::Key(crossterm::event::KeyEvent::new(c, crossterm::event::KeyModifiers::NONE));
        assert!(app.global_settings.activity_badges(), "on by default");
        for c in [KeyCode::F(9), KeyCode::Down, KeyCode::Char(' ')] {
            app.global_settings.handle_event(&key(c));
        }
        assert!(!app.global_settings.activity_badges());
        let off = rows(&sidebar_buffer(&mut app, 12)).join("\n");
        assert!(!off.contains('\u{26a0}') && !off.contains('\u{29d7}'), "{off}");
        app.global_settings.handle_event(&key(KeyCode::Esc));
        assert!(app.global_settings.activity_badges());
    }

    /// The A-i peek keeps the full line: job labels, the waiting tool.
    #[test]
    fn the_peek_spells_the_badge_out() {
        use super::super::sections::layout_tests::{bare_ws, test_app};
        let mut app = test_app();
        app.workspaces = vec![bare_ws("w")];
        session(&mut app, 0, "ts-1-0", "worker");
        session(&mut app, 0, "ts-2-0", "asker");
        let host = HostId::local();
        let now = now_unix();
        app.apply_agent_state(&host, "ts-1-0", &json!({"state":"working-background","since":now,
            "background":{"jobs":[{"id":"1","label":"cargo test --workspace"}]}}));
        app.apply_agent_state(&host, "ts-2-0", &json!({"state":"waiting-on-human","since":now,
            "detail":{"waiting_for":"permission_prompt","open_tool":"Bash"}}));
        let doing = |app: &App| app.build_peek_lines().into_iter().find_map(|l| match l {
            PeekLine::Field { label, value } if label == "Doing" => Some(value),
            _ => None,
        });
        app.cursor = Cursor::Session(0, 0);
        assert_eq!(doing(&app).as_deref(), Some("\u{29d7} background: cargo test --workspace"));
        app.cursor = Cursor::Session(0, 1);
        assert_eq!(doing(&app).as_deref(), Some("\u{261b} waiting on permission: Bash for 0s"));
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
