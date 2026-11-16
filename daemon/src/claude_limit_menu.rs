//! Clear Claude Code's usage-limit menu so a session does not park on it.
//!
//! After a turn fails on a subscription limit, Claude Code (seen in 2.1.318)
//! runs its own `/rate-limit-options` local command, once per reset time:
//!
//! ```text
//! You've hit your weekly limit · resets Nov 9, 8am (UTC)
//! What do you want to do?
//!   1. Stop and wait for limit to reset
//!   2. Wait here, then continue automatically at …
//!   3. Switch to usage credits
//! Enter to confirm · Esc to cancel
//! ```
//!
//! The 5-hour limit shows the same menu. A session nobody is watching sits
//! on it for days. Escape closes it the way "Stop and wait" does (Claude's
//! `display: "skip"`: nothing is written to the transcript), so the session
//! stays at its failed turn. This module sends that Escape and nothing else.
//! It never picks an option: option 2 arms Claude's own automatic continue,
//! and option 3 spends money.
//!
//! Whether the session resumes afterwards is not decided here.
//! `claude_limit_recovery` ([`crate::claude_account_recovery`]) stays the
//! only authority. It needs presence idle at the failed boundary, which a
//! session with the menu open never is; clearing the menu is what lets it
//! look. The Escape is written without stamping any input clock
//! ([`crate::session::InputHandle::write_limit_menu_escape`]), so both that
//! boundary and recovery's "no input since the failure" admission survive
//! it. `send_key`'s path stamps operator input and would block recovery.
//!
//! Safety, in the order the checks run:
//! - Claude's presence file says `waiting` with `waitingFor: "dialog open"`.
//!   Permission prompts report `permission prompt` and never qualify.
//! - The current screen (the replay ring rendered at the session's size;
//!   never scrollback or transcript text) ends with the menu: the limit
//!   header, the question, the "Stop and wait" option, and the footer in its
//!   last rows. The screen shown after choosing option 2 ("… Press esc to
//!   cancel the wait") does not match, so an Owner's choice is never undone.
//! - Grace: the menu has been seen for [`GRACE`] with the same option
//!   highlighted, and nobody has typed or delivered input for [`GRACE`].
//!   Arrow keys do not stamp operator input, so a moved highlight restarts
//!   the grace instead.
//! - The write re-checks under the session writer that nothing was printed
//!   since the match and that no input arrived.
//! - At most [`MAX_ATTEMPTS`] Escapes per turn boundary, [`RETRY`] apart,
//!   each checked after [`VERIFY`]. After that the session is left for a
//!   human.
//!
//! Each outcome is kept in the session's agent state until its next turn
//! (`detail.limit_menu`: `escape_sent`, `cleared` or `needs_human`, with
//! `detail.limit_menu_at`) and written to the daemon log.
//! `claude_limit_menu_clear = false` in daemon.toml, then
//! `daemon.reload_config`, stops new Escapes; pending checks still finish.

use crate::agent_state::{AgentStateCell, LimitMenuOutcome, LimitMenuRecord, PresenceStatus};
use crate::state::DaemonState;
use crate::workflow::pty_tracker::PtyModeTracker;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long the menu stays up, untouched, before CM presses Escape.
pub(crate) const GRACE: Duration = Duration::from_secs(120);
/// How long after an Escape the menu may take to close before it counts as
/// still open.
pub(crate) const VERIFY: Duration = Duration::from_secs(10);
/// Minimum gap between Escapes on the same boundary.
pub(crate) const RETRY: Duration = Duration::from_secs(30);
/// Escapes per turn boundary before the session is left for a human.
pub(crate) const MAX_ATTEMPTS: u32 = 3;
const POLL: Duration = Duration::from_secs(2);
/// Non-blank rows the footer may have below it.
const FOOTER_SLACK: usize = 2;
/// Rows between the question and the footer: options and their hints.
const MAX_MENU_ROWS: usize = 14;
/// Rows above the question that may carry the limit header.
const HEADER_ROWS: usize = 4;

/// The menu as drawn at the bottom of the screen. `selected` is the
/// highlighted option's row, so a moved highlight can restart the grace.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MenuView {
    selected: Option<String>,
}

/// Whitespace and box-drawing removed; typographic apostrophes normalized.
fn compact(line: &str) -> String {
    line.chars()
        .filter(|c| !c.is_whitespace() && !('\u{2500}'..='\u{257f}').contains(c))
        .map(|c| if c == '\u{2019}' { '\'' } else { c })
        .collect()
}

/// Claude's usage-limit menu, if it is what the bottom of `lines` (the
/// visible screen, top to bottom) shows.
pub(crate) fn limit_menu(lines: &[String]) -> Option<MenuView> {
    let rows: Vec<String> = lines.iter().map(|l| compact(l)).filter(|r| !r.is_empty()).collect();
    let footer = rows.iter().rposition(|r| r.contains("Entertoconfirm") && r.contains("Esctocancel"))?;
    if rows.len() - 1 - footer > FOOTER_SLACK {
        return None;
    }
    let question = rows[..footer].iter().rposition(|r| r == "Whatdoyouwanttodo?")?;
    if footer - question > MAX_MENU_ROWS {
        return None;
    }
    let options = &rows[question + 1..footer];
    if !options.concat().contains("Stopandwaitforlimittoreset") {
        return None;
    }
    let header = rows[question.saturating_sub(HEADER_ROWS)..question].concat();
    if !((header.contains("You'vehityour") || header.contains("You'vereachedyour")) && header.contains("limit")) {
        return None;
    }
    Some(MenuView { selected: options.iter().find(|r| r.starts_with('❯')).cloned() })
}

#[derive(Debug, PartialEq)]
enum Action {
    Wait,
    Escape,
    Record(LimitMenuOutcome),
    /// A human closed a menu CM had given up on: drop the flag.
    Forget,
}

/// The policy, free of I/O. `record` is the current boundary's only;
/// `seen_for` is how long the menu has been seen unchanged with presence
/// confirming the dialog (None when it is not).
fn decide(menu: bool, record: Option<&LimitMenuRecord>, seen_for: Option<Duration>,
    quiet: bool, enabled: bool, now: f64) -> Action {
    if !menu {
        return match record.map(|r| r.outcome) {
            Some(LimitMenuOutcome::EscapeSent) => Action::Record(LimitMenuOutcome::Cleared),
            Some(LimitMenuOutcome::NeedsHuman) => Action::Forget,
            _ => Action::Wait,
        };
    }
    let outcome = record.map(|r| r.outcome);
    let since_escape = record.map_or(f64::INFINITY, |r| now - r.at);
    let attempts = record.map_or(0, |r| r.attempts);
    let sent = outcome == Some(LimitMenuOutcome::EscapeSent);
    if outcome == Some(LimitMenuOutcome::NeedsHuman) || (sent && since_escape < VERIFY.as_secs_f64()) {
        return Action::Wait;
    }
    // Still on screen after the last allowed Escape. This verdict does not
    // wait for presence: a lapsed observation must not hide a stuck menu.
    if attempts >= MAX_ATTEMPTS && (sent || seen_for.is_some()) {
        return Action::Record(LimitMenuOutcome::NeedsHuman);
    }
    // Every new Escape needs presence confirming the dialog.
    let Some(seen_for) = seen_for else { return Action::Wait; };
    if !enabled || !quiet || attempts >= MAX_ATTEMPTS {
        return Action::Wait;
    }
    let due = if sent {
        since_escape >= RETRY.as_secs_f64()
    } else {
        // A first sighting, or the menu back at a boundary CM cleared once.
        seen_for >= GRACE
    };
    if due { Action::Escape } else { Action::Wait }
}

struct Target {
    uid: String,
    label: String,
    pid: libc::pid_t,
    generation: u64,
    dialog: bool,
    turn_seq: u64,
    record: Option<LimitMenuRecord>,
    cell: AgentStateCell,
    input: crate::session::InputHandle,
    fanout: Arc<crate::session::PtyByteFanout>,
    cols: u16,
    rows: u16,
}

struct Watch {
    pid: libc::pid_t,
    generation: u64,
    turn_seq: u64,
    view: MenuView,
    since: Instant,
}

type ScreenKey = (u64, u64, u16, u16, libc::pid_t, u64);

#[derive(Default)]
struct Memo {
    watches: HashMap<String, Watch>,
    screens: HashMap<String, (ScreenKey, Option<MenuView>)>,
}

fn targets(st: &DaemonState, watched: &HashMap<String, Watch>) -> Vec<Target> {
    let mut out = Vec::new();
    for (uid, s) in &st.sessions {
        if s.session_type != "claude-code" || s.last_exit.kernel_set() {
            continue;
        }
        let (dialog, turn_seq, record) = {
            let cell = s.agent_state.lock().unwrap_or_else(|p| p.into_inner());
            let dialog = cell.inputs.presence.as_ref().is_some_and(|p| p.valid
                && p.status == PresenceStatus::Waiting && p.waiting_for.as_deref() == Some("dialog open"));
            let turn_seq = cell.inputs.turn_seq;
            (dialog, turn_seq, cell.inputs.limit_menu.clone().filter(|m| m.turn_seq == turn_seq))
        };
        // Outstanding checks and give-up flags are revisited without a dialog.
        let pending = record.as_ref().is_some_and(|r| r.outcome != LimitMenuOutcome::Cleared);
        if !dialog && !pending && !watched.contains_key(uid) {
            continue;
        }
        out.push(Target {
            uid: uid.clone(), label: s.title.clone(), pid: s.pid, generation: s.generation,
            dialog, turn_seq, record, cell: Arc::clone(&s.agent_state), input: s.input_handle(),
            fanout: Arc::clone(&s.fanout), cols: s.last_cols, rows: s.last_rows,
        });
    }
    out.sort_by(|a, b| a.uid.cmp(&b.uid));
    out
}

/// The menu (or not) on the rendered ring, with the ring cursor and
/// geometry it was rendered at; None when the session's output has closed.
/// Unchanged output is not re-parsed.
fn render(memo: &mut Memo, t: &Target) -> Option<(Option<MenuView>, u64, u64)> {
    let geometry = t.fanout.geometry_version();
    let probe = t.fanout.snapshot_since(Some(u64::MAX));
    if probe.closed {
        return None;
    }
    let key = (probe.cursor, geometry, t.cols, t.rows, t.pid, t.generation);
    if let Some((cached, view)) = memo.screens.get(&t.uid) {
        if *cached == key {
            return Some((view.clone(), probe.cursor, geometry));
        }
    }
    let snapshot = t.fanout.snapshot_since(None);
    if snapshot.closed {
        return None;
    }
    let mut screen = PtyModeTracker::with_size(t.cols.max(1) as usize, t.rows.max(1) as usize);
    screen.feed(&snapshot.bytes, Instant::now());
    let view = limit_menu(&screen.visible_lines());
    if memo.screens.len() >= 512 {
        memo.screens.clear();
    }
    memo.screens.insert(t.uid.clone(), ((snapshot.cursor, geometry, t.cols, t.rows, t.pid, t.generation), view.clone()));
    Some((view, snapshot.cursor, geometry))
}

/// Replace the boundary's record, unless a new turn has started since.
fn set_record(t: &Target, record: Option<LimitMenuRecord>) -> bool {
    let mut cell = t.cell.lock().unwrap_or_else(|p| p.into_inner());
    if cell.inputs.turn_seq != t.turn_seq {
        return false;
    }
    cell.inputs.limit_menu = record;
    true
}

/// Final lifecycle check under the state lock, then the guarded write.
fn escape(state: &Arc<Mutex<DaemonState>>, t: &Target, cursor: u64, geometry: u64) -> std::io::Result<bool> {
    {
        let st = state.lock().unwrap_or_else(|p| p.into_inner());
        if st.restarting || !st.config.claude_limit_menu_clear {
            return Ok(false);
        }
        let Some(s) = st.sessions.get(&t.uid) else { return Ok(false); };
        if s.pid != t.pid || s.generation != t.generation || s.last_exit.kernel_set()
            || !t.input.belongs_to(s) || !Arc::ptr_eq(&s.agent_state, &t.cell)
            || crate::rotation::in_progress(&t.uid) || crate::handoff::in_progress(&t.uid)
            || crate::claude_recovery::admission_blocked(&t.uid)
            || crate::claude_recovery::has_pending_input(&t.uid)
        {
            return Ok(false);
        }
    }
    // Claude enables kitty keyboard mode; assume it as send_key does.
    let mode = t.fanout.keyboard().0
        .unwrap_or(alacritty_terminal::term::TermMode::DISAMBIGUATE_ESC_CODES);
    let bytes = crate::keys::key_bytes("escape", mode).map_err(std::io::Error::other)?;
    t.input.write_limit_menu_escape(&bytes, GRACE, || {
        let now = t.fanout.snapshot_since(Some(cursor));
        !now.closed && now.cursor == cursor && t.fanout.geometry_version() == geometry
    })
}

fn step(state: &Arc<Mutex<DaemonState>>, memo: &mut Memo, t: &Target, enabled: bool) {
    let Some((view, cursor, geometry)) = render(memo, t) else {
        memo.watches.remove(&t.uid);
        return;
    };
    let now = Instant::now();
    let seen_for = match (&view, t.dialog) {
        (Some(view), true) => {
            let fresh = memo.watches.get(&t.uid).is_none_or(|w| w.pid != t.pid
                || w.generation != t.generation || w.turn_seq != t.turn_seq || w.view != *view);
            if fresh {
                memo.watches.insert(t.uid.clone(), Watch { pid: t.pid, generation: t.generation,
                    turn_seq: t.turn_seq, view: view.clone(), since: now });
            }
            memo.watches.get(&t.uid).map(|w| now.saturating_duration_since(w.since))
        }
        _ => {
            memo.watches.remove(&t.uid);
            None
        }
    };
    let quiet = t.input.operator_quiet_for().is_none_or(|gap| gap >= GRACE);
    let at = crate::agent_state::unix_now();
    let attempts = t.record.as_ref().map_or(0, |r| r.attempts);
    let who = format!("{} ({})", t.uid, t.label);
    match decide(view.is_some(), t.record.as_ref(), seen_for, quiet, enabled, at) {
        Action::Wait => {}
        Action::Escape => match escape(state, t, cursor, geometry) {
            Ok(false) => {}
            result => {
                // A failed write may still have reached the PTY: it counts.
                let n = attempts + 1;
                set_record(t, Some(LimitMenuRecord { outcome: LimitMenuOutcome::EscapeSent,
                    at, attempts: n, turn_seq: t.turn_seq }));
                memo.watches.remove(&t.uid);
                match result {
                    Ok(_) => eprintln!("cm-daemon: Claude usage-limit menu on {who}: pressed Escape \
                        ({n}/{MAX_ATTEMPTS}); no option selected"),
                    Err(e) => eprintln!("cm-daemon: Claude usage-limit menu on {who}: Escape write \
                        failed ({n}/{MAX_ATTEMPTS}): {e}"),
                }
            }
        },
        Action::Record(LimitMenuOutcome::Cleared) => {
            let Some(mut record) = t.record.clone() else { return; };
            record.outcome = LimitMenuOutcome::Cleared;
            if set_record(t, Some(record)) {
                eprintln!("cm-daemon: Claude usage-limit menu on {who} cleared by CM's Escape; \
                    the session waits at its failed turn (resuming is claude_limit_recovery's decision)");
            }
        }
        Action::Record(LimitMenuOutcome::NeedsHuman) => {
            if set_record(t, Some(LimitMenuRecord { outcome: LimitMenuOutcome::NeedsHuman,
                at, attempts, turn_seq: t.turn_seq }))
            {
                eprintln!("cm-daemon: Claude usage-limit menu on {who} still open after \
                    {attempts} Escapes; left for a human");
            }
        }
        Action::Record(LimitMenuOutcome::EscapeSent) => {}
        Action::Forget => {
            if set_record(t, None) {
                eprintln!("cm-daemon: Claude usage-limit menu on {who} was closed by hand");
            }
        }
    }
}

fn pass(state: &Arc<Mutex<DaemonState>>, memo: &mut Memo) {
    let (enabled, targets) = {
        let st = state.lock().unwrap_or_else(|p| p.into_inner());
        if st.restarting {
            return;
        }
        (st.config.claude_limit_menu_clear, targets(&st, &memo.watches))
    };
    let live: HashSet<&str> = targets.iter().map(|t| t.uid.as_str()).collect();
    memo.watches.retain(|uid, _| live.contains(uid.as_str()));
    memo.screens.retain(|uid, _| live.contains(uid.as_str()));
    for t in &targets {
        step(state, memo, t, enabled);
    }
}

/// One background thread; screen parsing and the write never hold the
/// daemon lock.
pub(crate) fn start(state: &Arc<Mutex<DaemonState>>) -> std::io::Result<()> {
    let weak = Arc::downgrade(state);
    std::thread::Builder::new().name("cm-claude-limit-menu".into()).spawn(move || {
        let mut memo = Memo::default();
        loop {
            std::thread::sleep(POLL);
            let Some(state) = weak.upgrade() else { break; };
            let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pass(&state, &mut memo)));
            if run.is_err() {
                eprintln!("cm-daemon: Claude usage-limit menu pass panicked; continuing with fresh observations");
                memo = Memo::default();
            }
        }
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_state::{LastTurn, PresenceObs, TurnStatus};

    const WEEKLY: &str = "You've hit your weekly limit · resets Nov 9, 8am (UTC)\n\
        What do you want to do?\n\
        ❯ 1. Stop and wait for limit to reset\n  \
        2. Wait here, then continue automatically at 8am\n  \
        3. Switch to usage credits\n\
        Enter to confirm · Esc to cancel";

    /// Draw `text` on a fresh `cols`x`rows` screen through the terminal
    /// parser, the way the daemon renders a session's ring.
    fn screen(text: &str, cols: usize, rows: usize) -> Vec<String> {
        let mut t = PtyModeTracker::with_size(cols, rows);
        t.feed(text.replace('\n', "\r\n").as_bytes(), Instant::now());
        t.visible_lines()
    }

    fn conversation() -> String {
        (0..10).map(|i| format!("⏺ line {i} of the agent's earlier work\n")).collect()
    }

    #[test]
    fn matches_the_weekly_and_five_hour_menus_at_the_bottom_of_the_screen() {
        let weekly = screen(&format!("{}{WEEKLY}", conversation()), 100, 30);
        assert_eq!(limit_menu(&weekly), Some(MenuView { selected: Some("❯1.Stopandwaitforlimittoreset".into()) }));
        let five = WEEKLY.replace("weekly limit · resets Nov 9, 8am (UTC)", "5-hour limit · resets 4pm (UTC)");
        assert!(limit_menu(&screen(&five, 100, 30)).is_some(), "the 5-hour menu");
        let curly = WEEKLY.replace("You've", "You\u{2019}ve");
        assert!(limit_menu(&screen(&curly, 100, 30)).is_some(), "typographic apostrophe");
        let boxed: String = WEEKLY.lines().map(|l| format!("│ {l:<70} │\n")).collect();
        let boxed = format!("╭{}╮\n{boxed}╰{}╯", "─".repeat(72), "─".repeat(72));
        assert!(limit_menu(&screen(&boxed, 100, 30)).is_some(), "drawn inside a border");
        let narrow = screen(WEEKLY, 40, 30);
        assert!(limit_menu(&narrow).is_some(), "wrapped header and options at 40 columns: {narrow:#?}");
        let moved = WEEKLY.replace("❯ 1.", "  1.").replace("  2. Wait", "❯ 2. Wait");
        assert_eq!(limit_menu(&screen(&moved, 100, 30)).unwrap().selected.as_deref(),
            Some("❯2.Waithere,thencontinueautomaticallyat8am"), "the highlight is part of the view");
    }

    #[test]
    fn other_screens_never_match() {
        let option_two = "You've hit your weekly limit · resets Nov 9, 8am (UTC)\n\
            Claude Code will continue automatically at 8am. Keep this session open; it may still \
            pause for permission prompts. Press esc to cancel the wait.\n❯ ";
        let permission = "Bash command\n  rm -rf target\nDo you want to proceed?\n❯ 1. Yes\n  2. No\n\
            Esc to cancel · Tab to amend";
        let trial = "Your Claude Code trial has ended.\nWhat do you want to do?\n❯ 1. Upgrade your plan\n\
            Enter to confirm · Esc to cancel";
        // The menu quoted in the conversation, with another dialog drawn below it.
        let quoted = format!("{WEEKLY}\n{}{permission}", conversation());
        // The menu scrolled up: the composer is at the bottom.
        let composer = format!("{WEEKLY}\n{}❯ \n  ? for shortcuts", conversation());
        let no_header = WEEKLY.replacen("You've hit your weekly limit · resets Nov 9, 8am (UTC)\n", "", 1);
        let no_stop = WEEKLY.replace("Stop and wait for limit to reset", "Upgrade your plan");
        for (why, text) in [("option 2 confirmation", option_two.to_string()), ("permission dialog", permission.into()),
            ("trial ended", trial.into()), ("quoted above another dialog", quoted), ("scrolled up", composer),
            ("no limit header", no_header), ("no stop-and-wait option", no_stop)] {
            assert_eq!(limit_menu(&screen(&text, 100, 40)), None, "{why}");
        }
    }

    fn record(outcome: LimitMenuOutcome, at: f64, attempts: u32) -> LimitMenuRecord {
        LimitMenuRecord { outcome, at, attempts, turn_seq: 7 }
    }

    #[test]
    fn the_policy_waits_out_the_grace_bounds_attempts_and_never_acts_when_off() {
        let grace = Some(GRACE);
        let early = Some(GRACE - Duration::from_secs(1));
        assert_eq!(decide(true, None, early, true, true, 1000.0), Action::Wait, "inside the grace");
        assert_eq!(decide(true, None, None, true, true, 1000.0), Action::Wait, "presence does not confirm the dialog");
        assert_eq!(decide(true, None, grace, false, true, 1000.0), Action::Wait, "someone typed recently");
        assert_eq!(decide(true, None, grace, true, false, 1000.0), Action::Wait, "kill switch");
        assert_eq!(decide(true, None, grace, true, true, 1000.0), Action::Escape);
        let sent = record(LimitMenuOutcome::EscapeSent, 1000.0, 1);
        assert_eq!(decide(true, Some(&sent), None, true, true, 1005.0), Action::Wait, "still checking");
        assert_eq!(decide(true, Some(&sent), grace, true, true, 1020.0), Action::Wait, "retry is spaced");
        assert_eq!(decide(true, Some(&sent), grace, true, true, 1031.0), Action::Escape, "second try");
        assert_eq!(decide(true, Some(&sent), grace, true, false, 1031.0), Action::Wait, "no retry when off");
        assert_eq!(decide(false, Some(&sent), None, true, true, 1003.0), Action::Record(LimitMenuOutcome::Cleared));
        assert_eq!(decide(true, Some(&sent), None, true, true, 1031.0), Action::Wait,
            "a retry needs presence confirming the dialog");
        let spent = record(LimitMenuOutcome::EscapeSent, 1000.0, MAX_ATTEMPTS);
        assert_eq!(decide(true, Some(&spent), grace, true, true, 1005.0), Action::Wait, "the last one is still checking");
        assert_eq!(decide(true, Some(&spent), grace, true, true, 1011.0), Action::Record(LimitMenuOutcome::NeedsHuman));
        assert_eq!(decide(true, Some(&spent), None, true, false, 1011.0), Action::Record(LimitMenuOutcome::NeedsHuman),
            "a stuck menu is flagged even when presence lapsed or the switch is off");
        let flagged = record(LimitMenuOutcome::NeedsHuman, 1011.0, MAX_ATTEMPTS);
        assert_eq!(decide(true, Some(&flagged), grace, true, true, 5000.0), Action::Wait, "never past the bound");
        assert_eq!(decide(false, Some(&flagged), None, true, true, 5000.0), Action::Forget, "closed by hand");
        let cleared = record(LimitMenuOutcome::Cleared, 1000.0, 1);
        assert_eq!(decide(false, Some(&cleared), None, true, true, 5000.0), Action::Wait);
        assert_eq!(decide(true, Some(&cleared), early, true, true, 5000.0), Action::Wait, "a reopened menu waits again");
        assert_eq!(decide(true, Some(&cleared), grace, true, true, 5000.0), Action::Escape);
        let cleared_spent = record(LimitMenuOutcome::Cleared, 1000.0, MAX_ATTEMPTS);
        assert_eq!(decide(true, Some(&cleared_spent), grace, true, true, 5000.0), Action::Record(LimitMenuOutcome::NeedsHuman));
    }

    /// The failed-turn boundary `claude_account_recovery` admits on, as the
    /// presence reader and StopFailure hook leave it after a limit.
    fn failed_limit_turn(input: &crate::session::InputHandle) -> crate::agent_state::AgentState {
        let now = crate::agent_state::unix_now();
        let cell = input.test_agent_state();
        let mut cell = cell.lock().unwrap();
        cell.inputs.presence = Some(PresenceObs {
            valid: true, status: PresenceStatus::Idle, observed_at: now, status_updated_at: Some(now - 1.0),
            native_session_id: Some("93686392-56ae-455c-803d-6ce59cc55e29".into()),
            engine_version: Some("2.1.318".into()), waiting_for: None, main_turn_open: None,
            transcript_error: Some("rate_limit".into()),
            transcript_end: Some(LastTurn { ended_at: Some(now - 1.0), status: Some(TurnStatus::Failed) }),
            transcript_evidence_path: None, wait_resolution: None,
        });
        cell.inputs.latest_start = Some(now - 10.0);
        cell.inputs.background.complete = true;
        cell.recompute(now)
    }

    #[test]
    fn escape_keeps_the_failed_boundary_and_recovery_admission() {
        use crate::claude_account_recovery::failed_boundary;
        let (input, writes) = crate::session::InputHandle::test_handle_capturing();
        let agent = failed_limit_turn(&input);
        assert!(failed_boundary(&agent), "{agent:?}");
        let captured = Instant::now();
        assert!(input.write_limit_menu_escape(b"\x1b[27u", GRACE, || true).unwrap());
        assert_eq!(writes.lock().unwrap().iter().map(|(_, b)| b.clone()).collect::<Vec<_>>(), vec![b"\x1b[27u".to_vec()]);
        // CM's record does not disturb the boundary either.
        input.test_agent_state().lock().unwrap().inputs.limit_menu = Some(LimitMenuRecord {
            outcome: LimitMenuOutcome::Cleared, at: crate::agent_state::unix_now(), attempts: 1, turn_seq: agent.turn_seq });
        let after = input.test_agent_state().lock().unwrap().recompute(crate::agent_state::unix_now());
        assert!(failed_boundary(&after), "{after:?}");
        assert_eq!((after.turn_seq, after.last_turn.clone()), (agent.turn_seq, agent.last_turn.clone()));
        assert_eq!(after.detail.limit_menu.as_deref(), Some("cleared"));
        assert!(input.claude_limit_boundary_matches(agent.turn_seq, agent.last_turn.ended_at, &agent.background));
        // Recovery's admission (captured before the Escape) still publishes.
        let mut published = false;
        assert!(input.admit_claude_limit_notice(captured, agent.turn_seq, agent.last_turn.ended_at,
            &agent.background, || { published = true; Ok(true) }).unwrap());
        assert!(published);
        // The send_key path would have blocked it: why the Escape has its own write.
        let (keyed, _) = crate::session::InputHandle::test_handle_capturing();
        let agent = failed_limit_turn(&keyed);
        keyed.write_key(b"\x1b[27u").unwrap();
        assert!(!keyed.admit_claude_limit_notice(Instant::now(), agent.turn_seq, agent.last_turn.ended_at,
            &agent.background, || panic!("send_key's operator stamp refuses recovery")).unwrap());
    }

    #[test]
    fn the_escape_write_refuses_input_or_new_output_before_any_byte() {
        let (input, writes) = crate::session::InputHandle::test_handle_capturing();
        assert!(!input.write_limit_menu_escape(b"\x1b", GRACE, || false).unwrap(), "screen changed");
        input.stamp_operator_input_at(Instant::now() - Duration::from_secs(30));
        assert!(!input.write_limit_menu_escape(b"\x1b", GRACE, || true).unwrap(), "Owner typed 30 s ago");
        input.stamp_operator_input_at(Instant::now() - GRACE - Duration::from_secs(1));
        input.stamp_activity();
        assert!(!input.write_limit_menu_escape(b"\x1b", GRACE, || true).unwrap(), "an agent just sent input");
        assert!(writes.lock().unwrap().is_empty());
    }

    #[test]
    fn the_record_shows_until_the_next_turn() {
        let (input, _) = crate::session::InputHandle::test_handle_capturing();
        let agent = failed_limit_turn(&input);
        let cell = input.test_agent_state();
        let mut cell = cell.lock().unwrap();
        cell.inputs.limit_menu = Some(LimitMenuRecord { outcome: LimitMenuOutcome::NeedsHuman,
            at: 1234.0, attempts: MAX_ATTEMPTS, turn_seq: agent.turn_seq });
        let now = crate::agent_state::unix_now();
        let state = cell.recompute(now);
        assert_eq!((state.detail.limit_menu.as_deref(), state.detail.limit_menu_at), (Some("needs_human"), Some(1234.0)));
        let wire = serde_json::to_value(&state).unwrap();
        assert_eq!(wire["detail"]["limit_menu"], "needs_human");
        cell.note_input(now);
        let state = cell.recompute(now);
        assert!(state.detail.limit_menu.is_none() && state.detail.limit_menu_at.is_none(), "{state:?}");
    }

    /// End to end against a real PTY: a "Claude" that draws the menu and
    /// records what it receives. One kitty Escape arrives, nothing else; the
    /// next pass sees the menu gone and records it cleared.
    #[test]
    fn a_parked_menu_gets_one_escape_and_is_recorded_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let got = dir.path().join("got");
        let menu = WEEKLY.replace('\'', "'\\''").replace('\n', "\\r\\n");
        let script = format!("printf '\\033[>1u{menu}'; stty raw -echo; head -c 5 > '{}'; \
            printf '\\033[2J\\033[H❯ '; exec sleep 60", got.display());
        let mut p = crate::session::SpawnParams::new("ts-menu-0", "menu", "/bin/sh");
        p.args = vec!["-c".into(), script];
        p.workspace_id = "ws".into();
        p.session_type = "claude-code".into();
        let state = Arc::new(Mutex::new(DaemonState::new()));
        let session = crate::session::DaemonSession::spawn(p).expect("spawn");
        let cell = Arc::clone(&session.agent_state);
        state.lock().unwrap().sessions.insert("ts-menu-0".into(), session);
        let now = crate::agent_state::unix_now();
        cell.lock().unwrap().inputs.presence = Some(PresenceObs {
            valid: true, status: PresenceStatus::Waiting, observed_at: now, status_updated_at: Some(now),
            native_session_id: None, engine_version: Some("2.1.318".into()),
            waiting_for: Some("dialog open".into()), main_turn_open: None, transcript_error: None,
            transcript_end: None, transcript_evidence_path: None, wait_resolution: None,
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut memo = Memo::default();
        // Wait for the menu to be drawn, then age the watch past the grace.
        loop {
            pass(&state, &mut memo);
            if memo.watches.contains_key("ts-menu-0") { break; }
            assert!(Instant::now() < deadline, "the menu was never seen");
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(std::fs::read(&got).unwrap_or_default().is_empty(), "no Escape inside the grace");
        memo.watches.get_mut("ts-menu-0").unwrap().since = Instant::now() - GRACE;
        pass(&state, &mut memo);
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::fs::read(&got).unwrap_or_default().len() < 5 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(std::fs::read(&got).unwrap(), b"\x1b[27u", "exactly one kitty Escape");
        let sent = cell.lock().unwrap().inputs.limit_menu.clone().unwrap();
        assert_eq!((sent.outcome, sent.attempts), (LimitMenuOutcome::EscapeSent, 1));
        assert!(state.lock().unwrap().sessions["ts-menu-0"].last_input_at.lock().unwrap().is_none(),
            "the Escape is not input");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            pass(&state, &mut memo);
            if cell.lock().unwrap().inputs.limit_menu.as_ref().unwrap().outcome == LimitMenuOutcome::Cleared { break; }
            assert!(Instant::now() < deadline, "the cleared menu was never recorded");
            std::thread::sleep(Duration::from_millis(50));
        }
        for (_, mut s) in state.lock().unwrap().sessions.drain() {
            let _ = s.kill();
        }
    }
}
