//! Owner controls for norms, durable monitors and private notification rules.
use super::*;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct NormDraft {
    pub(super) text: String,
    base: String,
    revision: String,
    summary: String,
    pub(super) cursor: usize,
    pub(super) revert: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Pending {
    method: String,
    params: Value,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct SavedManagement {
    pub norms: BTreeMap<String, NormDraft>,
    pub pending: Option<Pending>,
    forms: BTreeMap<String, Vec<String>>,
}
#[derive(Default)]
pub(super) struct Management {
    pub context: Value,
    monitor_status: Value,
    pub last_position: Value,
    pub request: Value,
    doc: Value,
    text: String,
    current_text: String,
    current_revision: String,
    view: String,
    history: Vec<Value>,
    monitors: Vec<Value>,
    result: Value,
    prefs: Value,
    scope: String,
    next: Value,
    paging: bool,
    selected: usize,
    after_document: String,
    revert_target: String,
    viewed: usize,
    width: u16,
}
impl Management {
    pub fn badge_label(&self) -> String {
        let n = self.monitor_status["badges"].as_u64().unwrap_or(0);
        if n == 0 {
            String::new()
        } else {
            format!(" · {n}")
        }
    }
}
impl Messages {
    pub(super) fn management_view(&self) -> bool {
        self.target["norms"] == true
            || self.target["monitors"] == true
            || self.target["preferences"] == true
            || self.mode.starts_with("norm_")
            || matches!(self.mode.as_str(), "monitor_form" | "follow_form")
    }
    pub(super) fn conversation_label(&self, id: &Value) -> String {
        if let Some(c) = self.channels.iter().find(|c| c["id"] == *id) {
            return format!("#{}", c["path"].as_str().unwrap_or("?"));
        }
        if let Some(d) = self.dms.iter().find(|d| d["id"] == *id) {
            return format!("DM {}", self.dm_label(d));
        }
        "DM".into()
    }
    fn scope_input(&self) -> String {
        if let Some(thread) = self.filter["thread"].as_str() {
            return format!("thread:{thread}");
        }
        if let Some(path) = self.target["channel"].as_str() {
            return format!("#{path}");
        }
        if let Some(dm) = self.target["dm"].as_str() {
            return format!("@{dm}");
        }
        if let Some(cid) = self.target["conversation"].as_str() {
            return format!("conversation:{cid}");
        }
        "dms".into()
    }
    fn parse_scope(&self, s: &str) -> Result<Value, String> {
        let s = s.trim();
        if s == "dms" {
            return Ok(json!({"dms":true}));
        }
        if let Some(path) = s.strip_prefix('#') {
            return Ok(if let Some(base) = path.strip_suffix("/**") {
                json!({"channel":base,"include_children":true})
            } else {
                json!({"channel":path})
            });
        }
        if let Some(id) = s.strip_prefix("thread:") {
            return Ok(json!({"thread":id.trim()}));
        }
        if let Some(id) = s.strip_prefix("conversation:") {
            return Ok(json!({"conversation":id.trim()}));
        }
        if let Some(peer) = s.strip_prefix('@') {
            let matches: Vec<_> = self
                .people
                .iter()
                .filter(|p| {
                    p["id"] == peer
                        || p["name"]
                            .as_str()
                            .is_some_and(|n| n.eq_ignore_ascii_case(peer))
                })
                .collect();
            if matches.len() == 1 {
                return Ok(json!({"dm":matches[0]["id"]}));
            }
            return Err("Use an exact person name after @, or their participant ID".into());
        }
        Err("Scope: #channel, #channel/**, @person, dms, or thread:message-id".into())
    }
    fn begin_management_form(&mut self, mode: &str, defaults: Vec<String>) {
        let fields = self
            .saved
            .management
            .forms
            .get(mode)
            .cloned()
            .unwrap_or(defaults);
        self.start_form(mode, fields);
        self.pane = 1;
    }
    pub(super) fn keep_management_form(&mut self) {
        if matches!(
            self.mode.as_str(),
            "monitor_form" | "follow_form" | "norm_summary"
        ) {
            self.saved
                .management
                .forms
                .insert(self.mode.clone(), self.fields.clone());
            self.persist();
        }
    }
    pub(super) fn management_help(&self) -> String {
        if self.mode == "norm_edit" {
            return "Norms draft · Enter newline · Ctrl+s review · Esc keeps draft".into();
        }
        if self.mode == "norm_preview" {
            return "j/k scroll diff · Ctrl+s publish · Esc edit draft".into();
        }
        if self.mode == "norm_conflict" {
            return "Concurrent edit · b rebase saved draft · j/k scroll changes · Esc keeps draft"
                .into();
        }
        if !self.mode.is_empty() {
            return "Tab field · Enter save/review · Esc keeps draft".into();
        }
        if self.target["norms"] == true {
            return "d diff · h history · g current · p edit · v revert · D archive draft · Enter acknowledge · ] next".into();
        }
        if self.target["monitors"] == true {
            return "n new · Enter results/open · a acknowledge · x cancel · D dismiss · X cancel all · b list · ] next".into();
        }
        "e edit follow · Enter inspect rule · x inherit defaults · D toggle DND · B toggle bell"
            .into()
    }
}
pub(super) fn wrap_readable(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for line in text.split('\n') {
        let mut rest = line;
        loop {
            let mut cells = 0;
            let mut end = 0;
            for (index, c) in rest.char_indices() {
                let n = Span::raw(c.to_string()).width();
                if cells + n > width && index > 0 {
                    break;
                }
                cells += n;
                end = index + c.len_utf8();
            }
            if end == rest.len() {
                out.push(rest.to_owned());
                break;
            }
            let cut = rest[..end]
                .rfind(char::is_whitespace)
                .filter(|i| *i > 0)
                .unwrap_or(end);
            out.push(rest[..cut].trim_end().to_owned());
            rest = rest[cut..].trim_start_matches([' ', '\t']);
        }
    }
    out
}
fn scope_request(scope: &Value) -> Value {
    let mut out = json!({});
    for key in ["channel", "conversation", "thread"] {
        if scope[key].is_string() {
            out[key] = scope[key].clone();
        }
    }
    if scope["peer"].is_string() {
        out["dm"] = scope["peer"].clone();
    }
    for key in ["dms", "include_children"] {
        if scope[key] == true {
            out[key] = json!(true);
        }
    }
    out
}
fn scope_label(scope: &Value) -> String {
    if let Some(t) = scope["thread"].as_str() {
        format!("thread:{t}")
    } else if let Some(p) = scope["channel"].as_str() {
        format!(
            "#{p}{}",
            if scope["include_children"] == true {
                "/**"
            } else {
                ""
            }
        )
    } else if let Some(p) = scope["peer"].as_str() {
        format!("@{p}")
    } else if let Some(p) = scope["conversation"].as_str() {
        format!("conversation:{p}")
    } else {
        "dms".into()
    }
}
fn yes(s: &str) -> Result<bool, String> {
    match s.trim().to_lowercase().as_str() {
        "yes" | "true" | "on" => Ok(true),
        "no" | "false" | "off" => Ok(false),
        _ => Err("Use yes or no for switches".into()),
    }
}
impl App {
    pub(super) fn messaging_context(&mut self, v: &Value) {
        if v["context_status"].is_object() {
            self.messages.management.context = v["context_status"].clone();
        }
        if v["monitor_status"].is_object() {
            self.messages.management.monitor_status = v["monitor_status"].clone();
        }
        if v["attention"]["ring"] == true {
            use std::io::Write;
            let _ = std::io::stdout().write_all(b"\x07");
        }
    }
    pub(super) fn messaging_refresh_target(&mut self) {
        if self.messages.target["norms"] == true {
            if self.messages.management.doc.is_null() {
                self.messaging_document(json!({"action":"read"}));
            } else {
                self.messaging_request("messaging.open", json!({"claim_bell":true}));
            }
        } else if self.messages.target["monitors"] == true {
            if self.messages.management.view == "results" || self.messages.management.paging {
                self.messaging_request("messaging.open", json!({"claim_bell":true}));
            } else {
                self.messaging_request(
                    "messaging.monitors",
                    json!({"action":"list","claim_bell":true}),
                );
            }
        } else if self.messages.target["preferences"] == true {
            if self.messages.management.paging {
                self.messaging_request("messaging.open", json!({"claim_bell":true}));
                return;
            }
            let scope = self
                .messages
                .parse_scope(&self.messages.management.scope)
                .ok();
            self.messaging_request(
                "messaging.follow",
                json!({"action":"get","scope":scope,"claim_bell":true}),
            );
        } else {
            self.messaging_request("messaging.read", self.messages.query());
        }
    }
    fn messaging_document(&mut self, p: Value) {
        self.messaging_request("norms_document", p);
    }
    fn messaging_mutation(&mut self, method: &str, mut p: Value) {
        if self.messages.saved.management.pending.is_some() {
            self.messages.error = "A saved operation is pending; press R to retry it first".into();
            return;
        }
        p["request_id"] = json!(uuid::Uuid::new_v4().to_string());
        p["origin_daemon_id"] = json!(self.messages.daemon_id);
        self.messages.saved.management.pending = Some(Pending {
            method: method.into(),
            params: p.clone(),
        });
        if self.messages.persist() {
            self.messaging_request(method, p);
        }
    }
    pub(super) fn messaging_management_error(&mut self, method: &str, error: &str) {
        let definite = [
            "invalid_params:",
            "invalid_scope:",
            "not_found:",
            "norms_too_long:",
            "invalid_time:",
            "monitor_limit:",
            "invalid_receipt:",
            "event_too_large:",
            "preference_limit:",
            "unsupported_feature:",
        ];
        if self
            .messages
            .saved
            .management
            .pending
            .as_ref()
            .is_some_and(|p| p.method == method)
            && definite.iter().any(|s| error.contains(s))
        {
            self.messages.saved.management.pending = None;
            self.messages.persist();
        }
    }
    pub(super) fn messaging_management_result(&mut self, method: &str, v: &Value) -> bool {
        if method == "messaging.open" {
            return true;
        }
        if ![
            "messaging.norms",
            "norms_document",
            "messaging.monitor",
            "messaging.monitors",
            "messaging.follow",
        ]
        .contains(&method)
        {
            return false;
        }
        let mutation = self
            .messages
            .saved
            .management
            .pending
            .as_ref()
            .is_some_and(|p| p.method == method);
        if mutation {
            self.messages.saved.management.pending = None;
            self.messages.persist();
        }
        let action = self.messages.management.request["action"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        if method == "norms_document" {
            self.messages.management.doc = v.clone();
            self.messages.management.next = Value::Null;
            self.messages.management.text = v["text"].as_str().unwrap_or("").into();
            self.messages.management.view = "document".into();
            self.messages.management.viewed = 0;
            self.messages.body_scroll = 0;
            if v["format"] == "markdown"
                && v["revision"] == self.messages.management.context["current"]["global"]
            {
                self.messages.management.current_text = self.messages.management.text.clone();
                self.messages.management.current_revision =
                    v["revision"].as_str().unwrap_or("").into();
            }
            let next = std::mem::take(&mut self.messages.management.after_document);
            if next == "edit" {
                self.messaging_edit_norms();
            }
            if next == "rebase" {
                let m = &mut self.messages;
                if let Some(d) = m.saved.management.norms.get_mut(&m.space_id) {
                    d.base = m.management.current_text.clone();
                    d.revision = m.management.current_revision.clone();
                }
                m.persist();
                m.mode = "norm_edit".into();
                m.status = "Draft retained; review it against the new base".into();
            }
            if next == "revert" {
                self.messaging_revert_norms();
            }
        } else if method == "messaging.norms" {
            if action == "history" {
                self.messages.management.history =
                    v["items"].as_array().cloned().unwrap_or_default();
                self.messages.management.next = v["next_cursor"].clone();
                self.messages.management.view = "history".into();
                self.messages.management.selected = 0;
            } else if v["status"] == "conflict" {
                self.messages.mode = "norm_conflict".into();
                self.messages.status = "Norms changed while you edited; your draft is saved".into();
                self.messages.management.text = v["text"].as_str().unwrap_or("").into();
                self.messages.body_scroll = 0;
                // Fetch every conflict diff page before offering a rebase.
                self.messages.management.after_document = String::new();
                let since = self
                    .messages
                    .saved
                    .management
                    .norms
                    .get(&self.messages.space_id)
                    .map(|d| d.revision.clone());
                self.messaging_document(json!({"action":"diff","since":{"global":since},"revision":v["current_revision"]}));
            } else if v["status"] == "published" {
                self.messages
                    .saved
                    .management
                    .norms
                    .remove(&self.messages.space_id);
                self.messages.saved.management.forms.remove("norm_summary");
                self.messages.persist();
                self.messages.mode.clear();
                self.messages.status = "Norms published as a new revision".into();
                self.messaging_document(json!({"action":"read"}));
            }
        } else if method == "messaging.monitor" {
            self.messages.mode.clear();
            self.messages.target = json!({"monitors":true});
            self.messages.management.view = "monitors".into();
            self.messages.saved.management.forms.remove("monitor_form");
            self.messages.persist();
            self.messages.status = "Monitor saved; results survive closing this panel".into();
            self.messaging_refresh_target();
        } else if method == "messaging.monitors" {
            if action == "list" {
                let selected = self
                    .messages
                    .management
                    .monitors
                    .get(self.messages.management.selected)
                    .map(|m| m["id"].clone());
                self.messages.management.monitors =
                    v["items"].as_array().cloned().unwrap_or_default();
                self.messages.management.next = v["next_cursor"].clone();
                self.messages.management.selected = selected
                    .and_then(|id| {
                        self.messages
                            .management
                            .monitors
                            .iter()
                            .position(|m| m["id"] == id)
                    })
                    .unwrap_or(0);
                self.messages.management.view = "monitors".into();
            } else if action == "get" {
                self.messages.management.result = v.clone();
                self.messages.management.next = v["next_cursor"].clone();
                self.messages.management.view = "results".into();
                self.messages.management.selected = 0;
            } else {
                self.messages.status = if action == "ack" {
                    "Result page acknowledged; message unread state is separate"
                } else {
                    "Monitor updated; recorded hits are retained"
                }
                .into();
                self.messages.management.view = "monitors".into();
                self.messaging_refresh_target();
            }
        } else if method == "messaging.follow" {
            self.messages.management.prefs = v.clone();
            self.messages.management.next = v["next_cursor"].clone();
            if v["status"] == "conflict" {
                self.messages.mode.clear();
                self.messages.status =
                    "Preferences changed; review current values before saving again".into();
            } else if mutation {
                self.messages.mode.clear();
                self.messages.saved.management.forms.remove("follow_form");
                self.messages.persist();
                self.messages.status = "Preferences saved".into();
            }
        }
        true
    }
    fn messaging_edit_norms(&mut self) {
        if self.messages.saved.management.pending.is_some() {
            self.messages.error = "Retry the pending operation with R first".into();
            return;
        }
        if !self
            .messages
            .saved
            .management
            .norms
            .contains_key(&self.messages.space_id)
        {
            if self.messages.management.current_revision
                != self.messages.management.context["current"]["global"]
                    .as_str()
                    .unwrap_or("")
                || self.messages.management.current_revision.is_empty()
            {
                self.messages.management.after_document = "edit".into();
                self.messaging_document(json!({"action":"read"}));
                return;
            }
            let m = &mut self.messages;
            m.saved.management.norms.insert(
                m.space_id.clone(),
                NormDraft {
                    text: m.management.current_text.clone(),
                    base: m.management.current_text.clone(),
                    revision: m.management.current_revision.clone(),
                    ..NormDraft::default()
                },
            );
            m.persist();
        }
        self.messages.mode = "norm_edit".into();
        self.messages.fields.clear();
        self.messages.body_scroll = 0;
    }
    fn messaging_revert_norms(&mut self) {
        if self.messages.management.current_revision
            != self.messages.management.context["current"]["global"]
                .as_str()
                .unwrap_or("")
        {
            self.messages.management.after_document = "revert".into();
            self.messaging_document(json!({"action":"read"}));
            return;
        }
        if self.messages.management.doc["revision"] != self.messages.management.revert_target {
            self.messages.management.after_document = "revert".into();
            self.messaging_document(
                json!({"action":"read","revision":self.messages.management.revert_target}),
            );
            return;
        }
        if self
            .messages
            .saved
            .management
            .norms
            .contains_key(&self.messages.space_id)
        {
            self.messages.error="A norms draft already exists; p resumes it. Publish or save it to a file before replacing it.".into();
            return;
        }
        let m = &mut self.messages;
        m.saved.management.norms.insert(
            m.space_id.clone(),
            NormDraft {
                text: m.management.text.clone(),
                base: m.management.current_text.clone(),
                revision: m.management.current_revision.clone(),
                summary: format!("Revert to {}", m.management.revert_target),
                revert: Some(m.management.revert_target.clone()),
                ..NormDraft::default()
            },
        );
        m.persist();
        m.begin_management_form(
            "norm_summary",
            vec![format!("Revert to {}", m.management.revert_target)],
        );
    }
    fn messaging_publish_norms(&mut self) {
        let Some(d) = self
            .messages
            .saved
            .management
            .norms
            .get(&self.messages.space_id)
            .cloned()
        else {
            return;
        };
        let mut p = json!({"action":"publish","text":d.text,"expected_revision":d.revision,"summary":d.summary});
        if let Some(revision) = d.revert {
            p["action"] = json!("revert");
            p["revision"] = json!(revision);
            p.as_object_mut().unwrap().remove("text");
        }
        self.messaging_mutation("messaging.norms", p);
    }
    fn messaging_management_submit(&mut self) {
        let f = self.messages.fields.clone();
        let result = (|| -> Result<(), String> {
            match self.messages.mode.as_str() {
                "norm_summary" => {
                    let d = self
                        .messages
                        .saved
                        .management
                        .norms
                        .get_mut(&self.messages.space_id)
                        .ok_or("No saved draft")?;
                    if f[0].trim().is_empty() {
                        return Err("Describe why the norms are changing".into());
                    }
                    d.summary = f[0].clone();
                    self.messages.persist();
                    self.messages.mode = "norm_preview".into();
                    self.messages.body_scroll = 0;
                }
                "monitor_form" => {
                    let mut p = json!({"scope":self.messages.parse_scope(&f[0])?,"mode":f[1].trim(),"notify":f[2].trim(),"include_self":yes(&f[4])?});
                    if !f[3].trim().is_empty() {
                        p["expires_in"] = json!(f[3].trim());
                    }
                    match f[5].trim() {
                        "now" => {}
                        "last read" => {
                            if self.messages.management.last_position.is_null() {
                                return Err(
                                    "Read messages first to establish a catch-up boundary".into()
                                );
                            }
                            p["after"] = self.messages.management.last_position.clone();
                        }
                        _ => return Err("Start must be now or last read".into()),
                    }
                    self.messaging_mutation("messaging.monitor", p);
                }
                "follow_form" => {
                    let scope = self.messages.parse_scope(&f[0])?;
                    self.messages.management.scope = f[0].clone();
                    self.messaging_mutation("messaging.follow",json!({"action":"set","scope":scope,"inbox":yes(&f[1])?,"wake":yes(&f[2])?,"muted":yes(&f[3])?,"expected_revision":self.messages.management.prefs["revision"]}));
                }
                _ => {}
            }
            Ok(())
        })();
        if let Err(e) = result {
            self.messages.error = e;
        }
    }
    pub(super) fn messaging_management_key(&mut self, key: &crossterm::event::KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::ALT) {
            return false;
        }
        if key.code == KeyCode::Char('R') && self.messages.mode != "norm_edit" {
            if let Some(p) = self.messages.saved.management.pending.clone() {
                self.messaging_request(&p.method, p.params);
                return true;
            }
        }
        if self.messages.saved.management.pending.is_some() && !self.messages.mode.is_empty() {
            if key.code == KeyCode::Esc {
                self.messages.mode.clear();
            } else {
                self.messages.error =
                    "Operation pending. Esc keeps it saved; R retries the original request.".into();
            }
            return true;
        }
        if self.messages.mode == "norm_edit" {
            match key.code {
                KeyCode::Esc => {
                    self.messages.mode.clear();
                    self.messages.persist();
                }
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    let d = self
                        .messages
                        .saved
                        .management
                        .norms
                        .get(&self.messages.space_id)
                        .cloned()
                        .unwrap_or_default();
                    if d.text.len() > 32768 {
                        self.messages.error =
                            "Norms exceed 32 KiB; shorten them or reference a file".into();
                    } else {
                        self.messages
                            .begin_management_form("norm_summary", vec![d.summary]);
                    }
                }
                KeyCode::Left | KeyCode::Right | KeyCode::Home | KeyCode::End | KeyCode::Delete => {
                    self.messages.move_cursor(key.code)
                }
                KeyCode::Backspace => self.messages.edit_text("", true),
                KeyCode::Enter => self.messages.edit_text("\n", false),
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.messages.edit_text(&c.to_string(), false)
                }
                _ => {}
            }
            return true;
        }
        if matches!(
            self.messages.mode.as_str(),
            "monitor_form" | "follow_form" | "norm_summary"
        ) {
            match key.code {
                KeyCode::Esc => {
                    self.messages.keep_management_form();
                    self.messages.mode.clear();
                }
                KeyCode::Tab => {
                    self.messages.field = (self.messages.field + 1) % self.messages.fields.len()
                }
                KeyCode::BackTab => {
                    self.messages.field = (self.messages.field + self.messages.fields.len() - 1)
                        % self.messages.fields.len()
                }
                KeyCode::Enter => self.messaging_management_submit(),
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.messaging_management_submit()
                }
                KeyCode::Backspace => self.messages.edit_text("", true),
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.messages.edit_text(&c.to_string(), false)
                }
                _ => {}
            }
            self.messages.keep_management_form();
            return true;
        }
        if matches!(
            self.messages.mode.as_str(),
            "norm_preview" | "norm_conflict"
        ) {
            match key.code {
                KeyCode::Char('s')
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && self.messages.mode == "norm_preview" =>
                {
                    self.messaging_publish_norms()
                }
                KeyCode::Char('b') if self.messages.mode == "norm_conflict" => {
                    self.messages.management.after_document = "rebase".into();
                    self.messaging_document(json!({"action":"read"}));
                }
                KeyCode::Esc => {
                    self.messages.mode = if self.messages.mode == "norm_preview" {
                        "norm_edit"
                    } else {
                        ""
                    }
                    .into();
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.messages.body_scroll = self.messages.body_scroll.saturating_add(1)
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.messages.body_scroll = self.messages.body_scroll.saturating_sub(1)
                }
                _ => {}
            }
            return true;
        }
        if !self.messages.mode.is_empty() {
            return false;
        }
        if key.code == KeyCode::Char('W')
            || key.code == KeyCode::Char('n') && self.messages.target["monitors"] == true
        {
            let scope = self.messages.scope_input();
            self.messages.begin_management_form(
                "monitor_form",
                vec![
                    scope,
                    "once".into(),
                    "badge".into(),
                    String::new(),
                    "no".into(),
                    "now".into(),
                ],
            );
            return true;
        }
        if key.code == KeyCode::Char('f') {
            self.messages.management.paging = false;
            self.messages.management.scope = self.messages.scope_input();
            self.messages.target = json!({"preferences":true});
            self.messages.pane = 1;
            self.messaging_refresh_target();
            return true;
        }
        if !self.messages.management_view() || self.messages.pane == 0 {
            return false;
        }
        let in_norms = self.messages.target["norms"] == true;
        match key.code {
            KeyCode::Char('j') | KeyCode::Down
                if !in_norms || self.messages.management.view == "history" =>
            {
                let count = if in_norms {
                    self.messages.management.history.len()
                } else if self.messages.target["preferences"] == true {
                    self.messages.management.prefs["rules"]
                        .as_array()
                        .map(Vec::len)
                        .unwrap_or(0)
                } else if self.messages.management.view == "results" {
                    self.messages.management.result["items"]
                        .as_array()
                        .map(Vec::len)
                        .unwrap_or(0)
                } else {
                    self.messages.management.monitors.len()
                };
                self.messages.management.selected =
                    (self.messages.management.selected + 1).min(count.saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up
                if !in_norms || self.messages.management.view == "history" =>
            {
                self.messages.management.selected =
                    self.messages.management.selected.saturating_sub(1)
            }
            KeyCode::Char('g') if in_norms => self.messaging_document(json!({"action":"read"})),
            KeyCode::Char('d') if in_norms => self.messaging_document(json!({"action":"diff"})),
            KeyCode::Char('h') if in_norms => {
                self.messaging_request("messaging.norms", json!({"action":"history"}))
            }
            KeyCode::Char('p') if in_norms => self.messaging_edit_norms(),
            KeyCode::Char('D') if in_norms => {
                if let Some(d) = self
                    .messages
                    .saved
                    .management
                    .norms
                    .get(&self.messages.space_id)
                {
                    let path = Messages::path()
                        .with_file_name(format!("norms-draft-{}.md", uuid::Uuid::new_v4()));
                    match std::fs::write(&path, &d.text) {
                        Ok(()) => {
                            self.messages
                                .saved
                                .management
                                .norms
                                .remove(&self.messages.space_id);
                            self.messages.persist();
                            self.messages.status = format!("Draft archived at {}", path.display());
                        }
                        Err(e) => self.messages.error = e.to_string(),
                    }
                }
            }
            KeyCode::Char('v') if in_norms && self.messages.management.view == "history" => {
                if let Some(h) = self
                    .messages
                    .management
                    .history
                    .get(self.messages.management.selected)
                {
                    self.messages.management.revert_target =
                        h["revision"].as_str().unwrap_or("").into();
                    self.messaging_revert_norms();
                }
            }
            KeyCode::Enter if in_norms => {
                if self.messages.management.view == "history" {
                    if let Some(h) = self
                        .messages
                        .management
                        .history
                        .get(self.messages.management.selected)
                    {
                        self.messaging_document(json!({"action":"read","revision":h["revision"]}));
                    }
                } else if self.messages.management.doc["complete"] == true
                    && self.messages.management.viewed
                        >= self.messages.management.text.lines().count().max(1)
                    && self.messages.management.view == "read_complete"
                {
                    self.messaging_document(json!({"action":"read","revision":self.messages.management.doc["revision"],"ack_revision":self.messages.management.doc["revision"]}));
                    self.messages.status = "Norms revision acknowledged".into();
                } else {
                    self.messages.status =
                        "Scroll through the complete text or diff before acknowledging".into();
                }
            }
            KeyCode::Char('b') if self.messages.target["monitors"] == true => {
                self.messages.management.view = "monitors".into();
                self.messages.management.paging = false;
                self.messaging_refresh_target();
            }
            KeyCode::Enter if self.messages.target["monitors"] == true => {
                if self.messages.management.view == "results" {
                    if let Some(hit) = self.messages.management.result["items"]
                        .get(self.messages.management.selected)
                        .cloned()
                    {
                        self.messages.target = json!({"conversation":hit["conversation_id"]});
                        self.messages.filter = json!({"thread":hit["id"]});
                        self.messages.page_cursor = Value::Null;
                        self.messaging_refresh_target();
                    }
                } else if let Some(m) = self
                    .messages
                    .management
                    .monitors
                    .get(self.messages.management.selected)
                {
                    self.messaging_request(
                        "messaging.monitors",
                        json!({"action":"get","monitor_id":m["id"],"unacknowledged_only":true}),
                    );
                }
            }
            KeyCode::Char('a' | 'x' | 'D' | 'X') if self.messages.target["monitors"] == true => {
                let result = self.messages.management.view == "results";
                let id = if result {
                    self.messages.management.result["monitor"]["id"].clone()
                } else {
                    self.messages
                        .management
                        .monitors
                        .get(self.messages.management.selected)
                        .map(|m| m["id"].clone())
                        .unwrap_or_default()
                };
                let action = match key.code {
                    KeyCode::Char('a') => "ack",
                    KeyCode::Char('x') => "cancel",
                    KeyCode::Char('D') => "dismiss",
                    _ => "cancel_all",
                };
                if action == "ack" && !result {
                    self.messages.error = "Open results before acknowledging a page".into();
                } else {
                    self.messaging_mutation("messaging.monitors",json!({"action":action,"monitor_id":id,"receipt":if action=="ack"{self.messages.management.result["receipt"].clone()}else{Value::Null}}));
                }
            }
            KeyCode::Char(']') if !self.messages.management.next.is_null() => {
                self.messages.management.paging = true;
                let next = self.messages.management.next.clone();
                if in_norms {
                    self.messaging_request(
                        "messaging.norms",
                        json!({"action":"history","cursor":next}),
                    );
                } else if self.messages.target["preferences"] == true {
                    let scope = self
                        .messages
                        .parse_scope(&self.messages.management.scope)
                        .ok();
                    self.messaging_request(
                        "messaging.follow",
                        json!({"action":"get","scope":scope,"cursor":next}),
                    );
                } else if self.messages.management.view == "results" {
                    self.messaging_request("messaging.monitors",json!({"action":"get","monitor_id":self.messages.management.result["monitor"]["id"],"unacknowledged_only":true,"cursor":next}));
                } else {
                    self.messaging_request(
                        "messaging.monitors",
                        json!({"action":"list","cursor":next}),
                    );
                }
            }
            KeyCode::Char('e') if self.messages.target["preferences"] == true => {
                let p = &self.messages.management.prefs;
                let v = &p["effective"];
                let switch = |key: &str| if v[key] == true { "yes" } else { "no" }.to_owned();
                let scope = if self.messages.management.scope.is_empty() {
                    "dms".into()
                } else {
                    self.messages.management.scope.clone()
                };
                self.messages.begin_management_form(
                    "follow_form",
                    vec![scope, switch("inbox"), switch("wake"), switch("muted")],
                );
            }
            KeyCode::Enter if self.messages.target["preferences"] == true => {
                if let Some(rule) = self.messages.management.prefs["rules"]
                    .get(self.messages.management.selected)
                    .cloned()
                {
                    self.messages.management.scope = scope_label(&rule["scope"]);
                    self.messaging_refresh_target();
                }
            }
            KeyCode::Char('x') if self.messages.target["preferences"] == true => {
                if let Ok(scope) = self.messages.parse_scope(&self.messages.management.scope) {
                    self.messaging_mutation("messaging.follow",json!({"action":"remove","scope":scope,"expected_revision":self.messages.management.prefs["revision"]}));
                }
            }
            KeyCode::Char('D' | 'B') if self.messages.target["preferences"] == true => {
                let field = if key.code == KeyCode::Char('D') {
                    "dnd"
                } else {
                    "bell"
                };
                let mut p = json!({"action":"set","expected_revision":self.messages.management.prefs["revision"]});
                p[field] = json!(self.messages.management.prefs[field] != true);
                self.messaging_mutation("messaging.follow", p);
            }
            KeyCode::Esc
            | KeyCode::Tab
            | KeyCode::PageDown
            | KeyCode::PageUp
            | KeyCode::Char('j' | 'k')
            | KeyCode::Down
            | KeyCode::Up => return false,
            _ => {}
        }
        true
    }
    pub(super) fn draw_messaging_management(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        focused: bool,
    ) {
        let m = &mut self.messages;
        let style = Style::default().fg(theme::CHAT_TEXT).bg(theme::CHAT_PANEL);
        let inner = area.inner(ratatui::layout::Margin::new(1, 1));
        let width = inner.width.max(1) as usize;
        if m.mode == "norm_edit" {
            let d = m.draft();
            let (lines, row, col) = wrap_draft(&d.body, d.cursor, width);
            let scroll = row.saturating_sub(inner.height.saturating_sub(1) as usize);
            frame.render_widget(
                Paragraph::new(lines.join("\n"))
                    .style(style)
                    .scroll((scroll as u16, 0))
                    .block(chat_block(
                        format!("Norms draft · {} / 32768 bytes", d.body.len()),
                        true,
                    )),
                area,
            );
            if inner.width > 0 && inner.height > 0 {
                frame.set_cursor_position((
                    inner.x + col.min(width - 1) as u16,
                    inner.y + (row - scroll) as u16,
                ));
            }
            return;
        }
        if matches!(
            m.mode.as_str(),
            "monitor_form" | "follow_form" | "norm_summary"
        ) {
            let (title, labels, hint) = match m.mode.as_str() {
                "monitor_form" => (
                    "New monitor",
                    vec![
                        "Scope",
                        "Mode: once / continuous",
                        "Notify: badge / wake / none",
                        "Expiry: blank = none, or 10m / 2h",
                        "Include own messages: yes / no",
                        "Start: now / last read",
                    ],
                    "Scopes: #channel, #channel/**, @person, dms, thread:message-id",
                ),
                "follow_form" => (
                    "Follow preferences",
                    vec![
                        "Scope",
                        "Add to inbox: yes / no",
                        "Wake: yes / no (Owner stays passive)",
                        "Hard mute: yes / no",
                    ],
                    "Inherited mutes and DND override wakes. Tags never notify.",
                ),
                _ => (
                    "Explain norms change",
                    vec!["Summary"],
                    "Enter opens a diff preview; Ctrl+s there publishes.",
                ),
            };
            let mut lines = wrap_readable(hint, width);
            lines.push(String::new());
            let mut selected_line = 0;
            for (i, field) in m.fields.iter().enumerate() {
                if i == m.field {
                    selected_line = lines.len();
                }
                let text = format!(
                    "{} {}: {}",
                    if i == m.field { "›" } else { " " },
                    labels.get(i).unwrap_or(&"Value"),
                    field
                );
                lines.extend(wrap_draft(&text, 0, width).0);
            }
            let scroll = selected_line.saturating_sub(inner.height.saturating_sub(2) as usize);
            frame.render_widget(
                Paragraph::new(lines.join("\n"))
                    .style(style)
                    .scroll((scroll as u16, 0))
                    .block(chat_block(title, true)),
                area,
            );
            return;
        }
        let (title, text, list) = if m.mode == "norm_preview" {
            let d = m
                .saved
                .management
                .norms
                .get(&m.space_id)
                .cloned()
                .unwrap_or_default();
            (
                "Review norms change · Ctrl+s publishes".to_owned(),
                cm_daemon::messaging::norms_diff(&d.base, &d.text, &d.revision, "draft"),
                false,
            )
        } else if m.mode == "norm_conflict" {
            (
                "Concurrent norms edit · saved draft preserved".into(),
                m.management.text.clone(),
                false,
            )
        } else if m.target["norms"] == true && m.management.view == "history" {
            let text = m
                .management
                .history
                .iter()
                .enumerate()
                .map(|(i, h)| {
                    format!(
                        "{} {} · {}\n  {} · {}",
                        if i == m.management.selected {
                            "›"
                        } else {
                            " "
                        },
                        h["revision"]
                            .as_str()
                            .unwrap_or("?")
                            .chars()
                            .take(8)
                            .collect::<String>(),
                        h["summary"].as_str().unwrap_or("Initial norms"),
                        h["actor"]["name"].as_str().unwrap_or("?"),
                        h["created_at"].as_str().unwrap_or("")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            ("Norms history · Enter reads · v reverts".into(), text, true)
        } else if m.target["norms"] == true {
            let changed = m.management.context["changed"] == true;
            let title = format!(
                "Shared norms{} · {}",
                if changed { " · changed" } else { "" },
                m.management.doc["revision"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(8)
                    .collect::<String>()
            );
            (
                title,
                if m.management.doc.is_null() {
                    m.norms.clone()
                } else {
                    m.management.text.clone()
                },
                false,
            )
        } else if m.target["monitors"] == true && m.management.view == "results" {
            let r = &m.management.result;
            let monitor = &r["monitor"];
            let mut lines = vec![format!(
                "{} · {} · {} unread hits",
                scope_label(&monitor["scope"]),
                monitor["state"].as_str().unwrap_or("?"),
                monitor["unacknowledged"]
            )];
            lines.extend(
                r["items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                    .map(|(i, v)| {
                        format!(
                            "{} {} · {}\n  {}",
                            if i == m.management.selected {
                                "›"
                            } else {
                                " "
                            },
                            m.conversation_label(&v["conversation_id"]),
                            v["actor"]["name"].as_str().unwrap_or("?"),
                            v["preview"].as_str().unwrap_or("")
                        )
                    }),
            );
            if lines.len() == 1 {
                lines.push("No results on this page. b returns to monitors.".into());
            }
            (
                "Monitor results · a acknowledges this page".into(),
                lines.join("\n"),
                true,
            )
        } else if m.target["monitors"] == true {
            let mut lines = Vec::new();
            for (i, v) in m.management.monitors.iter().enumerate() {
                lines.push(format!(
                    "{} {} · {} · {} unread\n  {} / {} · expires {}",
                    if i == m.management.selected {
                        "›"
                    } else {
                        " "
                    },
                    scope_label(&v["scope"]),
                    v["state"].as_str().unwrap_or("?"),
                    v["unacknowledged"],
                    v["mode"].as_str().unwrap_or("?"),
                    v["notify"].as_str().unwrap_or("?"),
                    v["expires_at"].as_str().unwrap_or("never")
                ));
            }
            if lines.is_empty() {
                lines.push(
                    "No monitors. n creates one.\nOwner receives passive badges by default.".into(),
                );
            }
            (
                format!("Monitors{}", m.management.badge_label()),
                lines.join("\n"),
                true,
            )
        } else {
            let p = &m.management.prefs;
            let mut lines = vec![
                format!(
                    "DND: {} · Bell: {} · revision {}",
                    if p["dnd"] == true { "on" } else { "off" },
                    if p["bell"] == true { "on" } else { "off" },
                    p["revision"]
                ),
                "DMs and mentions enter Inbox. Channels require an explicit follow.".into(),
            ];
            if !p["scope"].is_null() {
                lines.push(format!(
                    "Selected: {}\nEffective: inbox {} · wake {} · muted {}",
                    scope_label(&p["scope"]),
                    p["effective"]["inbox"],
                    p["effective"]["wake"],
                    p["effective"]["muted"]
                ));
            }
            for (i, r) in p["rules"].as_array().into_iter().flatten().enumerate() {
                lines.push(format!(
                    "{} {}\n  inbox {} · wake {} · muted {}",
                    if i == m.management.selected {
                        "›"
                    } else {
                        " "
                    },
                    scope_label(&r["scope"]),
                    r["inbox"],
                    r["wake"],
                    r["muted"]
                ));
            }
            lines.push("e adds/edits a rule. x removes the selected override.\nOwner receives badges; bell is an explicit global preference.".into());
            (
                "Your notification preferences".into(),
                lines.join("\n"),
                true,
            )
        };
        let lines = wrap_readable(&text, width);
        let max_scroll = lines.len().saturating_sub(inner.height as usize);
        let scroll = if list {
            let marker = lines.iter().position(|l| l.starts_with('›')).unwrap_or(0);
            marker
                .saturating_sub(inner.height.saturating_sub(3) as usize)
                .min(max_scroll)
        } else {
            (m.body_scroll as usize).min(max_scroll)
        };
        if m.target["norms"] == true && !list && m.mode.is_empty() {
            if m.management.width != inner.width {
                m.management.width = inner.width;
                m.management.viewed = 0;
            }
            if scroll <= m.management.viewed {
                m.management.viewed = m
                    .management
                    .viewed
                    .max((scroll + inner.height as usize).min(lines.len()));
            }
            if m.management.viewed >= lines.len() && m.management.doc["complete"] == true {
                m.management.view = "read_complete".into();
            }
        }
        let spans = lines
            .into_iter()
            .map(|line| {
                let color = if line.starts_with('+') {
                    theme::CHAT_OWNER
                } else if line.starts_with('-') {
                    theme::CHAT_TAG
                } else if line.starts_with('›') {
                    theme::CHAT_FOCUS
                } else {
                    theme::CHAT_TEXT
                };
                Line::styled(line, Style::default().fg(color))
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(spans)
                .style(style)
                .scroll((scroll as u16, 0))
                .block(chat_block(title, focused || !m.mode.is_empty())),
            area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn app() -> App {
        let mut app = App::new(crate::config::Config {
            api_url: String::new(),
            api_token: String::new(),
            gcp_project: String::new(),
            gcp_zone: String::new(),
            repos: HashMap::new(),
        });
        app.messages.visible = true;
        app.messages.space_id = "space".into();
        app.messages.daemon_id = "origin".into();
        app.messages.pane = 1;
        app
    }
    fn key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
        app.messaging_event(&CrosstermEvent::Key(crossterm::event::KeyEvent::new(
            code, mods,
        )));
    }
    #[test]
    fn messaging_b_norm_draft_preview_conflict_and_pending_survive_restart() {
        let _lock = crate::test_support::home_lock();
        let _home = super::super::tests::Home::new();
        let mut a = app();
        a.messages.target = json!({"norms":true});
        a.messages.management.context = json!({"current":{"global":"r1"},"changed":true});
        a.messaging_management_result("norms_document",&json!({"text":"Old convention.\n","revision":"r1","format":"markdown","complete":true}));
        key(&mut a, KeyCode::Char('p'), KeyModifiers::NONE);
        a.messages.move_cursor(KeyCode::End);
        a.messaging_event(&CrosstermEvent::Paste("\nNew rule. 👋".into()));
        key(&mut a, KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(a.messages.mode, "norm_summary");
        a.messages.fields[0] = "Explain why".into();
        key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(a.messages.mode, "norm_preview");
        let body = a.messages.saved.management.norms["space"].text.clone();
        a.messages.saved.management.pending = Some(Pending {
            method: "messaging.norms".into(),
            params: json!({"action":"publish","text":body,"request_id":"fixed","origin_daemon_id":"origin"}),
        });
        a.messages.persist();
        let restored = Messages::load();
        assert_eq!(restored.saved.management.norms["space"].text, body);
        assert_eq!(
            restored.saved.management.pending.unwrap().params["request_id"],
            "fixed"
        );
        a.messages.management.request = json!({"action":"publish"});
        a.messaging_management_result(
            "messaging.norms",
            &json!({"status":"conflict","current_revision":"r2","text":"+Competing rule"}),
        );
        assert_eq!(a.messages.mode, "norm_conflict");
        assert_eq!(a.messages.saved.management.norms["space"].text, body);
        assert!(a.messages.saved.management.pending.is_none());
        a.messages.management.context = json!({"current":{"global":"r2"}});
        a.messages.management.after_document = "rebase".into();
        a.messaging_management_result(
            "norms_document",
            &json!({"text":"Competing rule\n","revision":"r2","format":"markdown","complete":true}),
        );
        assert_eq!(a.messages.saved.management.norms["space"].revision, "r2");
        assert_eq!(a.messages.saved.management.norms["space"].text, body);
    }
    #[test]
    fn messaging_refresh_buffers_owner_edits_instead_of_dropping_them() {
        let _lock = crate::test_support::home_lock();
        let _home = super::super::tests::Home::new();
        let mut a = app();
        a.messages.target = json!({"norms":true});
        a.messages.management.context = json!({"current":{"global":"r1"}});
        a.messaging_management_result(
            "norms_document",
            &json!({"text":"Initial rule","revision":"r1","format":"markdown","complete":true}),
        );
        a.messages.busy = true;
        key(&mut a, KeyCode::Char('p'), KeyModifiers::NONE);
        a.messaging_event(&CrosstermEvent::Paste("Buffered edit. ".into()));
        assert_eq!(a.messages.queued_events.len(), 2);
        a.messages.busy = false;
        a.messaging_tick();
        assert_eq!(a.messages.mode, "norm_edit");
        assert!(Messages::load().saved.management.norms["space"]
            .text
            .starts_with("Buffered edit. "));
    }
    #[test]
    fn messaging_b_forms_and_views_render_at_80x24_and_narrow() {
        let _lock = crate::test_support::home_lock();
        let _home = super::super::tests::Home::new();
        let mut a = app();
        a.messages.target = json!({"monitors":true});
        key(&mut a, KeyCode::Char('n'), KeyModifiers::NONE);
        assert_eq!(a.messages.fields[2], "badge");
        assert!(a.messages.fields[3].is_empty());
        a.messages.fields[0] = "#work/**".into();
        key(&mut a, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(
            Messages::load().saved.management.forms["monitor_form"][0],
            "#work/**"
        );
        key(&mut a, KeyCode::Char('n'), KeyModifiers::NONE);
        for mode in [
            "monitor_form",
            "follow_form",
            "norm_summary",
            "norm_edit",
            "norm_preview",
            "",
        ] {
            a.messages.mode = mode.into();
            if mode == "norm_edit" || mode == "norm_preview" {
                a.messages.saved.management.norms.insert(
                    "space".into(),
                    NormDraft {
                        text: "A new rule\nA second line 👋".into(),
                        base: "Old rule\n".into(),
                        ..NormDraft::default()
                    },
                );
            }
            for (w, h) in [(80, 24), (48, 16), (120, 35)] {
                let mut terminal =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
                terminal.draw(|f| a.draw_messages(f)).unwrap();
                let rendered = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|c| c.symbol())
                    .collect::<String>();
                assert!(rendered.contains("Owner"));
            }
        }
        a.messages.mode.clear();
        a.messages.target = json!({"norms":true});
        a.messages.management.doc = json!({"revision":"r","complete":true});
        a.messages.management.text = "Line of norms.\n".repeat(100);
        a.messages.management.view = "document".into();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| a.draw_messages(f)).unwrap();
        assert_ne!(a.messages.management.view, "read_complete");
        // Jumping past unseen lines cannot clear the badge.
        a.messages.body_scroll = 99;
        terminal.draw(|f| a.draw_messages(f)).unwrap();
        assert_ne!(a.messages.management.view, "read_complete");
        for offset in 0..100 {
            a.messages.body_scroll = offset;
            terminal.draw(|f| a.draw_messages(f)).unwrap();
        }
        assert_eq!(a.messages.management.view, "read_complete");
    }
}
