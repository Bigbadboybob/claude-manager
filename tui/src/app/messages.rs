//! Owner messaging view. Network work runs off the terminal event loop.
mod channel;
mod conversation;
mod manage;
mod membership;
mod mentions;
use super::*;
use ratatui::widgets::Wrap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Draft {
    body: String,
    #[serde(default)]
    cursor: usize,
    request_id: String,
    reply_to: Option<String>,
    tags: Vec<String>,
    mentions: Vec<String>,
    #[serde(default)]
    entities: Vec<mentions::Mention>,
    links: Vec<Value>,
    origin: Option<String>,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Saved {
    management: manage::SavedManagement,
    dms_collapsed: bool,
    drafts: BTreeMap<String, Draft>,
    names: BTreeMap<String, (u64, String)>,
}
#[derive(Default)]
pub struct Messages {
    pub visible: bool,
    management: manage::Management,
    saved: Saved,
    target: Value,
    channels: Vec<Value>,
    people: Vec<Value>,
    channel_members: Vec<Value>,
    channel_candidates_ready: bool,
    dms: Vec<Value>,
    items: Vec<Value>,
    selected: usize,
    menu: usize,
    pane: u8,
    mode: String,
    text: String,
    fields: Vec<String>,
    field: usize,
    pending_send: Option<(String, String)>,
    page_cursor: Value,
    pub error: String,
    status: String,
    sync: Value,
    cache: Value,
    norms: String,
    filter: Value,
    next: Value,
    rx: Option<Receiver<(String, Result<Value, String>)>>,
    busy: bool,
    queued_events: std::collections::VecDeque<CrosstermEvent>,
    last_refresh: Option<Instant>,
    daemon_id: String,
    space_id: String,
    body_scroll: u16,
    timeline_scroll: usize,
    timeline_size: (u16, u16),
    reveal_selection: bool,
    picker_selected: usize,
    mention_selected: usize,
    mention_dismissed: bool,
    picker_members: Vec<String>,
    append_older: bool,
    select_older: bool,
    loaded_target: Value,
    receipt: Value,
    channel_edit_base: Value,
    channel_selection_pending: bool,
    pub settings_name: Option<(String, String, u64)>,
    /// Rendered rows per message id at a width (unselected); rebuilt only
    /// when that message changes, so scrolling never re-wraps history.
    layout: HashMap<String, (u16, Vec<Line<'static>>)>,
    layout_builds: u64,
    /// Older history is wanted (PgUp/k at the top, or the prefetch) but a
    /// request was in flight; fetched on the next idle tick.
    want_older: bool,
    /// The visible rows reach the oldest loaded message: prefetch.
    near_top: bool,
    /// n / N: the direction to keep seeking a mention after older history
    /// loads, with how many pages are left to try.
    seek_mention: (i8, u8),
    /// A one-line `y` confirm for marking a conversation read.
    confirm: Option<(String, Value)>,
    /// `+` was pressed: the next 1–5 toggles a reaction on the selection.
    reacting: bool,
    /// Screen width at the last draw (sidebar resize bounds).
    screen_width: u16,
    /// Older messages just prepended: the timeline keeps its place.
    prepended: usize,
    /// Bumped whenever the loaded messages change; keys `row_heights`.
    layout_epoch: u64,
    /// (epoch, width, count, rows per message incl. separator).
    row_heights: (u64, u16, usize, Vec<usize>),
}

/// The reactions Owner can add (the daemon's allowlist), keys 1–5 after `+`.
const REACTIONS: [&str; 5] = ["\u{2705}", "\u{1f440}", "\u{1f44d}", "\u{274c}", "\u{1f389}"];

/// Has Owner already reacted with `emoji`? `reactions_mine` when the daemon
/// supplies it, else Owner's display name among the reactors.
fn reacted_by_me(m: &Value, emoji: &str) -> bool {
    let r = &m["reactions"][emoji];
    if let Some(mine) = r["mine"].as_bool() {
        return mine;
    }
    match m["reactions_mine"].as_array() {
        Some(mine) => mine.iter().any(|e| e == emoji),
        None => reaction_names(r).iter().any(|n| n == "Owner"),
    }
}

/// Reactor names from `{count, names, mine}` or a bare name list.
fn reaction_names(r: &Value) -> Vec<String> {
    r["names"]
        .as_array()
        .or_else(|| r.as_array())
        .into_iter()
        .flatten()
        .filter_map(|n| n.as_str().map(str::to_owned))
        .collect()
}

/// Messages addressed to Owner: a structured mention or `@here`.
fn mentions_owner(m: &Value) -> bool {
    m["actor"]["id"] != "owner"
        && (m["data"]["mention_here"] == true
            || m["data"]
                .get("mention_recipients")
                .unwrap_or(&m["data"]["mentions"])
                .as_array()
                .is_some_and(|a| a.iter().any(|id| id == "owner")))
}

/// Shorten from the middle so both ends of a long name stay readable.
fn middle_truncate(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_owned();
    }
    if max <= 1 {
        return "\u{2026}".chars().take(max).collect();
    }
    let head = (max - 1).div_ceil(2);
    let tail = max - 1 - head;
    let mut out: String = s.chars().take(head).collect();
    out.push('\u{2026}');
    out.extend(s.chars().skip(n - tail));
    out
}

/// `@99+` style counts for the sidebar's count column.
fn count_label(prefix: char, n: u64) -> String {
    match n {
        0 => String::new(),
        1..=99 => format!("{prefix}{n}"),
        _ => format!("{prefix}99+"),
    }
}

/// Current UTC time as RFC 3339 (the TUI carries no date crate).
fn rfc3339_now() -> String {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let (days, secs) = (t.div_euclid(86_400), t.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", secs / 3600, secs % 3600 / 60, secs % 60)
}
impl Messages {
    pub fn load() -> Self {
        let saved = std::fs::read(Self::path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self {
            saved,
            target: json!({"channel":"general"}),
            filter: json!({}),
            ..Self::default()
        }
    }
    fn path() -> PathBuf {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(".cm/messages-owner-ui.json")
    }
    fn persist(&mut self) -> bool {
        if let Err(e) = cm_daemon::messaging::atomic_replace(
            &Self::path(),
            &serde_json::to_value(&self.saved).unwrap(),
        ) {
            self.error = format!("Draft not saved: {e}");
            return false;
        }
        true
    }
    fn key(&self) -> String {
        format!("{}:{}", self.space_id, self.target)
    }
    fn draft(&self) -> Draft {
        if self.mode == "norm_edit" {
            return self
                .saved
                .management
                .norms
                .get(&self.norms_key())
                .map(|n| Draft {
                    body: n.text.clone(),
                    cursor: n.cursor,
                    ..Draft::default()
                })
                .unwrap_or_default();
        }
        self.saved
            .drafts
            .get(&self.key())
            .cloned()
            .unwrap_or_default()
    }
    fn set_draft(&mut self, draft: Draft) -> bool {
        if self.mode == "norm_edit" {
            let n = self
                .saved
                .management
                .norms
                .entry(self.norms_key())
                .or_default();
            if n.text != draft.body {
                n.revert = None;
            }
            n.text = draft.body;
            n.cursor = draft.cursor;
            return self.persist();
        }
        self.saved.drafts.insert(self.key(), draft);
        self.persist()
    }
    fn menu_items(&self) -> Vec<(String, Value)> {
        let mut out = vec![
            ("Inbox".into(), json!({"inbox":true})),
            ("  Mentions".into(), json!({"mentions":true})),
            (
                "Needs Owner".into(),
                json!({"channel":"*","tags":["needs-owner"]}),
            ),
            (
                format!(
                    "Shared norms{}",
                    if self.management.context["current"]["global"] != self.management.context["acknowledged"]["global"] {
                        " · changed"
                    } else {
                        ""
                    }
                ),
                json!({"norms":true}),
            ),
            (
                format!("Monitors{}", self.management.badge_label()),
                json!({"monitors":true}),
            ),
            ("Preferences".into(), json!({"preferences":true})),
        ];
        out.push((
            "Browse channels · b".into(),
            json!({"channel_browser":true}),
        ));
        let mut channels: Vec<_> = self
            .channels
            .iter()
            .filter(|c| c["joined"] != false)
            .collect();
        channels.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
        // Counts are drawn in their own column before the name (menu_counts).
        out.extend(channels.into_iter().map(|c| {
            (
                format!(
                    "#{}",
                    c["name"]
                        .as_str()
                        .or_else(|| c["path"].as_str())
                        .unwrap_or("?")
                ),
                json!({"channel":c["path"]}),
            )
        }));
        let count: u64 = self.dms.iter().filter_map(|d| d["unread"].as_u64()).sum();
        out.push((
            format!(
                "{} DMs{}",
                if self.saved.dms_collapsed {
                    "▸"
                } else {
                    "▾"
                },
                if count > 0 {
                    format!(" ● {count}")
                } else {
                    String::new()
                }
            ),
            json!({"dm_section":true}),
        ));
        if !self.saved.dms_collapsed {
            let mut dms: Vec<_> = self.dms.iter().collect();
            dms.sort_by(|a, b| {
                b["last"]["created_at"]
                    .as_str()
                    .cmp(&a["last"]["created_at"].as_str())
                    .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
            });
            out.extend(dms.into_iter().map(|d| {
                (format!("  {}", self.dm_label(d)), json!({"conversation":d["id"]}))
            }));
        }
        out
    }
    /// (mentions, unread) for a sidebar row's count column.
    fn menu_counts(&self, target: &Value) -> (u64, u64) {
        let row = if let Some(path) = target["channel"].as_str() {
            self.channels.iter().find(|c| c["path"] == path)
        } else if target.get("conversation").is_some() {
            self.dms.iter().find(|d| d["id"] == target["conversation"])
        } else {
            None
        };
        row.map_or((0, 0), |r| (r["mentions"].as_u64().unwrap_or(0), r["unread"].as_u64().unwrap_or(0)))
    }
    /// Width of the sidebar count column (`@2 ●15 `), and each target's text.
    fn count_columns(&self) -> (usize, Vec<(String, String)>) {
        let rows: Vec<(String, String)> = self
            .menu_items()
            .iter()
            .map(|(_, t)| {
                let (m, u) = self.menu_counts(t);
                (count_label('@', m), count_label('\u{25cf}', u))
            })
            .collect();
        let mw = rows.iter().map(|r| r.0.chars().count()).max().unwrap_or(0);
        let uw = rows.iter().map(|r| r.1.chars().count()).max().unwrap_or(0);
        let width = if mw > 0 { mw + 1 } else { 0 } + if uw > 0 { uw + 1 } else { 0 };
        // Only conversation rows carry the column; menu rows stay flush.
        let conversation = |t: &Value| {
            t["channel"].as_str().is_some_and(|c| c != "*") || t.get("conversation").is_some()
        };
        let targets: Vec<Value> = self.menu_items().into_iter().map(|(_, t)| t).collect();
        let rows = rows
            .into_iter()
            .zip(targets)
            .map(|((m, u), t)| {
                if !conversation(&t) {
                    return (String::new(), String::new());
                }
                (
                    if mw > 0 { format!("{m:<mw$} ") } else { String::new() },
                    if uw > 0 { format!("{u:<uw$} ") } else { String::new() },
                )
            })
            .collect();
        (width, rows)
    }
    /// The conversation a mark-read applies to, with its label and counts.
    fn mark_read_target(&self) -> Option<(String, Value, u64, u64)> {
        let target = if self.pane == 0 {
            self.menu_items().get(self.menu).map(|(_, t)| t.clone())?
        } else {
            self.target.clone()
        };
        if target["inbox"] == true {
            return Some(("everything in Inbox".into(), json!({"inbox":true}), 0, 0));
        }
        if target["mentions"] == true {
            return Some(("all your mentions".into(), json!({"mentions":true}), 0, 0));
        }
        if target["channel"].as_str().is_some_and(|c| c != "*") || target.get("conversation").is_some() {
            let (mentions, unread) = self.menu_counts(&target);
            let name = if let Some(c) = target["channel"].as_str() {
                self.channels.iter().find(|ch| ch["path"] == c)
                    .map(|ch| format!("#{}", ch["name"].as_str().unwrap_or(c)))
                    .unwrap_or_else(|| format!("#{c}"))
            } else {
                self.conversation_label(&target["conversation"])
            };
            return Some((name, target, mentions, unread));
        }
        None
    }
    fn target_label(&self) -> String {
        if self.target["mentions"] == true {
            return "Mentions".into();
        }
        if self.target["norms"] == true {
            return self.target["norms_label"].as_str().unwrap_or("Shared norms").into();
        }
        if self.target["inbox"] == true {
            return "Inbox".into();
        }
        if self.target["dms"] == true {
            return "Incoming DMs".into();
        }
        if let Some(c) = self.target["channel"].as_str() {
            return if c == "*" {
                "Needs Owner".into()
            } else {
                self.channels.iter().find(|ch| ch["path"] == c).map(|ch| format!("#{}", ch["name"].as_str().unwrap_or(c))).unwrap_or_else(|| format!("#{c}"))
            };
        }
        if let Some(dm) = self.target.get("dm") {
            let ids: Vec<_> = if let Some(id) = dm.as_str() { vec![id] }
                else { dm.as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default() };
            return format!("DM {}", ids.iter().map(|id| self.person_name(id)).collect::<Vec<_>>().join(", "));
        }
        if let Some(d) = self.dms.iter().find(|d| d["id"] == self.target["conversation"]) {
            return format!("DM {}", self.dm_label(d));
        }
        if let Some(c) = self
            .channels
            .iter()
            .find(|c| c["id"] == self.target["conversation"])
        {
            return format!("#{}", c["name"].as_str().or_else(|| c["path"].as_str()).unwrap_or("?"));
        }
        "Conversation".into()
    }
    fn can_edit(&mut self) -> bool {
        if self.mode.starts_with("norm_") && self.saved.management.pending.is_some() {
            self.error = "An operation is pending. R retries its saved contents.".into();
            return false;
        }
        if !self.draft().request_id.is_empty() {
            self.error="Send outcome is pending. Ctrl+s retries the saved message; keep its contents until resolved.".into();
            false
        } else {
            true
        }
    }
    fn move_cursor(&mut self, key: KeyCode) {
        let mut d = self.draft();
        let at = d.cursor.min(d.body.len());
        d.cursor = match key {
            KeyCode::Left => d.body[..at]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0),
            KeyCode::Right => at + d.body[at..].chars().next().map(char::len_utf8).unwrap_or(0),
            KeyCode::Home => d.body[..at].rfind('\n').map(|i| i + 1).unwrap_or(0),
            KeyCode::End => d.body[at..]
                .find('\n')
                .map(|i| at + i)
                .unwrap_or(d.body.len()),
            KeyCode::Delete if d.request_id.is_empty() => {
                if let Some(c) = d.body[at..].chars().next() {
                    d.replace_text(at..at + c.len_utf8(), "");
                }
                at
            }
            _ => at,
        };
        self.mention_dismissed = false;
        self.mention_selected = 0;
        self.set_draft(d);
    }
    fn edit_text(&mut self, text: &str, backspace: bool) {
        if matches!(self.mode.as_str(), "compose" | "norm_edit") {
            if !self.can_edit() {
                return;
            }
            let mut d = self.draft();
            let at = d.cursor.min(d.body.len());
            if backspace {
                let previous = d.body[..at]
                    .char_indices()
                    .last()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                d.replace_text(previous..at, "");
            } else {
                d.replace_text(at..at, text);
            }
            self.mention_selected = 0;
            self.mention_dismissed = false;
            self.set_draft(d);
        } else if !self.fields.is_empty() {
            let field = &mut self.fields[self.field];
            if backspace {
                field.pop();
            } else {
                field.push_str(text);
            }
        } else if backspace {
            self.text.pop();
        } else {
            self.text.push_str(text);
        }
    }
    fn start_form(&mut self, mode: &str, fields: Vec<String>) {
        self.mode = mode.into();
        self.fields = fields;
        self.field = 0;
        self.text.clear();
    }
    fn complete_mention(&mut self) {
        if self.mode != "metadata" || self.field != 1 {
            return;
        }
        let text = &mut self.fields[1];
        let start = text.rfind(',').map(|n| n + 1).unwrap_or(0);
        let prefix = text[start..].trim().trim_start_matches('@').to_lowercase();
        let matches: Vec<_> = self
            .people
            .iter()
            .filter_map(|p| p["name"].as_str())
            .filter(|n| n.to_lowercase().starts_with(&prefix))
            .collect();
        if matches.len() == 1 {
            text.replace_range(start.., &format!("{}", matches[0]));
        } else {
            self.status = format!("Matches: {}", matches.join(", "));
        }
    }
    #[cfg(test)]
    pub(super) fn target_for_test(&self) -> &Value {
        &self.target
    }
    fn sync_label(&self) -> String {
        if self.sync["enabled"] != true {
            return String::new();
        }
        let pending = self.sync["pending"].as_u64().unwrap_or(0);
        let connection = if self.sync["connected"] == true {
            "online"
        } else {
            "offline"
        };
        if pending > 0 {
            format!(" · {connection} · {pending} syncing")
        } else {
            format!(" · {connection}")
        }
    }
    /// Refresh faster while a conversation's history is still arriving, so the
    /// board fills in as the hub backfill progresses.
    fn refresh_interval(&self) -> Duration {
        if self.target["mentions"] == true {
            // A multi-page inbox scan; mentions are also pushed as wakes.
            Duration::from_secs(15)
        } else if self.cache["backfill"].is_object() {
            Duration::from_secs(1)
        } else {
            Duration::from_secs(3)
        }
    }
    fn query(&self) -> Value {
        let mut p = self.target.clone();
        if let Some(f) = self.filter.as_object() {
            for (k, v) in f {
                p[k] = v.clone();
            }
        }
        p["limit"] = json!(20);
        p["newest_first"] = json!(true);
        p["claim_bell"] = json!(true);
        p
    }
}
impl App {
    /// F8 / Alt+m opening Messages while something is unread: start on the
    /// Inbox, newest first, instead of the last channel (status-bar `✉`/`@`).
    fn open_messages_on_unread(&mut self) {
        if self.unread.total() == 0 {
            return;
        }
        self.messages.menu = 0;
        self.messages.target = json!({"inbox": true});
        self.messages.filter = json!({});
        self.messages.page_cursor = Value::Null;
        self.messages.pane = 1;
        self.messages.selected = 0;
        self.messages.body_scroll = 0;
        self.messages.channel_selection_pending = false;
    }

    pub fn messaging_tick(&mut self) {
        // Refreshes run off-thread, but input still belongs to the operator.
        // Replay it in order before starting another refresh. Closing the panel
        // hides rendering while queued edits finish saving to their draft.
        while !self.messages.busy {
            let Some(event) = self.messages.queued_events.pop_front() else {
                break;
            };
            let visible = self.messages.visible;
            self.messages.visible = true;
            self.messaging_event(&event);
            if !visible {
                self.messages.visible = false;
            }
            self.needs_redraw = true;
        }
        let result = self.messages.rx.as_ref().and_then(|rx| rx.try_recv().ok());
        if let Some((method, result)) = result {
            self.messages.busy = false;
            self.messages.rx = None;
            self.needs_redraw = true;
            match result {
                Err(e) => {
                    self.messaging_management_error(&method, &e);
                    if method == "messaging.send" {
                        if let Some((key, request)) = self.messages.pending_send.take() {
                            // Only explicit pre-commit rejections permit changing the intent.
                            let rejected = [
                                "message_too_long:",
                                "invalid_message:",
                                "invalid_params:",
                                "invalid_target:",
                                "invalid_name:",
                                "reserved_name:",
                                "not_found:",
                                "invalid_mention:",
                                "join_required:",
                                "conversation_archived:",
                                "invalid_link:",
                                "event_too_large:",
                            ];
                            if rejected.iter().any(|code| e.contains(code)) {
                                if let Some(d) = self.messages.saved.drafts.get_mut(&key) {
                                    if d.request_id == request {
                                        d.request_id.clear();
                                        d.origin = None;
                                    }
                                }
                                self.messages.persist();
                            }
                        }
                    }
                    self.messages.error = if method == "messaging.react"
                        && (e.contains("unknown") || e.contains("not implemented"))
                    {
                        "Reactions need the updated daemon (next deploy)".into()
                    } else {
                        e
                    };
                }
                Ok(v) => {
                    self.messages.error.clear();
                    let status = if method == "bootstrap" {
                        &v["open"]
                    } else {
                        &v
                    };
                    if status["sync"].is_object() {
                        self.messages.sync = status["sync"].clone();
                    }
                    if status["cache"].is_object() {
                        self.messages.cache = status["cache"].clone();
                    }
                    self.messaging_context(&v);
                    if self.messaging_management_result(&method, &v) {
                        return;
                    }
                    match method.as_str() {
                        "bootstrap" => {
                            self.messages.channels = v["channels"]["items"]
                                .as_array()
                                .cloned()
                                .unwrap_or_default();
                            self.messages.people =
                                v["people"]["items"].as_array().cloned().unwrap_or_default();
                            self.messages.daemon_id =
                                v["people"]["daemon_id"].as_str().unwrap_or("").into();
                            self.messages.space_id =
                                v["people"]["space_id"].as_str().unwrap_or("").into();
                            self.messaging_context(&v["open"]);
                            self.messages.norms =
                                v["open"]["norms"]["text"].as_str().unwrap_or("").into();
                            self.messages.dms = v["dms"]["items"]
                                .as_array()
                                .cloned()
                                .unwrap_or_default();
                            self.messages.select_saved_channel();
                            self.messaging_refresh_target();
                        }
                        "messaging.read" => {
                            self.messages.accept_messages(&v);
                            if self.messages.seek_mention.0 != 0 {
                                self.messaging_seek_mention(self.messages.seek_mention.0);
                            }
                            if let Some(reason) = v["degraded"].as_str() {
                                self.messages.error = format!("Storage is read only: {reason}");
                            }
                            self.messages.status =
                                read_status(self.messages.items.len(), &v);
                        }
                        "messaging.send" => {
                            if let Some((key, request)) = self.messages.pending_send.take() {
                                if self
                                    .messages
                                    .saved
                                    .drafts
                                    .get(&key)
                                    .is_some_and(|d| d.request_id == request)
                                {
                                    self.messages.saved.drafts.remove(&key);
                                    self.messages.persist();
                                }
                            }
                            self.messages.mode.clear();
                            self.messages.status = if v["sync"]["enabled"] == true {
                                if v["replication"] == "replicated" {
                                    "Saved · synced"
                                } else {
                                    "Saved locally · syncing"
                                }
                            } else {
                                "Sent · stored locally"
                            }
                            .into();
                            if let Some(cid) = v["event"]["conversation_id"].as_str() {
                                self.messages.target = json!({"conversation":cid});
                            }
                            self.messages.page_cursor = Value::Null;
                            self.messages.loaded_target = Value::Null;
                            self.messaging_request("messaging.read", self.messages.query());
                        }
                        "mentions_list" => {
                            self.messages.accept_messages(&v);
                            self.messages.status = format!("{} mentions · Enter opens in context · M marks all read", self.messages.items.len());
                        }
                        "jump" => {
                            self.messages.accept_messages(&v);
                            if let Some(i) = self.messages.items.iter().position(|m| m["id"] == v["_jump"]) {
                                self.messages.selected = i;
                                self.messages.reveal_selection = true;
                                self.messages.status = "Jumped to the mention · n/N next/previous mention".into();
                            } else {
                                self.messages.status = "That message is older than the loaded history".into();
                            }
                        }
                        "mark_read" => {
                            self.messages.status = match v["marked"].as_u64() {
                                Some(n) => format!("Marked {n} read"),
                                None => "Marked read".into(),
                            };
                            self.messaging_apply_counts(&v["counts"]);
                            self.messages.page_cursor = Value::Null;
                            self.messaging_refresh_target();
                        }
                        "messaging.react" => {
                            self.messages.accept_reaction(&v);
                            self.messaging_apply_counts(&v["counts"]);
                        }
                        "channel_members" => {
                            self.messages.channel_members = v["items"].as_array().cloned().unwrap_or_default();
                        }
                        "channel_add_candidates" => {
                            self.messages.channel_members = v["members"]["items"].as_array().cloned().unwrap_or_default();
                            self.messages.people = v["people"]["items"].as_array().cloned().unwrap_or_default();
                            self.messages.channel_candidates_ready = true;
                        }
                        "pin_prepare" => self.messaging_pin_prepared(&v),
                        "session.set_name" => {
                            self.messages.status = "Session name updated".into();
                            self.messaging_request("bootstrap", json!({}));
                        }
                        _ => {}
                    }
                }
            }
        }
        if self.messages.visible
            && !self.messages.busy
            && (self.messages.want_older || self.messages.near_top)
            && !self.messages.next.is_null()
            // A failed page waits for the regular refresh, not every tick.
            && self.messages.error.is_empty()
            && self.messages.target["mentions"] != true
        {
            let select = std::mem::take(&mut self.messages.want_older) && self.messages.selected == 0;
            self.messages.near_top = false;
            self.messaging_load_older(select);
            return;
        }
        if self.messages.visible
            && !self.messages.busy
            && self.messages.mode.is_empty()
            && (self.messages.page_cursor.is_null() || self.messages.live_conversation())
            && self
                .messages
                .last_refresh
                .is_none_or(|t| t.elapsed() > self.messages.refresh_interval())
        {
            self.messaging_refresh_target();
        }
    }
    fn messaging_request(&mut self, method: &str, mut params: Value) {
        if matches!(method, "messaging.norms" | "norms_document") && params.get("scope").is_none() {
            params["scope"] = json!(self.messages.norms_scope());
        }
        if self.messages.busy {
            return;
        }
        let host = cm_daemon::host_id::HostId::local();
        let Some(socket) = self.host_pool.live_socket_path(&host) else {
            self.messages.error = "Local daemon is unavailable".into();
            return;
        };
        let token = self.host_pool.operator_token_for(&host);
        self.messages.management.request = params.clone();
        let refresh = matches!(method, "messaging.read" | "hub_refresh")
            && self.messages.live_conversation()
            && self.messages.loaded_target == self.messages.query()
            && params["cursor"].is_null();
        // Reading a conversation marks it read (the daemon's default for a
        // direct read); a background refresh only does so while Owner is at
        // the newest message, not while scrolled back in history.
        if refresh {
            params["mark_read"] = json!(self.messages.selected + 1 >= self.messages.items.len());
            self.messages.management.request = params.clone();
        }
        let catch_up_to = (method == "messaging.read" && refresh)
            .then(|| self.messages.items.last().map(|m| m["id"].clone())).flatten();
        let method = method.to_owned();
        let (tx, rx) = mpsc::channel();
        self.messages.rx = Some(rx);
        self.messages.busy = true;
        self.messages.last_refresh = Some(Instant::now());
        std::thread::spawn(move || {
            let call = |m: &str, p: Value| {
                crate::client_session::rpc_messaging_board(&socket, &token, m, p)
                    .map_err(|e| e.to_string())
            };
            let directory = |method: &str, mut params: Value| -> Result<Value, String> {
                let mut out = Vec::new();
                loop {
                    let page = call(method, params.clone())?;
                    out.extend(page["items"].as_array().cloned().unwrap_or_default());
                    if page["next_cursor"].is_null() {
                        let mut value = page;
                        value["items"] = json!(out);
                        return Ok(value);
                    }
                    params["cursor"] = page["next_cursor"].clone();
                }
            };
            let result = if method == "bootstrap" {
                (|| {
                    Ok(
                        json!({"channels":directory("messaging.channels",json!({"action":"list"}))?,"people":directory("messaging.people",json!({"include_exited":true}))?,"open":call("messaging.open",json!({"channel":"general","claim_bell":true}))?,"dms":directory("messaging.dms",json!({}))?}),
                    )
                })()
            } else if method == "messaging.read" || method == "hub_refresh" {
                (|| {
                    if method == "hub_refresh" {
                        let mut fresh = params.clone();
                        fresh["freshness"] = json!("hub");
                        call("messaging.read", fresh)?;
                    }
                    let mut value = call("messaging.read", params.clone())?;
                    // A burst can exceed a read page. Catch up to the last known
                    // message before merging, using the first page's frozen cursor.
                    if let Some(anchor) = catch_up_to {
                        while !value["items"]
                            .as_array()
                            .is_some_and(|items| items.iter().any(|m| m["id"] == anchor))
                            && !value["next_cursor"].is_null()
                        {
                            let mut older = params.clone();
                            older["cursor"] = value["next_cursor"].clone();
                            let page = call(&method, older)?;
                            value["items"]
                                .as_array_mut()
                                .unwrap()
                                .extend(page["items"].as_array().cloned().unwrap_or_default());
                            value["receipt"]["ids"].as_array_mut().unwrap().extend(
                                page["receipt"]["ids"]
                                    .as_array()
                                    .cloned()
                                    .unwrap_or_default(),
                            );
                            value["next_cursor"] = page["next_cursor"].clone();
                        }
                    }
                    // Sidebar failures must not hide successfully read messages.
                    // Older pages skip the sidebar: paging back stays one call.
                    if !params["cursor"].is_null() {
                        return Ok(value);
                    }
                    if let Ok(dms) = directory("messaging.dms", json!({})) {
                        value["_dms"] = dms["items"].clone();
                    }
                    if let Ok(channels) = directory("messaging.channels", json!({})) {
                        value["_channels"] = channels["items"].clone();
                    }
                    if let Ok(people) =
                        directory("messaging.people", json!({"include_exited":true}))
                    {
                        value["_people"] = people["items"].clone();
                    }
                    Ok(value)
                })()
            } else if method == "mentions_list" {
                // Owner's mentions, newest first: inbox pages filtered here
                // (the read API has no mention filter).
                (|| {
                    let mut out = Vec::new();
                    // mentions_only filters on the daemon; the client filter
                    // below keeps an older daemon (which ignores it) correct.
                    let mut p = json!({"inbox":true,"mentions_only":true,"newest_first":true,"limit":200});
                    for _ in 0..5 {
                        let page = call("messaging.read", p.clone())?;
                        out.extend(page["items"].as_array().into_iter().flatten().filter(|m| mentions_owner(m)).cloned());
                        if page["next_cursor"].is_null() || out.len() >= 200 {
                            break;
                        }
                        p["cursor"] = page["next_cursor"].clone();
                    }
                    Ok(json!({"items": out, "next_cursor": null}))
                })()
            } else if method == "jump" {
                // Load a conversation back to a given message (≤ 40 pages).
                (|| {
                    let id = params["_jump"].clone();
                    let mut p = params.clone();
                    p.as_object_mut().unwrap().remove("_jump");
                    let mut value = call("messaging.read", p.clone())?;
                    for _ in 0..40 {
                        if value["items"].as_array().is_some_and(|i| i.iter().any(|m| m["id"] == id))
                            || value["next_cursor"].is_null()
                        {
                            break;
                        }
                        let mut older = p.clone();
                        older["cursor"] = value["next_cursor"].clone();
                        let page = call("messaging.read", older)?;
                        value["items"].as_array_mut().unwrap().extend(page["items"].as_array().cloned().unwrap_or_default());
                        value["next_cursor"] = page["next_cursor"].clone();
                    }
                    value["_jump"] = id;
                    Ok(value)
                })()
            } else if method == "mark_read" {
                // Inbox: one mark_read_before. A conversation or the mention
                // list: collect its unread ids, then acknowledge them in
                // receipts of ≤ 200 (the ordinary receipt path).
                (|| {
                    let target = params["target"].clone();
                    let unsupported = |e: &str| {
                        let l = e.to_lowercase();
                        l.contains("not implemented") || (l.contains("unknown") && l.contains("method"))
                    };
                    // Read cursors: one O(1) write per affected conversation;
                    // the reply carries the new counts.
                    let request = if target["inbox"] == true {
                        json!({"all": true})
                    } else if target["mentions"] == true {
                        json!({"mentions": true})
                    } else {
                        target.clone()
                    };
                    match call("messaging.mark_read", request) {
                        Err(e) if unsupported(&e) => {}
                        other => return other,
                    }
                    // An older daemon: the inbox bulk mark, else receipts.
                    if target["inbox"] == true {
                        return call("messaging.read", json!({"inbox":true,"mark_read_before":rfc3339_now()}));
                    }
                    // An older daemon: acknowledge the unread ids in receipts.
                    let mentions_only = target["mentions"] == true;
                    let mut q = if mentions_only { json!({"inbox":true}) } else { target.clone() };
                    q["unread_only"] = json!(true);
                    q["limit"] = json!(200);
                    let mut ids: Vec<Value> = Vec::new();
                    for _ in 0..100 {
                        let page = call("messaging.read", q.clone())?;
                        ids.extend(
                            page["items"].as_array().into_iter().flatten()
                                .filter(|m| !mentions_only || mentions_owner(m))
                                .map(|m| m["id"].clone()),
                        );
                        if page["next_cursor"].is_null() {
                            break;
                        }
                        q["cursor"] = page["next_cursor"].clone();
                    }
                    let base = if mentions_only { json!({"inbox":true}) } else { target };
                    for chunk in ids.chunks(200) {
                        let mut p = base.clone();
                        p["limit"] = json!(1);
                        p["ack_receipt"] = json!({"actor":"owner","space_id":params["space_id"],"ids":chunk});
                        call("messaging.read", p)?;
                    }
                    Ok(json!({"marked": ids.len()}))
                })()
            } else if method == "channel_members" {
                directory("messaging.channels", params)
            } else if method == "channel_add_candidates" {
                (|| {
                    Ok(json!({
                        "members": directory("messaging.channels", params)?,
                        "people": directory("messaging.people", json!({"include_exited":true}))?
                    }))
                })()
            } else if method == "pin_prepare" {
                (|| {
                    let mut value = call(
                        "messaging.pins",
                        json!({"conversation":params["conversation"],"limit":1}),
                    )?;
                    value["intent"] = params;
                    Ok(value)
                })()
            } else if method == "norms_document" {
                (|| {
                    let mut p = params;
                    let mut text = String::new();
                    loop {
                        let mut page = call("messaging.norms", p.clone())?;
                        text.push_str(page["text"].as_str().unwrap_or(""));
                        if page["next_cursor"].is_null() {
                            page["text"] = json!(text);
                            page["offset"] = json!(0);
                            return Ok(page);
                        }
                        p["cursor"] = page["next_cursor"].clone();
                    }
                })()
            } else {
                call(&method, params)
            };
            let _ = tx.send((
                if method == "hub_refresh" {
                    "messaging.read".into()
                } else {
                    method
                },
                result,
            ));
        });
    }
    pub(super) fn messaging_event(&mut self, event: &CrosstermEvent) -> bool {
        if let CrosstermEvent::Paste(text) = event {
            if self.messages.visible && self.messages.busy {
                self.messages.queued_events.push_back(event.clone());
                return true;
            }
            if self.messages.visible && !self.messages.busy && !self.messages.mode.is_empty() {
                if self.messages.management_view()
                    && self.messages.saved.management.pending.is_some()
                {
                    self.messages.error = format!("Unconfirmed {} is saved; R retries it", self.messages.pending_label());
                } else {
                    self.messages.edit_text(&text.replace("\r\n", "\n"), false);
                    self.messages.keep_management_form();
                }
            }
            return self.messages.visible;
        }
        let CrosstermEvent::Key(key) = event else {
            return self.messages.visible;
        };
        if key.code == KeyCode::F(8)
            || key.modifiers.contains(KeyModifiers::ALT)
                && key.code == KeyCode::Char('m')
                && !key.modifiers.contains(KeyModifiers::SHIFT)
        {
            self.messages.visible = !self.messages.visible;
            if self.messages.visible {
                self.open_messages_on_unread();
                self.messaging_request("bootstrap", json!({}));
            }
            return true;
        }
        if !self.messages.visible {
            return false;
        }
        // Moving around never waits for a refresh: only keys that change
        // state queue behind the request in flight.
        let navigation = self.messages.mode.is_empty()
            && self.messages.confirm.is_none()
            && !self.messages.reacting
            && self.messages.queued_events.is_empty()
            && !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && matches!(
                key.code,
                KeyCode::Char('j' | 'k') | KeyCode::Up | KeyCode::Down | KeyCode::PageUp
                    | KeyCode::PageDown | KeyCode::Home | KeyCode::End
            );
        if self.messages.busy && !navigation {
            if key.code == KeyCode::Esc {
                self.messages.visible = false;
            } else if !(key.code == KeyCode::Char('s')
                && key.modifiers.contains(KeyModifiers::CONTROL)
                && (self.messages.pending_send.is_some()
                    || self.messages.saved.management.pending.is_some()))
            {
                self.messages.queued_events.push_back(event.clone());
            }
            return true;
        }
        if let Some((_, target)) = self.messages.confirm.take() {
            if key.code == KeyCode::Char('y') {
                self.messages.status = "Marking read…".into();
                self.messaging_request("mark_read", json!({"target": target, "space_id": self.messages.space_id}));
            } else {
                self.messages.status = "Not marked".into();
            }
            return true;
        }
        if std::mem::take(&mut self.messages.reacting) {
            match key.code {
                KeyCode::Char(c @ '1'..='5') => self.messaging_react((c as u8 - b'1') as usize),
                _ => self.messages.status = "No reaction".into(),
            }
            return true;
        }
        if self.messages.mode.is_empty() && key.modifiers.contains(KeyModifiers::CONTROL) {
            return true;
        }
        if self.messages.mode.is_empty()
            && key.code == KeyCode::Char('+')
            && self.messages.pane == 1
            && !self.messages.management_view()
        {
            if self.messages.items.get(self.messages.selected).is_some() {
                self.messages.reacting = true;
                self.messages.status = format!(
                    "React: {} · same key again removes yours · any other key cancels",
                    REACTIONS.iter().enumerate().map(|(i, e)| format!("{} {e}", i + 1)).collect::<Vec<_>>().join("  ")
                );
            }
            return true;
        }
        // In a conversation n / N step between mentions of Owner (in the
        // sidebar they keep their meaning: new channel / norms).
        if self.messages.mode.is_empty()
            && self.messages.pane == 1
            && !self.messages.management_view()
            && matches!(key.code, KeyCode::Char('n' | 'N'))
            && !key.modifiers.contains(KeyModifiers::ALT)
        {
            let forward = key.code == KeyCode::Char('n');
            // The mention list is newest first: "next" goes down the list.
            self.messaging_seek_mention(if forward { 1 } else { -1 });
            return true;
        }
        if self.messaging_channel_browser_key(key) || self.messages.mention_key(key) {
            return true;
        }
        if self.messaging_picker_key(key) {
            return true;
        }
        if self.messaging_management_key(key) {
            return true;
        }
        if !self.messages.mode.is_empty() {
            match key.code {
                KeyCode::Left | KeyCode::Right | KeyCode::Home | KeyCode::End | KeyCode::Delete
                    if self.messages.mode == "compose" =>
                {
                    self.messages.move_cursor(key.code)
                }
                KeyCode::Tab if !self.messages.fields.is_empty() => {
                    self.messages.field = (self.messages.field + 1) % self.messages.fields.len()
                }
                KeyCode::BackTab if self.messages.mode == "metadata" => {
                    self.messages.complete_mention()
                }
                KeyCode::Esc => {
                    self.messages.mode.clear();
                    self.messages.persist();
                }
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if self.messages.mode == "compose" {
                        self.messaging_send();
                    } else {
                        self.messaging_submit_form();
                    }
                }
                KeyCode::Enter if self.messages.mode != "compose" => self.messaging_submit_form(),
                KeyCode::Backspace => self.messages.edit_text("", true),
                KeyCode::Enter => self.messages.edit_text("\n", false),
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.messages.edit_text(&c.to_string(), false)
                }
                _ => {}
            }
            return true;
        }
        match key.code {
            KeyCode::Esc => self.messages.visible = false,
            KeyCode::Tab => self.messages.pane = (self.messages.pane + 1) % 2,
            KeyCode::Char('j') | KeyCode::Down => {
                if self.messages.pane == 0 {
                    self.messages.menu = (self.messages.menu + 1)
                        .min(self.messages.menu_items().len().saturating_sub(1));
                } else if self.messages.target["norms"] == true {
                    self.messages.body_scroll = self.messages.body_scroll.saturating_add(1);
                } else {
                    self.messages.selected = (self.messages.selected + 1)
                        .min(self.messages.items.len().saturating_sub(1));
                    self.messages.reveal_selection = true;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.messages.pane == 0 {
                    self.messages.menu = self.messages.menu.saturating_sub(1);
                } else if self.messages.target["norms"] == true {
                    self.messages.body_scroll = self.messages.body_scroll.saturating_sub(1);
                } else {
                    if self.messages.selected == 0 && !self.messages.next.is_null() {
                        self.messaging_load_older(true);
                    } else {
                        self.messages.selected = self.messages.selected.saturating_sub(1);
                    }
                    self.messages.reveal_selection = true;
                }
            }
            KeyCode::PageDown => {
                let page = (self.messages.timeline_size.1 as usize).saturating_sub(2).max(10);
                self.messages.timeline_scroll = self.messages.timeline_scroll.saturating_add(page);
                self.messages.reveal_selection = false;
            }
            KeyCode::PageUp => {
                let page = (self.messages.timeline_size.1 as usize).saturating_sub(2).max(10);
                if self.messages.timeline_scroll == 0 {
                    self.messaging_load_older(false);
                }
                self.messages.timeline_scroll = self.messages.timeline_scroll.saturating_sub(page);
                self.messages.reveal_selection = false;
            }
            KeyCode::Home if self.messages.pane == 1 => {
                self.messages.selected = 0;
                self.messages.reveal_selection = true;
            }
            KeyCode::End if self.messages.pane == 1 => {
                self.messages.selected = self.messages.items.len().saturating_sub(1);
                self.messages.reveal_selection = true;
            }
            KeyCode::Char('M') => self.messaging_confirm_mark_read(),
            KeyCode::Char('<') => self.messaging_resize_sidebar(-2),
            KeyCode::Char('>') => self.messaging_resize_sidebar(2),
            KeyCode::Char('d') => {
                self.messages.mode = "dm_picker".into();
                self.messages.text.clear();
                self.messages.fields.clear();
                self.messages.error.clear();
                self.messages.picker_selected = 0;
                self.messages.picker_members.clear();
            }
            KeyCode::Char(' ') if self.messages.pane == 0 => {
                if self.messages.menu_items().get(self.messages.menu).is_some_and(|(_, t)| t["dm_section"] == true) {
                    self.messages.saved.dms_collapsed = !self.messages.saved.dms_collapsed;
                    self.messages.persist();
                }
            }
            KeyCode::Enter => {
                if self.messages.pane == 0 {
                    if let Some((_, target)) =
                        self.messages.menu_items().get(self.messages.menu).cloned()
                    {
                        if target["channel_browser"] == true {
                            self.messages.browse_channels();
                            return true;
                        }
                        if target["dm_section"] == true {
                            self.messages.saved.dms_collapsed = !self.messages.saved.dms_collapsed;
                            self.messages.persist();
                            return true;
                        }
                        self.messages.target = target;
                        self.messages.filter = json!({});
                        self.messages.page_cursor = Value::Null;
                        self.messages.seek_mention = (0, 0);
                        self.messages.want_older = false;
                        self.messages.pane = 1;
                        self.messages.selected = 0;
                        self.messages.body_scroll = 0;
                        self.messaging_refresh_target();
                    }
                } else if self.messages.target["mentions"] == true {
                    self.messaging_open_in_context();
                } else {
                    self.messaging_ack_selected();
                }
            }
            KeyCode::Char('b') => self.messages.browse_channels(),
            KeyCode::Char('u') => self.messaging_channel_members(),
            KeyCode::Char('J') => self.messaging_membership(true),
            KeyCode::Char('L') => self.messaging_membership(false),
            KeyCode::Char('c') => {
                if !self.messages.can_post() { return true; }
                if self.messages.space_id.is_empty() {
                    self.messages.error = "Connect with g before composing".into();
                } else if self.messages.target["channel"] != "*"
                    && (self.messages.target.get("channel").is_some()
                        || self.messages.target.get("dm").is_some()
                        || self.messages.target.get("conversation").is_some())
                {
                    self.messages.mode = "compose".into();
                } else {
                    self.messages.error = "Choose a channel or DM before composing".into();
                }
            }
            KeyCode::Char('G') if !self.messages.management_view() => {
                self.messages.page_cursor = Value::Null;
                self.messaging_request("hub_refresh", self.messages.query());
            }
            KeyCode::Char('r') => {
                if let Some(v) = self.messages.items.get(self.messages.selected).cloned() {
                    self.messages.target = json!({"conversation":v["conversation_id"]});
                    if !self.messages.can_post() || !self.messages.can_edit() {
                        return true;
                    }
                    let mut d = self.messages.draft();
                    d.reply_to = v["id"].as_str().map(str::to_owned);
                    self.messages.filter = json!({});
                    self.messages.page_cursor = Value::Null;
                    self.messages.set_draft(d);
                    self.messages.mode = "compose".into();
                }
            }
            KeyCode::Char('t') => {
                if let Some(v) = self.messages.items.get(self.messages.selected).cloned() {
                    self.messages.target = json!({"conversation":v["conversation_id"]});
                    self.messages.filter = json!({"thread":v["data"]["thread_root"].as_str().unwrap_or(v["id"].as_str().unwrap_or(""))});
                    self.messaging_request("messaging.read", self.messages.query());
                }
            }
            KeyCode::Char('n') => self.messaging_channel_form(false),
            KeyCode::Char('S') => self.messaging_channel_form(true),
            KeyCode::Char('p') => self.messaging_pin_selected(),
            KeyCode::Char('P') => self.messaging_show_pins(),
            KeyCode::Char('/') => {
                let f = &self.messages.filter;
                let fields = vec![
                    f["time"]["since"].as_str().unwrap_or("").into(),
                    f["time"]["start"].as_str().unwrap_or("").into(),
                    f["time"]["end"].as_str().unwrap_or("").into(),
                    f["tags"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default(),
                    f["time_basis"].as_str().unwrap_or("created").into(),
                ];
                self.messages.start_form("filter", fields);
            }
            KeyCode::Char('e') => {
                let d = self.messages.draft();
                if self.messages.can_edit() {
                    let names = d
                        .mentions
                        .iter()
                        .map(|id| {
                            self.messages
                                .people
                                .iter()
                                .find(|p| p["id"] == *id)
                                .and_then(|p| p["name"].as_str())
                                .unwrap_or(id)
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    self.messages.start_form(
                        "metadata",
                        vec![
                            d.tags.join(", "),
                            names,
                            d.links
                                .iter()
                                .filter_map(|v| v["uri"].as_str())
                                .collect::<Vec<_>>()
                                .join(", "),
                        ],
                    );
                }
            }
            KeyCode::Char('N') => {
                self.messages.start_form("rename", vec![]);
            }
            KeyCode::Char('s') => {
                if let Some(uid) = self.active_session().map(|(_, ts)| ts.uid.clone()) {
                    if let Some(p) = self
                        .messages
                        .people
                        .iter()
                        .find(|p| p["session_uid"] == uid)
                    {
                        self.messages.target = json!({"dm":p["id"]});
                        self.messaging_request("messaging.read", self.messages.query());
                    }
                }
            }
            KeyCode::Char(']') => {
                if !self.messages.next.is_null() {
                    let mut p = self.messages.query();
                    p["cursor"] = self.messages.next.clone();
                    self.messages.page_cursor = self.messages.next.clone();
                    self.messages.append_older = true;
                    self.messaging_request("messaging.read", p);
                }
            }
            KeyCode::Char('g') => {
                self.messages.page_cursor = Value::Null;
                self.messages.loaded_target = Value::Null;
                self.messages.append_older = false;
                self.messages.select_older = false;
                self.messaging_request("bootstrap", json!({}));
            }
            KeyCode::Char('w') => {
                let d = self.messages.draft();
                let path = Messages::path()
                    .with_file_name(format!("message-draft-{}.md", uuid::Uuid::new_v4()));
                match std::fs::write(&path, d.body) {
                    Ok(()) => self.messages.status = format!("Draft saved: {}", path.display()),
                    Err(e) => self.messages.error = e.to_string(),
                }
            }
            _ => {}
        }
        true
    }
    fn messaging_send(&mut self) {
        if self.messages.busy {
            return;
        }
        if self.messages.draft().request_id.is_empty() && !self.messages.can_post() { return; }
        let mut d = self.messages.draft();
        if d.body.chars().count() > 3000 {
            self.messages.error =
                "Over 3000 characters. Save with Esc then w, and send a short file reference."
                    .into();
            return;
        }
        if d.request_id.is_empty() {
            d.request_id = uuid::Uuid::new_v4().to_string();
            d.origin = Some(self.messages.daemon_id.clone());
        }
        if !self.messages.set_draft(d.clone()) {
            return;
        }
        self.messages.error.clear();
        let mut p = self.messages.target.clone();
        p["body"] = json!(d.body);
        p["request_id"] = json!(d.request_id);
        p["origin_daemon_id"] = json!(d.origin);
        p["reply_to"] = json!(d.reply_to);
        p["tags"] = json!(d.tags);
        let (mentions, here) = d.mention_payload();
        p["mentions"] = json!(mentions);
        if here { p["mention_here"] = json!(true); }
        p["links"] = json!(d.links);
        self.messages.pending_send = Some((self.messages.key(), d.request_id.clone()));
        self.messaging_request("messaging.send", p);
    }
    fn messaging_submit_form(&mut self) {
        if matches!(self.messages.mode.as_str(), "channel" | "channel_edit") {
            self.messaging_save_channel();
            return;
        }
        let text = self.messages.text.clone();
        let fields = self.messages.fields.clone();
        let split = |s: &str| {
            s.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        match self.messages.mode.as_str() {
            "filter" => {
                if !fields[0].trim().is_empty()
                    && (!fields[1].trim().is_empty() || !fields[2].trim().is_empty())
                {
                    self.messages.error = "Choose a past duration OR an absolute range".into();
                    return;
                }
                let mut time = json!({});
                for (i, key) in ["since", "start", "end"].iter().enumerate() {
                    if !fields[i].trim().is_empty() {
                        time[*key] = json!(fields[i].trim());
                    }
                }
                self.messages.filter = json!({"time":if time.as_object().is_some_and(|v|v.is_empty()){Value::Null}else{time},"tags":split(&fields[3]),"time_basis":fields[4].trim()});
                self.messages.page_cursor = Value::Null;
                self.messaging_request("messaging.read", self.messages.query());
            }
            "metadata" => {
                if !self.messages.can_edit() {
                    return;
                }
                let mut d = self.messages.draft();
                d.tags = split(&fields[0]);
                d.mentions.clear();
                for name in split(&fields[1]) {
                    let hits: Vec<_> = self
                        .messages
                        .people
                        .iter()
                        .filter(|p| {
                            p["name"].as_str().is_some_and(|n| {
                                n.to_lowercase() == name.trim_start_matches('@').to_lowercase()
                            }) || p["id"] == name
                        })
                        .collect();
                    if hits.len() != 1 {
                        self.messages.error = format!(
                            "Choose an exact person name: {name}. Shift+Tab completes a prefix."
                        );
                        return;
                    }
                    d.mentions.push(hits[0]["id"].as_str().unwrap().into());
                }
                d.links = split(&fields[2])
                    .into_iter()
                    .map(|uri| json!({"uri":uri,"label":"Reference"}))
                    .collect();
                self.messages.set_draft(d);
            }
            "rename" => {
                let Some(uid) = self.active_session().map(|(_, ts)| ts.uid.clone()) else {
                    return;
                };
                let Some((rev, _)) = self.messages.saved.names.get(&uid).cloned() else {
                    self.messages.error = "Session must first choose its messaging name".into();
                    return;
                };
                self.messaging_request("session.set_name",json!({"uid":uid,"name":text,"expected_name_revision":rev,"request_id":uuid::Uuid::new_v4().to_string()}));
            }
            _ => {}
        }
        self.messages.mode.clear();
        self.messages.fields.clear();
    }
    /// Fetch the next older page (100 messages, no sidebar refresh).
    fn messaging_load_older(&mut self, select: bool) {
        if self.messages.next.is_null() {
            return;
        }
        if self.messages.busy {
            self.messages.want_older = true;
            return;
        }
        let mut p = self.messages.query();
        p["cursor"] = self.messages.next.clone();
        p["limit"] = json!(100);
        self.messages.page_cursor = self.messages.next.clone();
        self.messages.append_older = true;
        self.messages.select_older = select;
        self.messaging_request("messaging.read", p);
    }
    /// n / N: select the next (1) or previous (-1) message that mentions
    /// Owner; going back loads older history (up to 20 pages) as needed.
    fn messaging_seek_mention(&mut self, dir: i8) {
        let items = &self.messages.items;
        let at = self.messages.selected;
        let hit = if dir > 0 {
            items.iter().enumerate().skip(at + 1).find(|(_, m)| mentions_owner(m)).map(|(i, _)| i)
        } else {
            items[..at.min(items.len())].iter().rposition(mentions_owner)
        };
        if let Some(i) = hit {
            self.messages.selected = i;
            self.messages.reveal_selection = true;
            self.messages.seek_mention = (0, 0);
            self.messages.status.clear();
            return;
        }
        let (pending, pages) = self.messages.seek_mention;
        let pages = if pending == dir { pages } else { 20 };
        if dir < 0 && pages > 0 && !self.messages.next.is_null() {
            self.messages.seek_mention = (dir, pages - 1);
            self.messages.status = "Looking for an older mention…".into();
            self.messaging_load_older(false);
        } else {
            self.messages.seek_mention = (0, 0);
            self.messages.status = format!("No {} mention", if dir > 0 { "newer" } else { "older" });
        }
    }
    /// Enter on a mention: open its conversation loaded back to it.
    fn messaging_open_in_context(&mut self) {
        let Some(m) = self.messages.items.get(self.messages.selected).cloned() else {
            return;
        };
        self.messages.target = json!({"conversation": m["conversation_id"]});
        self.messages.filter = json!({});
        self.messages.page_cursor = Value::Null;
        self.messages.pane = 1;
        self.messages.body_scroll = 0;
        let mut p = self.messages.query();
        p["limit"] = json!(100);
        p["_jump"] = m["id"].clone();
        self.messaging_request("jump", p);
    }
    /// Fresh `messaging.counts` from a mark-read or react reply: update the
    /// sidebar rows and the status-bar indicator without another call.
    fn messaging_apply_counts(&mut self, counts: &Value) {
        let Some(per) = counts["conversations"].as_object() else { return };
        for row in self.messages.channels.iter_mut().chain(self.messages.dms.iter_mut()) {
            let Some(id) = row["id"].as_str() else { continue };
            let c = per.get(id);
            row["unread"] = json!(c.and_then(|c| c["unread"].as_u64()).unwrap_or(0));
            row["mentions"] = json!(c.and_then(|c| c["mentions"].as_u64()).unwrap_or(0));
        }
        if self.unread.accept_counts(counts) {
            self.needs_redraw = true;
        }
    }
    /// Toggle reaction `REACTIONS[i]` by Owner on the selected message.
    fn messaging_react(&mut self, i: usize) {
        let Some(m) = self.messages.items.get(self.messages.selected) else { return };
        let emoji = REACTIONS[i];
        let remove = reacted_by_me(m, emoji);
        let p = json!({
            "message_id": m["id"],
            "emoji": emoji,
            "remove": remove,
            "request_id": uuid::Uuid::new_v4().to_string(),
            "origin_daemon_id": self.messages.daemon_id,
        });
        self.messages.status = format!("{} {emoji}…", if remove { "Removing" } else { "Reacting" });
        self.messaging_request("messaging.react", p);
    }
    /// `M`: confirm, then mark the selected conversation (or the Inbox /
    /// mention list) read, mentions included.
    fn messaging_confirm_mark_read(&mut self) {
        let Some((name, target, mentions, unread)) = self.messages.mark_read_target() else {
            self.messages.error = "Choose a channel, DM, Inbox or Mentions to mark read".into();
            return;
        };
        let counts = match (unread, mentions) {
            (0, 0) if target["inbox"] == true || target["mentions"] == true => String::new(),
            (0, 0) => " (nothing unread)".into(),
            (u, 0) => format!(" ({u} unread)"),
            (u, m) => format!(" ({u} unread, {m} mention{})", if m == 1 { "" } else { "s" }),
        };
        self.messages.confirm = Some((format!("Mark {name} read{counts}? y confirms · any other key cancels"), target));
    }
    fn messaging_resize_sidebar(&mut self, delta: i32) {
        let width = self.messages.screen_width.max(80);
        let current = i32::from(self.messages_sidebar_width(width));
        let next = (current + delta).clamp(16, i32::from(width) * 3 / 5) as u16;
        match self.global_settings.set_messages_sidebar_width(next) {
            Ok(()) => self.messages.status = format!("Sidebar {next} columns · saved"),
            Err(e) => self.messages.error = format!("Sidebar width not saved: {e:#}"),
        }
    }
    /// Conversation sidebar width: the saved width, or one that fits the
    /// longest row (count column + name), capped at 40% of the screen.
    fn messages_sidebar_width(&self, total: u16) -> u16 {
        if total < 80 {
            return 16;
        }
        let saved = self.global_settings.messages_sidebar_width();
        if saved > 0 {
            return saved.clamp(16, total * 3 / 5);
        }
        let (cw, _) = self.messages.count_columns();
        let longest = self.messages.menu_items().iter().map(|(l, _)| l.chars().count()).max().unwrap_or(0);
        // "› " + counts + name + borders.
        ((2 + cw + longest + 2) as u16).clamp(24, (total * 2 / 5).max(24))
    }
    fn messaging_ack_selected(&mut self) {
        let Some(v) = self.messages.items.get(self.messages.selected) else {
            return;
        };
        let mut p = self.messages.query();
        if !self.messages.page_cursor.is_null() {
            p["cursor"] = self.messages.page_cursor.clone();
        }
        p["ack_receipt"] =
            json!({"actor":"owner","space_id":self.messages.space_id,"ids":[v["id"]]});
        self.messaging_request("messaging.read", p);
    }
    pub(super) fn messages_name_revision(&self, uid: &str) -> u64 {
        self.messages.saved.names.get(uid).map(|x| x.0).unwrap_or(0)
    }
    pub(super) fn rename_messaging_settings(
        &mut self,
        wi: usize,
        si: usize,
        name: &str,
    ) -> Result<bool, String> {
        let Some(ts) = self.workspaces.get(wi).and_then(|ws| ws.sessions.get(si)) else {
            return Ok(false);
        };
        let uid = ts.uid.clone();
        let host = ts.host_id.clone();
        let Some((current, _)) = self.messages.saved.names.get(&uid).cloned() else {
            return Ok(false);
        };
        let expected = self.messages.settings_name.as_ref().filter(|x| x.0 == uid);
        if expected.is_some_and(|x| x.1 == name) {
            return Ok(true);
        }
        let revision = expected.map(|x| x.2).unwrap_or(current);
        let socket = self
            .host_pool
            .live_socket_path(&host)
            .ok_or("Host unavailable; name unchanged")?;
        let v=crate::client_session::rpc_messaging(&socket,&self.host_pool.operator_token_for(&host),"session.set_name",json!({"uid":uid,"name":name,"expected_name_revision":revision,"request_id":uuid::Uuid::new_v4().to_string()})).map_err(|e|e.to_string())?;
        let event = &v["event"]["data"]["identity"];
        self.apply_messaging_name(
            &host,
            &uid,
            &json!({"name_revision":event["revision"],"label":event["name"]}),
        );
        Ok(true)
    }
    pub(super) fn messaging_manifest_names(&self) -> BTreeMap<String, cm_daemon::messaging::Name> {
        self.messages
            .saved
            .names
            .iter()
            .map(|(uid, (revision, name))| {
                (
                    uid.clone(),
                    cm_daemon::messaging::Name {
                        name: name.clone(),
                        revision: *revision,
                        revision_id: String::new(),
                        aliases: vec![],
                        session_uid: uid.clone(),
                        released: false,
                    },
                )
            })
            .collect()
    }
    pub(super) fn overlay_messaging_names(&self, manifest: &mut cm_daemon::manifest::Manifest) {
        for ws in manifest.workspaces.values_mut() {
            for entry in &mut ws.sessions {
                let saved = self.messages.saved.names.get(&entry.uid);
                let canonical = manifest.messaging_names.get(&entry.uid);
                if let Some(name) =
                    canonical.filter(|n| saved.is_none_or(|(rev, _)| n.revision >= *rev))
                {
                    entry.label = name.name.clone();
                } else if let Some((_, name)) = saved {
                    entry.label = name.clone();
                }
            }
        }
    }
    pub(crate) fn apply_messaging_name(
        &mut self,
        host: &cm_daemon::host_id::HostId,
        uid: &str,
        entry: &Value,
    ) {
        let (Some(rev), Some(name)) = (entry["name_revision"].as_u64(), entry["label"].as_str())
        else {
            return;
        };
        if self
            .messages
            .saved
            .names
            .get(uid)
            .is_some_and(|(old, _)| *old > rev)
        {
            return;
        }
        for ws in &mut self.workspaces {
            for ts in &mut ws.sessions {
                if ts.uid == uid && ts.host_id == *host {
                    ts.label = name.to_string();
                }
            }
        }
        self.messages
            .saved
            .names
            .insert(uid.into(), (rev, name.into()));
        self.messages.persist();
        self.save_session_manifest();
        self.needs_redraw = true;
    }
    pub(super) fn draw_messages(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let text_style = Style::default().fg(theme::CHAT_TEXT);
        let muted = Style::default().fg(theme::CHAT_MUTED);
        let accent = Style::default().fg(theme::CHAT_FOCUS);
        let editing = !self.messages.mode.is_empty();
        let conversations_focused = self.messages.pane == 0 && !editing;
        let messages_focused = self.messages.pane == 1 && !editing;
        frame.render_widget(Block::default().style(text_style.bg(theme::CHAT_BG)), area);
        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(4),
        ])
        .split(area);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("CM · Messages", accent.add_modifier(Modifier::BOLD)),
                Span::styled(" · Owner", Style::default().fg(theme::CHAT_OWNER)),
                Span::styled(
                    self.messages.sync_label(),
                    Style::default().fg(if self.messages.sync["connected"] == false {
                        theme::CHAT_TAG
                    } else {
                        theme::CHAT_MUTED
                    }),
                ),
                Span::styled("   j/k move · Tab pane · Alt+m / F8 / Esc return", muted),
            ])),
            rows[0],
        );
        self.messages.screen_width = area.width;
        let sidebar = self.messages_sidebar_width(area.width);
        let mut cols = Layout::horizontal([
            Constraint::Length(sidebar),
            Constraint::Min(10),
        ])
        .split(rows[1])
        .to_vec();
        let narrow = area.width < 80;
        if narrow {
            cols = vec![rows[1], rows[1]];
        }
        let menu = self.messages.menu_items();
        let (_, counts) = self.messages.count_columns();
        // "› " + borders; the count column is never cut, the name shortens
        // from the middle.
        let labels = menu
            .iter()
            .enumerate()
            .map(|(i, (label, target))| {
                let color = if target.get("tags").is_some() {
                    theme::CHAT_TAG
                } else if target.get("channel").is_some() {
                    theme::CHAT_FOCUS
                } else if target.get("dm").is_some() || target.get("conversation").is_some() || target["dm_section"] == true {
                    theme::CHAT_AGENT
                } else if target["norms"] == true {
                    theme::CHAT_MUTED
                } else {
                    theme::CHAT_OWNER
                };
                let selected = i == self.messages.menu;
                let style = Style::default().fg(color);
                let (mention, unread) = counts.get(i).cloned().unwrap_or_default();
                let used = mention.chars().count() + unread.chars().count();
                let name_room = (cols[0].width as usize).saturating_sub(4 + used);
                Line::from(vec![
                    Span::raw(if selected { "› " } else { "  " }),
                    Span::styled(mention, Style::default().fg(theme::CHAT_TAG).add_modifier(Modifier::BOLD)),
                    Span::styled(unread, Style::default().fg(theme::CHAT_OWNER)),
                    Span::raw(middle_truncate(label, name_room)),
                ])
                .style(if selected {
                    style.bg(theme::CHAT_SELECTION).add_modifier(Modifier::BOLD)
                } else {
                    style
                })
            })
            .collect::<Vec<_>>();
        if !narrow || self.messages.pane == 0 && self.messages.mode.is_empty() {
            frame.render_widget(
                Paragraph::new(labels)
                    .style(text_style.bg(theme::CHAT_PANEL))
                    .scroll((
                        self.messages
                            .menu
                            .saturating_sub(cols[0].height.saturating_sub(3) as usize)
                            as u16,
                        0,
                    ))
                    .block(chat_block("Conversations", conversations_focused)),
                cols[0],
            );
        }
        if !narrow || self.messages.pane == 1 || !self.messages.mode.is_empty() {
            if matches!(self.messages.mode.as_str(), "channel_browser" | "channel_roster" | "channel_add_member") {
                self.draw_channel_browser(frame, cols[1]);
            } else if self.messages.mode == "dm_picker" {
                self.draw_messaging_picker(frame, cols[1]);
            } else if matches!(self.messages.mode.as_str(), "channel" | "channel_edit") {
                self.draw_messaging_channel_form(frame, cols[1]);
            } else if self.messages.management_view() {
                self.draw_messaging_management(frame, cols[1], messages_focused);
            } else {
                let composer_height = if self.messages.mode.is_empty() {
                    if self.messages.draft().body.is_empty() {
                        0
                    } else {
                        3
                    }
                } else if matches!(
                    self.messages.mode.as_str(),
                    "filter" | "channel" | "channel_edit"
                ) {
                    10
                } else {
                    7
                };
                let mention_height = if self.messages.mention_options().is_empty() {
                    0
                } else {
                    (self.messages.mention_options().len().min(5) as u16 + 2)
                        .min(cols[1].height.saturating_sub(6))
                };
                let composer_height = if mention_height > 0 {
                    composer_height.min(cols[1].height.saturating_sub(mention_height + 3))
                } else {
                    composer_height
                };
                let content = Layout::vertical([
                    Constraint::Min(3),
                    Constraint::Length(composer_height),
                    Constraint::Length(mention_height),
                ])
                .split(cols[1]);
                self.draw_messaging_timeline(frame, content[0], messages_focused);
                self.draw_mention_options(frame, content[2]);
                let d = self.messages.draft();
                let text = if self.messages.mode.is_empty() || self.messages.mode == "compose" {
                    d.body
                } else if !self.messages.fields.is_empty() {
                    let labels = match self.messages.mode.as_str() {
                        "filter" => vec![
                            "Past (e.g. 10m)",
                            "From (date/time + zone)",
                            "Until (date/time + zone)",
                            "Tags, comma separated",
                            "Time basis (created / received)",
                        ],
                        "metadata" => vec![
                            "Tags, comma separated",
                            "Mention names (Shift+Tab completes)",
                            "Reference URIs, comma separated",
                        ],
                        "channel" => vec![
                            "Permanent path",
                            "Display name (optional)",
                            "Description",
                            "All agents may edit (yes/no)",
                            "Additional admin names or IDs",
                        ],
                        "channel_edit" => vec![
                            "Display name",
                            "Description",
                            "All agents may edit (yes/no)",
                            "Additional admin names or IDs",
                        ],
                        _ => vec!["Channel path", "Description"],
                    };
                    self.messages
                        .fields
                        .iter()
                        .enumerate()
                        .map(|(i, v)| {
                            format!(
                                "{} {}: {}",
                                if i == self.messages.field { "›" } else { " " },
                                labels[i],
                                v
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    self.messages.text.clone()
                };
                let title = if self.messages.mode.is_empty() || self.messages.mode == "compose" {
                    let (mentions, here) = self.messages.draft().mention_payload();
                    let audience = if here { " · @here".into() } else if !mentions.is_empty() {
                        format!(" · {} mention{}", mentions.len(), if mentions.len() == 1 { "" } else { "s" })
                    } else { String::new() };
                    format!(
                        "Owner → {}{audience} · {}/3000 · Ctrl+s sends",
                        self.messages.target_label(),
                        text.chars().count()
                    )
                } else {
                    format!(
                        "{} · Tab next field · Enter saves · Esc cancels",
                        self.messages.mode
                    )
                };
                if self.messages.mode == "compose" {
                    let inner = content[1].inner(ratatui::layout::Margin::new(1, 1));
                    let (lines, cursor_row, cursor_col) =
                        wrap_draft(&text, d.cursor, inner.width.max(1) as usize);
                    let scroll = cursor_row.saturating_sub(inner.height.saturating_sub(1) as usize);
                    frame.render_widget(
                        Paragraph::new(lines.join("\n"))
                            .style(text_style.bg(theme::CHAT_PANEL))
                            .scroll((scroll as u16, 0))
                            .block(chat_block(title, editing)),
                        content[1],
                    );
                    if inner.width > 0 && inner.height > 0 {
                        frame.set_cursor_position((
                            inner.x + cursor_col.min(inner.width as usize - 1) as u16,
                            inner.y + (cursor_row - scroll) as u16,
                        ));
                    }
                } else {
                    frame.render_widget(
                        Paragraph::new(text)
                            .style(text_style.bg(theme::CHAT_PANEL))
                            .wrap(Wrap { trim: false })
                            .block(chat_block(title, editing)),
                        content[1],
                    );
                }
            }
        }
        let status = if let Some((prompt, _)) = &self.messages.confirm {
            prompt.as_str()
        } else if self.messages.error.is_empty() {
            self.messages.status.as_str()
        } else {
            self.messages.error.as_str()
        };
        let status_style = Style::default().fg(if self.messages.confirm.is_some() {
            theme::CHAT_TAG
        } else if !self.messages.error.is_empty() {
            theme::ERROR
        } else if self.messages.busy {
            theme::CHAT_TAG
        } else {
            theme::CHAT_OWNER
        });
        if self.messages.management_view() {
            let mut help: Vec<_> =
                manage::wrap_readable(&self.messages.management_help(), rows[2].width as usize)
                    .into_iter()
                    .take(3)
                    .map(|s| Line::styled(s, muted))
                    .collect();
            help.push(Line::styled(
                if self.messages.saved.management.pending.is_some() {
                    format!("{status} · R retries pending operation")
                } else {
                    status.into()
                },
                status_style,
            ));
            frame.render_widget(Paragraph::new(help), rows[2]);
            return;
        }
        frame.render_widget(
            Paragraph::new(vec![
                chat_help(&[
                    ("c", "compose"),
                    ("@", "mention"),
                    ("d", "DM/group"),
                    ("b", "channels"),
                    ("J/L", "join/leave"),
                    ("u", "members"),
                    ("+", "react"),
                    ("M", "mark read"),
                ]),
                chat_help(&[
                    ("r/t", "reply/thread"),
                    ("n/S", "new/settings"),
                    ("p/P", "pin/pins"),
                    ("/", "filter"),
                    ("e", "metadata"),
                    ("</>", "sidebar"),
                ]),
                chat_help(&[
                    ("]", "older"),
                    ("g/G", "refresh/hub"),
                    ("N", "norms (sidebar)"),
                    ("n/N", "mention (chat)"),
                    ("W", "monitor"),
                    ("f", "preferences"),
                ]),
                Line::styled(
                    format!(
                        "{}{}",
                        if self.messages.busy {
                            "Working… "
                        } else {
                            ""
                        },
                        status
                    ),
                    status_style,
                ),
            ]),
            rows[2],
        );
    }
}

/// Status line for a conversation read. A replica reports history that is
/// still arriving from the hub as `cache.backfill`; show progress instead of
/// "not cached" so a fresh join reads as joined and loading.
fn read_status(count: usize, v: &Value) -> String {
    let backfill = &v["cache"]["backfill"];
    let fetching = backfill.is_object().then(|| {
        match (backfill["done"].as_u64(), backfill["total"].as_u64()) {
            (Some(done), Some(total)) => format!("fetching history {done}/{total}"),
            _ => "fetching history".to_owned(),
        }
    });
    let joined = v["target"]["joined"] == true;
    match (count, fetching) {
        (0, Some(fetching)) if joined => format!("Joined · {fetching}"),
        (0, Some(fetching)) => {
            let mut s = fetching;
            s[..1].make_ascii_uppercase();
            s
        }
        (n, Some(fetching)) => format!("{n} messages · {fetching}"),
        (0, None) if v["coverage"] == "partial" => "History not cached · G fetches from hub".into(),
        (0, None) => "No messages in cached history".into(),
        (n, None) => format!("{n} messages · {}", v["coverage"].as_str().unwrap_or("unknown")),
    }
}

fn chat_block(title: impl Into<String>, focused: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused {
            theme::CHAT_FOCUS
        } else {
            theme::CHAT_BORDER
        }))
        .title(Span::styled(
            title.into(),
            Style::default().fg(theme::CHAT_FOCUS),
        ))
}

fn chat_actor_style(message: &Value) -> Style {
    Style::default().fg(if message["actor"]["id"] == "owner" {
        theme::CHAT_OWNER
    } else if message["actor"]["kind"] == "system" {
        theme::CHAT_MUTED
    } else {
        theme::CHAT_AGENT
    })
}

fn chat_help(items: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, (key, label)) in items.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", Style::default().fg(theme::CHAT_MUTED)));
        }
        spans.push(Span::styled(
            key.to_string(),
            Style::default().fg(theme::CHAT_FOCUS),
        ));
        spans.push(Span::styled(
            format!(" {label}"),
            Style::default().fg(theme::CHAT_MUTED),
        ));
    }
    Line::from(spans)
}

fn wrap_draft(text: &str, cursor: usize, width: usize) -> (Vec<String>, usize, usize) {
    let mut lines = vec![String::new()];
    let mut col = 0;
    let mut position = (0, 0);
    for (index, c) in text.char_indices() {
        let size = Span::raw(c.to_string()).width();
        if c != '\n' && col + size > width {
            lines.push(String::new());
            col = 0;
        }
        if index == cursor {
            position = (lines.len() - 1, col);
        }
        if c == '\n' {
            lines.push(String::new());
            col = 0;
        } else {
            lines.last_mut().unwrap().push(c);
            col += size;
        }
    }
    if cursor >= text.len() {
        if col >= width {
            lines.push(String::new());
            col = 0;
        }
        position = (lines.len() - 1, col);
    }
    (lines, position.0, position.1)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn messaging_read_status_shows_backfill_progress_instead_of_uncached() {
        let fetching = json!({"coverage":"backfilling","target":{"joined":true},
            "cache":{"status":"backfilling","backfill":{"state":"fetching","done":1024,"total":6300}}});
        assert_eq!(read_status(0, &fetching), "Joined · fetching history 1024/6300");
        assert_eq!(read_status(7, &fetching), "7 messages · fetching history 1024/6300");
        let requested = json!({"coverage":"backfilling","cache":{"backfill":{"state":"requested"}}});
        assert_eq!(read_status(0, &requested), "Fetching history");
        let legacy = json!({"coverage":"partial","cache":{"status":"partial"}});
        assert_eq!(read_status(0, &legacy), "History not cached · G fetches from hub");
        assert_eq!(read_status(3, &json!({"coverage":"complete"})), "3 messages · complete");
        let mut m = Messages::default();
        assert_eq!(m.refresh_interval(), Duration::from_secs(3));
        m.cache = fetching["cache"].clone();
        assert_eq!(m.refresh_interval(), Duration::from_secs(1));
    }
    fn owner_app() -> App {
        let mut app = App::new(crate::config::Config {
            api_url: String::new(),
            api_token: String::new(),
            gcp_project: String::new(),
            gcp_zone: String::new(),
            repos: HashMap::new(),
        });
        app.messages.visible = true;
        app.messages.space_id = "space".into();
        app.messages.people = vec![json!({"id":"a","name":"Alpha"}), json!({"id":"owner","name":"Owner"})];
        app.messages.channels = vec![
            json!({"id":"c1","path":"behavior-triage","name":"Behavior Triage Orchestrator Reviews","unread":5,"mentions":2}),
            json!({"id":"c2","path":"general","name":"general","unread":0,"mentions":0}),
        ];
        app.messages.dms = vec![json!({"id":"d1","peer":"a","peers":["a"],"unread":3})];
        app
    }

    fn screen(app: &mut App, w: u16, h: u16) -> String {
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| app.draw_messages(f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn press(app: &mut App, code: KeyCode) {
        app.messaging_event(&CrosstermEvent::Key(crossterm::event::KeyEvent::new(code, KeyModifiers::NONE)));
    }

    fn chat(n: usize, mention: bool, body: &str) -> Value {
        json!({"id":format!("m{n}"),"conversation_id":"c1","logical_time":format!("{n}"),
            "actor":{"id":"a","name":"Alpha"},"body":body,"created_at":"2026-10-07T22:00:00Z",
            "read":true,"data":{"tags":[],"links":[],"mention_recipients": if mention { json!(["owner"]) } else { json!([]) }}})
    }

    #[test]
    fn messages_sidebar_counts_lead_names_truncate_in_the_middle_and_resize_persists() {
        let _lock = crate::test_support::home_lock();
        let _home = Home::new();
        let mut app = owner_app();
        for width in [100u16, 160] {
            let text = screen(&mut app, width, 30);
            if std::env::var_os("CM_MSG_DUMP").is_some() {
                println!("--- {width} columns ---\n{text}");
            }
            let row = text.lines().find(|l| l.contains("#Behavior")).unwrap_or_else(|| panic!("{text}"));
            assert!(row.contains("@2 \u{25cf}5 #Behavior"), "counts before the name: {row}");
            // Character columns within the sidebar (the timeline title also says #general).
            let col = |line: &str, needle: &str| {
                let chars: Vec<char> = line.chars().collect();
                let n: Vec<char> = needle.chars().collect();
                chars.windows(n.len()).position(|w| w == n.as_slice())
            };
            let general = text.lines().find(|l| col(l, "#general").is_some_and(|c| c < 40)).unwrap();
            assert_eq!(col(general, "#general"), col(row, "#Behavior"), "names align after the count column");
            let dm = text.lines().find(|l| l.contains("Alpha") && col(l, "Alpha").is_some_and(|c| c < 40)).unwrap();
            assert_eq!(col(dm, "\u{25cf}3"), col(row, "\u{25cf}5"), "DM unread in the same column: {dm}");
            if width >= 160 {
                assert!(row.contains("#Behavior Triage Orchestrator Reviews"), "wide: the whole name fits: {row}");
            } else {
                assert!(row.contains("#Behavior Tri") && row.contains("\u{2026}") && row.contains("Reviews"), "middle truncation: {row}");
            }
        }
        // > / < resize the sidebar and save the width.
        let before = app.messages_sidebar_width(160);
        press(&mut app, KeyCode::Char('>'));
        assert_eq!(app.messages_sidebar_width(160), before + 2);
        let saved = std::fs::read_to_string(dirs::home_dir().unwrap().join(".cm/tui-settings.toml")).unwrap();
        assert!(saved.contains(&format!("messages_sidebar_width = {}", before + 2)), "{saved}");
        press(&mut app, KeyCode::Char('<'));
        press(&mut app, KeyCode::Char('<'));
        assert_eq!(app.messages_sidebar_width(160), before - 2);
        assert_eq!(middle_truncate("abcdefghij", 5), "ab\u{2026}ij");
        assert_eq!(count_label('@', 140), "@99+");
    }

    #[test]
    fn mark_read_confirms_with_counts_and_any_other_key_cancels() {
        let _lock = crate::test_support::home_lock();
        let _home = Home::new();
        let mut app = owner_app();
        app.messages.target = json!({"channel":"behavior-triage"});
        app.messages.pane = 1;
        press(&mut app, KeyCode::Char('M'));
        let prompt = app.messages.confirm.as_ref().map(|c| c.0.clone()).unwrap();
        assert_eq!(prompt, "Mark #Behavior Triage Orchestrator Reviews read (5 unread, 2 mentions)? y confirms \u{00b7} any other key cancels");
        assert!(screen(&mut app, 160, 30).contains("(5 unread, 2 mentions)? y confirms"));
        press(&mut app, KeyCode::Char('x'));
        assert!(app.messages.confirm.is_none());
        assert_eq!(app.messages.status, "Not marked");
        // From the sidebar, the Inbox row marks everything in it.
        app.messages.pane = 0;
        app.messages.menu = 0;
        press(&mut app, KeyCode::Char('M'));
        assert_eq!(app.messages.confirm.as_ref().unwrap().1, json!({"inbox":true}));
        press(&mut app, KeyCode::Char('y'));
        assert!(app.messages.confirm.is_none());
        assert!(app.messages.busy || app.messages.error.contains("unavailable"), "y sends mark_read");
    }

    #[test]
    fn mentions_list_is_newest_first_opens_in_context_and_n_steps_between_mentions() {
        let _lock = crate::test_support::home_lock();
        let _home = Home::new();
        let mut app = owner_app();
        // The Mentions entry sits under Inbox.
        let items = app.messages.menu_items();
        assert_eq!(items[1].1, json!({"mentions":true}));
        app.messages.target = json!({"mentions":true});
        app.messages.pane = 1;
        app.messages.accept_messages(&json!({"items":[chat(9, true, "newest ping"), chat(3, true, "older ping")]}));
        assert_eq!(app.messages.items[0]["id"], "m9", "newest first");
        assert_eq!(app.messages.selected, 0);
        let text = screen(&mut app, 100, 30);
        assert!(text.contains("Mentions") && text.contains("#behavior-triage") && text.contains("newest ping"), "{text}");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.messages.target, json!({"conversation":"c1"}), "Enter opens the conversation");
        app.messages.busy = false; // No daemon in tests: drop the jump request.
        app.messages.rx = None;
        // In a conversation: n / N walk the mentions of Owner.
        app.messages.target = json!({"channel":"behavior-triage"});
        app.messages.items = (0..10).map(|i| chat(i, i == 2 || i == 7, "hello")).collect();
        app.messages.selected = 9;
        press(&mut app, KeyCode::Char('N'));
        assert_eq!(app.messages.selected, 7);
        press(&mut app, KeyCode::Char('N'));
        assert_eq!(app.messages.selected, 2);
        press(&mut app, KeyCode::Char('N'));
        assert_eq!(app.messages.selected, 2);
        assert_eq!(app.messages.status, "No older mention");
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(app.messages.selected, 7);
    }

    #[test]
    fn scrolling_a_5k_message_channel_reuses_layout_and_stays_fast() {
        let _lock = crate::test_support::home_lock();
        let _home = Home::new();
        let mut app = owner_app();
        app.messages.target = json!({"channel":"behavior-triage"});
        app.messages.pane = 1;
        app.messages.items = (0..5000)
            .map(|i| chat(i, i % 50 == 0, &"long message body with several words to wrap ".repeat(1 + i % 7)))
            .collect();
        app.messages.loaded_target = app.messages.query();
        app.messages.selected = 4999;
        app.messages.reveal_selection = true;
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 50)).unwrap();
        terminal.draw(|f| app.draw_messages(f)).unwrap();
        let warm = app.messages.layout_builds;
        assert!(warm >= 5000, "first frame lays out every message once ({warm})");
        let bottom = app.messages.timeline_scroll;
        let started = Instant::now();
        let frames = 60;
        for _ in 0..frames {
            press(&mut app, KeyCode::PageUp);
            terminal.draw(|f| app.draw_messages(f)).unwrap();
        }
        let elapsed = started.elapsed();
        println!("5k messages: {frames} PageUp frames in {elapsed:?} ({:?}/frame)", elapsed / frames);
        assert!(app.messages.timeline_scroll < bottom, "the view moved up");
        assert!(app.messages.layout_builds - warm <= frames as u64, "at most the selected message is rebuilt per frame");
        // Debug build on a loaded host: generous, but far below a re-wrap per frame.
        assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
        // A width change re-lays out once, then caches again.
        let mut narrow = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 50)).unwrap();
        narrow.draw(|f| app.draw_messages(f)).unwrap();
        let after_resize = app.messages.layout_builds;
        narrow.draw(|f| app.draw_messages(f)).unwrap();
        assert_eq!(app.messages.layout_builds - after_resize, 1);
    }

    #[test]
    fn reactions_render_toggle_owner_and_counts_replies_update_sidebar_and_indicator() {
        let _lock = crate::test_support::home_lock();
        let _home = Home::new();
        let mut app = owner_app();
        app.messages.target = json!({"channel":"behavior-triage"});
        app.messages.pane = 1;
        let mut m = chat(1, false, "deploy done");
        m["reactions"] = json!({"\u{2705}":{"count":2,"names":["Alpha","Owner"],"mine":true},"\u{1f440}":["Beta"]});
        app.messages.items = vec![m];
        let text = screen(&mut app, 160, 30);
        if std::env::var_os("CM_MSG_DUMP").is_some() {
            println!("--- reactions ---\n{text}");
        }
        // (A wide emoji's second cell reads as a blank in the buffer dump.)
        let line = text.lines().find(|l| l.contains("Alpha, Owner")).unwrap_or_else(|| panic!("{text}"));
        assert!(line.contains('\u{2705}') && line.contains('\u{1f440}') && line.contains("Beta"), "{line}");
        assert!(reacted_by_me(&app.messages.items[0], "\u{2705}"));
        assert!(!reacted_by_me(&app.messages.items[0], "\u{1f440}"));
        // + then 1 toggles Owner's ✅ off (Owner already reacted).
        press(&mut app, KeyCode::Char('+'));
        assert!(app.messages.status.starts_with("React: 1 \u{2705}"));
        press(&mut app, KeyCode::Char('1'));
        let r = &app.messages.management.request;
        assert!(
            (r["emoji"] == "\u{2705}" && r["remove"] == true && r["request_id"].is_string())
                || app.messages.error.contains("unavailable"),
            "{r}"
        );
        app.messages.busy = false;
        app.messages.rx = None;
        // The reply replaces the aggregate and carries the new counts.
        app.messages.accept_reaction(&json!({"message_id":"m1","reactions":{"\u{2705}":{"count":1,"names":["Alpha"],"mine":false}}}));
        assert!(!reacted_by_me(&app.messages.items[0], "\u{2705}"));
        let text = screen(&mut app, 160, 30);
        assert!(text.lines().any(|l| l.contains('\u{2705}') && l.contains("Alpha ") && !l.contains("Owner")), "{text}");
        app.messaging_apply_counts(&json!({"conversations":{"c1":{"unread":0,"mentions":0}},"dms":1,"mentions":0,"unread":1}));
        assert_eq!(app.messages.menu_counts(&json!({"channel":"behavior-triage"})), (0, 0), "M clears the row at once");
        assert_eq!(app.messages.menu_counts(&json!({"conversation":"d1"})), (0, 0), "absent from counts = read");
        assert_eq!((app.unread.dms, app.unread.mentions), (1, 0));
        // Any other key after + cancels.
        press(&mut app, KeyCode::Char('+'));
        press(&mut app, KeyCode::Char('x'));
        assert_eq!(app.messages.status, "No reaction");
    }

    #[test]
    fn refreshes_mark_read_only_at_the_newest_message() {
        let _lock = crate::test_support::home_lock();
        let _home = Home::new();
        let mut app = owner_app();
        app.messages.target = json!({"channel":"behavior-triage"});
        app.messages.items = (0..3).map(|i| chat(i, false, "x")).collect();
        app.messages.loaded_target = app.messages.query();
        app.messages.selected = 0;
        app.messaging_refresh_target();
        assert_eq!(app.messages.management.request["mark_read"], false, "scrolled back: a refresh does not mark read");
        app.messages.busy = false;
        app.messages.rx = None;
        app.messages.selected = 2;
        app.messaging_refresh_target();
        assert_eq!(app.messages.management.request["mark_read"], true);
        // Opening a conversation leaves the daemon default (reading marks read).
        app.messages.busy = false;
        app.messages.rx = None;
        app.messages.loaded_target = Value::Null;
        app.messaging_refresh_target();
        assert!(app.messages.management.request.get("mark_read").is_none());
    }

    pub(super) struct Home {
        old: Option<std::ffi::OsString>,
        _temp: tempfile::TempDir,
        sockets: Vec<(String, Option<std::ffi::OsString>)>,
    }
    impl Home {
        pub(super) fn new() -> Self {
            let t = tempfile::tempdir().unwrap();
            let old = std::env::var_os("HOME");
            let sockets = ["CM_DAEMON_SOCKET", "CM_TUI_SOCKET"]
                .into_iter()
                .map(|key| (key.to_string(), std::env::var_os(key)))
                .collect();
            unsafe {
                std::env::set_var("HOME", t.path());
                std::env::set_var("CM_DAEMON_SOCKET", t.path().join("missing-daemon.sock"));
                std::env::set_var("CM_TUI_SOCKET", t.path().join("tui.sock"));
            }
            Self {
                old,
                _temp: t,
                sockets,
            }
        }
    }
    impl Drop for Home {
        fn drop(&mut self) {
            unsafe {
                for (key, value) in &self.sockets {
                    if let Some(value) = value {
                        std::env::set_var(key, value);
                    } else {
                        std::env::remove_var(key);
                    }
                }
                if let Some(old) = &self.old {
                    std::env::set_var("HOME", old);
                } else {
                    std::env::remove_var("HOME");
                }
            }
        }
    }
    #[test]
    fn messaging_draft_survives_restart_and_uncertain_send_cannot_change_intent() {
        let _lock = crate::test_support::home_lock();
        let _home = Home::new();
        let mut m = Messages::load();
        m.space_id = "space".into();
        m.mode = "compose".into();
        m.edit_text("A short reply. 👋", false);
        m.move_cursor(KeyCode::Left);
        m.edit_text("!", false);
        m.move_cursor(KeyCode::Left);
        m.move_cursor(KeyCode::Delete);
        m.move_cursor(KeyCode::End);
        let mut d = m.draft();
        d.request_id = "original".into();
        d.origin = Some("original-host".into());
        m.set_draft(d);
        m.edit_text("Changed", false);
        assert_eq!(m.draft().body, "A short reply. 👋");
        assert!(m.error.contains("pending"));
        let mut restored = Messages::load();
        restored.space_id = "space".into();
        assert_eq!(restored.draft().request_id, "original");
        assert_eq!(restored.draft().origin.as_deref(), Some("original-host"));
        restored.target = json!({"dm":"other"});
        assert!(restored.draft().body.is_empty());
    }
    #[test]
    fn messaging_owner_view_renders_narrow_and_wide_and_preserves_pending_draft() {
        let _lock = crate::test_support::home_lock();
        let _home = Home::new();
        let mut app = App::new(crate::config::Config {
            api_url: String::new(),
            api_token: String::new(),
            gcp_project: String::new(),
            gcp_zone: String::new(),
            repos: HashMap::new(),
        });
        app.messages.visible = true;
        app.messages.space_id = "space".into();
        app.messages.mode = "compose".into();
        app.messages.pane = 1;
        app.messaging_event(&CrosstermEvent::Paste("Quick pasted reply.".into()));
        assert_eq!(app.messages.draft().body, "Quick pasted reply.");
        app.messages.items = vec![
            json!({"id":"message","actor":{"name":"Parser Scout"},"body":"Ready to review.","created_at":"2026-09-06T22:00:00Z","data":{"tags":["needs-owner"],"links":[]}}),
        ];
        for (w, h) in [(120, 32), (80, 24), (45, 20)] {
            let backend = ratatui::backend::TestBackend::new(w, h);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal.draw(|f| app.draw_messages(f)).unwrap();
            let rendered = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect::<String>();
            assert!(rendered.contains("Owner"));
            assert!(rendered.contains("Quick pasted reply."));
        }
        app.messages.mode.clear();
        app.messaging_event(&CrosstermEvent::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('/'),
            KeyModifiers::NONE,
        )));
        assert_eq!(app.messages.fields.len(), 5);
        app.messages.fields = vec![
            "10m".into(),
            "".into(),
            "".into(),
            "needs-owner".into(),
            "received".into(),
        ];
        app.messaging_submit_form();
        assert_eq!(app.messages.filter["time"]["since"], "10m");
    }
    #[test]
    fn messaging_settings_and_reconnect_preserve_newer_names_and_grouping() {
        let _lock = crate::test_support::home_lock();
        let _home = Home::new();
        let mut app = App::new(crate::config::Config {
            api_url: String::new(),
            api_token: String::new(),
            gcp_project: String::new(),
            gcp_zone: String::new(),
            repos: HashMap::new(),
        });
        let session =
            crate::session::Session::new("/bin/true", &[], 80, 24, None, HashMap::new(), None)
                .unwrap();
        let mut ts = make_simple_session_with_uid("a".into(), "original", "claude", session, None);
        ts.workflow_run_id = Some("run".into());
        ts.workflow_role = Some("worker".into());
        ts.continuous_task_id = Some("continuous".into());
        ts.task_id = Some("task".into());
        app.sessions_restored = true;
        app.workspaces.push(Workspace {
            color: None,
            pinned: false,
            id: "w".into(),
            name: "workspace".into(),
            is_closed: false,
            is_cloud: false,
            repo_url: None,
            worktree_path: None,
            main_repo_path: None,
            worker_vm: None,
            worker_zone: None,
            host_id: cm_daemon::host_id::HostId::local(),
            sessions: vec![ts],
            tombstones: vec![],
        });
        let host = cm_daemon::host_id::HostId::local();
        app.apply_messaging_name(&host, "a", &json!({"name_revision":1,"label":"Scout"}));
        app.messages.settings_name = Some(("a".into(), "Scout".into(), 1));
        app.apply_messaging_name(&host, "a", &json!({"name_revision":2,"label":"Gardener"}));
        assert!(app.rename_messaging_settings(0, 0, "Scout").unwrap());
        assert_eq!(app.workspaces[0].sessions[0].label, "Gardener");
        app.apply_messaging_name(&host, "a", &json!({"name_revision":1,"label":"Stale"}));
        assert_eq!(app.workspaces[0].sessions[0].label, "Gardener");
        app.workspaces[0].sessions[0].label = "revisionless reconnect".into();
        app.apply_messaging_name(&host, "a", &json!({"name_revision":2,"label":"Gardener"}));
        let ts = &app.workspaces[0].sessions[0];
        assert_eq!(ts.label, "Gardener");
        assert_eq!(ts.workflow_role.as_deref(), Some("worker"));
        assert_eq!(ts.continuous_task_id.as_deref(), Some("continuous"));
        assert_eq!(ts.task_id.as_deref(), Some("task"));
        let mut manifest = App::load_manifest();
        app.overlay_messaging_names(&mut manifest);
        assert_eq!(manifest.workspaces["w"].sessions[0].label, "Gardener");
    }
}
