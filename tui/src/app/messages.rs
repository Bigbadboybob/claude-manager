//! Owner messaging view. Network work runs off the terminal event loop.
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
    links: Vec<Value>,
    origin: Option<String>,
}
#[derive(Default, Serialize, Deserialize)]
struct Saved {
    drafts: BTreeMap<String, Draft>,
    names: BTreeMap<String, (u64, String)>,
}
#[derive(Default)]
pub struct Messages {
    pub visible: bool,
    saved: Saved,
    target: Value,
    channels: Vec<Value>,
    people: Vec<Value>,
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
    norms: String,
    filter: Value,
    next: Value,
    rx: Option<Receiver<(String, Result<Value, String>)>>,
    busy: bool,
    last_refresh: Option<Instant>,
    daemon_id: String,
    space_id: String,
    body_scroll: u16,
    receipt: Value,
    pub settings_name: Option<(String, String, u64)>,
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
        self.saved
            .drafts
            .get(&self.key())
            .cloned()
            .unwrap_or_default()
    }
    fn set_draft(&mut self, draft: Draft) -> bool {
        self.saved.drafts.insert(self.key(), draft);
        self.persist()
    }
    fn menu_items(&self) -> Vec<(String, Value)> {
        let mut out = vec![
            ("Inbox".into(), json!({"inbox":true})),
            ("New DMs".into(), json!({"dms":true,"unread_only":true})),
            (
                "Needs Owner".into(),
                json!({"channel":"*","tags":["needs-owner"]}),
            ),
            ("Shared norms".into(), json!({"norms":true})),
        ];
        out.extend(self.channels.iter().map(|c| {
            (
                format!("#{}", c["path"].as_str().unwrap_or("?")),
                json!({"channel":c["path"]}),
            )
        }));
        out.extend(self.people.iter().filter(|p| p["id"] != "owner").map(|p| {
            (
                format!(
                    "DM {}{}",
                    p["name"].as_str().unwrap_or("?"),
                    if p["present"] == true {
                        ""
                    } else {
                        " (offline)"
                    }
                ),
                json!({"dm":p["id"]}),
            )
        }));
        out
    }
    fn target_label(&self) -> String {
        if self.target["norms"] == true {
            return "Shared norms".into();
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
                format!("#{c}")
            };
        }
        let peer = self.target["dm"].as_str().map(str::to_owned).or_else(|| {
            self.dms
                .iter()
                .find(|d| d["id"] == self.target["conversation"])
                .and_then(|d| d["peer"].as_str())
                .map(str::to_owned)
        });
        if let Some(id) = peer {
            return format!(
                "DM {}",
                self.people
                    .iter()
                    .find(|p| p["id"] == id)
                    .and_then(|p| p["name"].as_str())
                    .unwrap_or(&id)
            );
        }
        if let Some(c) = self
            .channels
            .iter()
            .find(|c| c["id"] == self.target["conversation"])
        {
            return format!("#{}", c["path"].as_str().unwrap_or("?"));
        }
        "Conversation".into()
    }
    fn can_edit(&mut self) -> bool {
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
                    d.body.replace_range(at..at + c.len_utf8(), "");
                }
                at
            }
            _ => at,
        };
        self.set_draft(d);
    }
    fn edit_text(&mut self, text: &str, backspace: bool) {
        if self.mode == "compose" {
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
                d.body.replace_range(previous..at, "");
                d.cursor = previous;
            } else {
                d.body.insert_str(at, text);
                d.cursor = at + text.len();
            }
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
    fn query(&self) -> Value {
        let mut p = self.target.clone();
        if let Some(f) = self.filter.as_object() {
            for (k, v) in f {
                p[k] = v.clone();
            }
        }
        p["limit"] = json!(20);
        p["newest_first"] = json!(true);
        p
    }
}
impl App {
    pub fn messaging_tick(&mut self) {
        let result = self.messages.rx.as_ref().and_then(|rx| rx.try_recv().ok());
        if let Some((method, result)) = result {
            self.messages.busy = false;
            self.messages.rx = None;
            self.needs_redraw = true;
            match result {
                Err(e) => {
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
                    self.messages.error = e;
                }
                Ok(v) => {
                    self.messages.error.clear();
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
                            self.messages.norms =
                                v["open"]["norms"]["text"].as_str().unwrap_or("").into();
                            self.messages.dms = v["open"]["dms"]["items"]
                                .as_array()
                                .cloned()
                                .unwrap_or_default();
                            self.messaging_request("messaging.read", self.messages.query());
                        }
                        "messaging.read" => {
                            let old = self
                                .messages
                                .items
                                .get(self.messages.selected)
                                .map(|v| v["id"].clone());
                            self.messages.items =
                                v["items"].as_array().cloned().unwrap_or_default();
                            self.messages.selected = old
                                .and_then(|id| {
                                    self.messages.items.iter().position(|v| v["id"] == id)
                                })
                                .unwrap_or(0);
                            self.messages.next = v["next_cursor"].clone();
                            self.messages.receipt = v["receipt"].clone();
                            if let Some(reason) = v["degraded"].as_str() {
                                self.messages.error = format!("Storage is read only: {reason}");
                            }
                            self.messages.status = if self.messages.items.is_empty() {
                                "No messages".into()
                            } else {
                                format!(
                                    "{} messages · {}",
                                    self.messages.items.len(),
                                    v["coverage"].as_str().unwrap_or("unknown")
                                )
                            };
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
                            self.messages.status = "Sent · stored locally".into();
                            self.messaging_request("messaging.read", self.messages.query());
                        }
                        "messaging.channels" => self.messaging_request("bootstrap", json!({})),
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
            && self.messages.mode.is_empty()
            && self.messages.page_cursor.is_null()
            && self
                .messages
                .last_refresh
                .is_none_or(|t| t.elapsed() > Duration::from_secs(3))
            && self.messages.target["norms"] != true
        {
            self.messaging_request("messaging.read", self.messages.query());
        }
    }
    fn messaging_request(&mut self, method: &str, params: Value) {
        if self.messages.busy {
            return;
        }
        let host = cm_daemon::host_id::HostId::local();
        let Some(socket) = self.host_pool.live_socket_path(&host) else {
            self.messages.error = "Local daemon is unavailable".into();
            return;
        };
        let token = self.host_pool.operator_token_for(&host);
        let method = method.to_owned();
        let (tx, rx) = mpsc::channel();
        self.messages.rx = Some(rx);
        self.messages.busy = true;
        self.messages.last_refresh = Some(Instant::now());
        std::thread::spawn(move || {
            let call = |m: &str, p: Value| {
                crate::client_session::rpc_messaging(&socket, &token, m, p)
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
                        json!({"channels":directory("messaging.channels",json!({"action":"list"}))?,"people":directory("messaging.people",json!({"include_exited":true}))?,"open":call("messaging.open",json!({"channel":"general"}))?}),
                    )
                })()
            } else {
                call(&method, params)
            };
            let _ = tx.send((method, result));
        });
    }
    pub(super) fn messaging_event(&mut self, event: &CrosstermEvent) -> bool {
        if let CrosstermEvent::Paste(text) = event {
            if self.messages.visible && !self.messages.busy && !self.messages.mode.is_empty() {
                self.messages.edit_text(&text.replace("\r\n", "\n"), false);
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
                self.messaging_request("bootstrap", json!({}));
            }
            return true;
        }
        if !self.messages.visible {
            return false;
        }
        if self.messages.busy {
            if key.code == KeyCode::Esc {
                self.messages.visible = false;
            }
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
                    self.messages.body_scroll = 0;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.messages.pane == 0 {
                    self.messages.menu = self.messages.menu.saturating_sub(1);
                } else if self.messages.target["norms"] == true {
                    self.messages.body_scroll = self.messages.body_scroll.saturating_sub(1);
                } else {
                    self.messages.selected = self.messages.selected.saturating_sub(1);
                    self.messages.body_scroll = 0;
                }
            }
            KeyCode::PageDown => {
                self.messages.body_scroll = self.messages.body_scroll.saturating_add(10)
            }
            KeyCode::PageUp => {
                self.messages.body_scroll = self.messages.body_scroll.saturating_sub(10)
            }
            KeyCode::Enter => {
                if self.messages.pane == 0 {
                    if let Some((_, target)) =
                        self.messages.menu_items().get(self.messages.menu).cloned()
                    {
                        self.messages.target = target;
                        self.messages.filter = json!({});
                        self.messages.page_cursor = Value::Null;
                        self.messages.pane = 1;
                        self.messages.selected = 0;
                        if self.messages.target["norms"] != true {
                            self.messaging_request("messaging.read", self.messages.query());
                        }
                    }
                } else {
                    self.messaging_ack_selected();
                }
            }
            KeyCode::Char('c') => {
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
            KeyCode::Char('r') => {
                if let Some(v) = self.messages.items.get(self.messages.selected).cloned() {
                    self.messages.target = json!({"conversation":v["conversation_id"]});
                    let mut d = self.messages.draft();
                    d.reply_to = v["id"].as_str().map(str::to_owned);
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
            KeyCode::Char('n') => {
                self.messages
                    .start_form("channel", vec![String::new(), String::new()]);
            }
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
                    self.messaging_request("messaging.read", p);
                }
            }
            KeyCode::Char('g') => {
                self.messages.page_cursor = Value::Null;
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
        p["mentions"] = json!(d.mentions);
        p["links"] = json!(d.links);
        self.messages.pending_send = Some((self.messages.key(), d.request_id.clone()));
        self.messaging_request("messaging.send", p);
    }
    fn messaging_submit_form(&mut self) {
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
            "channel" => {
                self.messaging_request("messaging.channels",json!({"action":"create","path":fields[0].trim(),"description":fields[1],"request_id":uuid::Uuid::new_v4().to_string()}));
            }
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
        frame.render_widget(
            Block::default().style(text_style.bg(theme::CHAT_BG)),
            area,
        );
        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .split(area);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("CM · Messages", accent.add_modifier(Modifier::BOLD)),
                Span::styled(" · Owner", Style::default().fg(theme::CHAT_OWNER)),
                Span::styled("   j/k move · Tab pane · Alt+m / F8 / Esc return", muted),
            ])),
            rows[0],
        );
        let mut cols = Layout::horizontal([
            Constraint::Length(if area.width >= 80 { 24 } else { 16 }),
            Constraint::Min(10),
        ])
        .split(rows[1])
        .to_vec();
        let narrow = area.width < 80;
        if narrow {
            cols = vec![rows[1], rows[1]];
        }
        let menu = self.messages.menu_items();
        let labels = menu
            .iter()
            .enumerate()
            .map(|(i, (label, target))| {
                let color = if target.get("tags").is_some() {
                    theme::CHAT_TAG
                } else if target.get("channel").is_some() {
                    theme::CHAT_FOCUS
                } else if target.get("dm").is_some() {
                    theme::CHAT_AGENT
                } else if target["norms"] == true {
                    theme::CHAT_MUTED
                } else {
                    theme::CHAT_OWNER
                };
                let selected = i == self.messages.menu;
                let style = Style::default().fg(color);
                Line::from(format!("{} {}", if selected { "›" } else { " " }, label))
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
            if self.messages.target["norms"] == true {
                frame.render_widget(
                    Paragraph::new(self.messages.norms.as_str())
                        .style(text_style.bg(theme::CHAT_PANEL))
                        .wrap(Wrap { trim: false })
                        .scroll((self.messages.body_scroll, 0))
                        .block(chat_block("Shared norms", messages_focused)),
                    cols[1],
                );
            } else {
                let content = Layout::vertical([
                    Constraint::Length(if self.messages.mode.is_empty() { 6 } else { 3 }),
                    Constraint::Min(3),
                    Constraint::Length(if self.messages.mode.is_empty() {
                        3
                    } else if self.messages.mode == "filter" {
                        9
                    } else {
                        7
                    }),
                ])
                .split(cols[1]);
                let lines = self
                    .messages
                    .items
                    .iter()
                    .enumerate()
                    .map(|(i, v)| {
                        let selected = i == self.messages.selected;
                        Line::from(vec![
                            Span::styled(if selected { "› " } else { "  " }, accent),
                            Span::styled(
                                v["actor"]["name"].as_str().unwrap_or("?"),
                                chat_actor_style(v).add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(": ", muted),
                            Span::styled(
                                v["body"].as_str().unwrap_or("").lines().next().unwrap_or(""),
                                text_style,
                            ),
                        ])
                        .style(if selected {
                            Style::default().bg(theme::CHAT_SELECTION)
                        } else {
                            Style::default()
                        })
                    })
                    .collect::<Vec<_>>();
                frame.render_widget(
                    Paragraph::new(lines)
                        .style(text_style.bg(theme::CHAT_PANEL))
                        .scroll((self.messages.selected.saturating_sub(4) as u16, 0))
                        .block(chat_block(self.messages.target_label(), messages_focused)),
                    content[0],
                );
                let selected = self.messages.items.get(self.messages.selected);
                let body = selected
                    .map(|v| {
                        let tags = v["data"]["tags"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(Value::as_str)
                                    .map(|s| format!("#{s}"))
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            })
                            .unwrap_or_default();
                        let links = v["data"]["links"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .map(|v| {
                                        format!(
                                            "{}: {}",
                                            v["label"].as_str().unwrap_or("Reference"),
                                            v["uri"].as_str().unwrap_or("")
                                        )
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                            .unwrap_or_default();
                        let delivery = v["notification"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v["status"].as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            })
                            .unwrap_or_default();
                        let read = v["read"] == true;
                        let mut lines = vec![Line::from(vec![
                            Span::styled(
                                v["actor"]["name"].as_str().unwrap_or("").to_owned(),
                                chat_actor_style(v).add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(
                                format!(" · {} · ", v["created_at"].as_str().unwrap_or("")),
                                muted,
                            ),
                            Span::styled(
                                if read { "read" } else { "unread" },
                                if read { muted } else { Style::default().fg(theme::CHAT_TAG) },
                            ),
                        ])];
                        lines.extend(v["body"].as_str().unwrap_or("").lines().map(|line| {
                            Line::styled(line.to_owned(), text_style)
                        }));
                        if !tags.is_empty() {
                            lines.push(Line::styled(tags, Style::default().fg(theme::CHAT_TAG)));
                        }
                        lines.extend(links.lines().map(|line| Line::styled(line.to_owned(), accent)));
                        if !delivery.is_empty() {
                            lines.push(Line::styled(delivery, muted));
                        }
                        lines
                    })
                    .unwrap_or_else(|| {
                        vec![Line::styled(
                            "No messages. Choose a channel or person; c composes.",
                            muted,
                        )]
                    });
                frame.render_widget(
                    Paragraph::new(body)
                        .style(text_style.bg(theme::CHAT_PANEL))
                        .wrap(Wrap { trim: false })
                        .scroll((self.messages.body_scroll, 0))
                        .block(chat_block(
                            "Message · Enter marks read · PgUp/PgDn scroll",
                            messages_focused,
                        )),
                    content[1],
                );
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
                    format!(
                        "Owner → {} · {}/3000 · Ctrl+s sends",
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
                    let inner = content[2].inner(ratatui::layout::Margin::new(1, 1));
                    let (lines, cursor_row, cursor_col) =
                        wrap_draft(&text, d.cursor, inner.width.max(1) as usize);
                    let scroll = cursor_row.saturating_sub(inner.height.saturating_sub(1) as usize);
                    frame.render_widget(
                        Paragraph::new(lines.join("\n"))
                            .style(text_style.bg(theme::CHAT_PANEL))
                            .scroll((scroll as u16, 0))
                            .block(chat_block(title, editing)),
                        content[2],
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
                        content[2],
                    );
                }
            }
        }
        let status = if self.messages.error.is_empty() {
            self.messages.status.as_str()
        } else {
            self.messages.error.as_str()
        };
        let status_style = Style::default().fg(if !self.messages.error.is_empty() {
            theme::ERROR
        } else if self.messages.busy {
            theme::CHAT_TAG
        } else {
            theme::CHAT_OWNER
        });
        frame.render_widget(
            Paragraph::new(vec![
                chat_help(&[("c", "compose"), ("r", "reply"), ("t", "thread"), ("n", "channel"), ("/", "filter"), ("e", "tags/mentions/links")]),
                chat_help(&[("s", "DM session"), ("N", "rename session"), ("]", "older"), ("g", "refresh"), ("w", "save draft to file")]),
                Line::styled(format!("{}{}", if self.messages.busy { "Working… " } else { "" }, status), status_style),
            ]),
            rows[2],
        );
    }
}

fn chat_block(title: impl Into<String>, focused: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused { theme::CHAT_FOCUS } else { theme::CHAT_BORDER }))
        .title(Span::styled(title.into(), Style::default().fg(theme::CHAT_FOCUS)))
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
        spans.push(Span::styled(key.to_string(), Style::default().fg(theme::CHAT_FOCUS)));
        spans.push(Span::styled(format!(" {label}"), Style::default().fg(theme::CHAT_MUTED)));
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
    struct Home {
        old: Option<std::ffi::OsString>,
        _temp: tempfile::TempDir,
        sockets: Vec<(String, Option<std::ffi::OsString>)>,
    }
    impl Home {
        fn new() -> Self {
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
            is_pushing: false,
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
