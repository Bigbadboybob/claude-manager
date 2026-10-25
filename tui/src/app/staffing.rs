//! F9 Staffing: central Owner policy; all RPC work stays off the input thread.
use super::*;
use cm_daemon::staffing::{Engine, Policy, Preference, Rule, Snapshot, Strength, KINDS};
use crossterm::event::KeyModifiers;
use serde_json::{json, Value};
use std::sync::mpsc;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

type Target = (String, PathBuf, String);
type Reply = Result<(Snapshot, Target), String>;
const PREFERENCE_ROW: usize = 1 + KINDS.len() * 2;
const STRENGTH_ROW: usize = PREFERENCE_ROW + 1;
const REASON_ROW: usize = PREFERENCE_ROW + 2;
const EXPIRY_ROW: usize = PREFERENCE_ROW + 3;
const ROW_COUNT: usize = PREFERENCE_ROW + 4;
fn model_index(row: usize) -> Option<usize> {
    (row >= 2 && row < PREFERENCE_ROW && row % 2 == 0).then(|| (row - 2) / 2)
}
fn text_row(row: usize) -> bool {
    matches!(row, REASON_ROW | EXPIRY_ROW) || model_index(row).is_some()
}
#[derive(Clone, Copy, PartialEq)]
enum Operation {
    Poll,
    Reload,
    Save,
}
struct Dialog {
    version: u64,
    policy: Policy,
    models: Vec<String>,
    preference: Option<Engine>,
    strength: Strength,
    reason: String,
    expires: String,
    row: usize,
    error: Option<String>,
}
impl Dialog {
    fn new(snapshot: &Snapshot) -> Self {
        let p = snapshot.policy.preference.as_ref();
        Self {
            version: snapshot.version,
            policy: snapshot.policy.clone(),
            models: KINDS
                .iter()
                .map(|k| {
                    snapshot
                        .policy
                        .rules
                        .get(*k)
                        .and_then(Rule::model)
                        .unwrap_or("")
                        .to_owned()
                })
                .collect(),
            preference: p.map(|p| p.engine),
            strength: p.map_or(Strength::Strong, |p| p.strength),
            reason: p.map_or(String::new(), |p| p.reason.clone()),
            expires: p
                .and_then(|p| p.expires_at)
                .map_or(String::new(), |e| e.to_rfc3339()),
            row: 0,
            error: None,
        }
    }
    fn text_field(&mut self) -> Option<&mut String> {
        match self.row {
            REASON_ROW => Some(&mut self.reason),
            EXPIRY_ROW => Some(&mut self.expires),
            row => model_index(row).map(|i| &mut self.models[i]),
        }
    }
    fn policy(&self) -> Result<Policy, String> {
        let mut policy = self.policy.clone();
        for (i, kind) in KINDS.iter().enumerate() {
            let model = self.models[i].trim();
            if let Some(rule) = policy.rules.get_mut(*kind) {
                *rule = Rule::new(rule.engine(), (!model.is_empty()).then(|| model.to_owned()));
            } else if !model.is_empty() {
                return Err(format!("Set the {kind} engine before its model"));
            }
        }
        policy.preference = self
            .preference
            .map(|engine| -> Result<Preference, String> {
                let expires_at = if self.expires.trim().is_empty() {
                    None
                } else {
                    Some(
                        chrono::DateTime::parse_from_rfc3339(self.expires.trim())
                            .map_err(|_| {
                                "Expiry needs a date and timezone, or blank for no expiry"
                                    .to_string()
                            })?
                            .with_timezone(&chrono::Utc),
                    )
                };
                Ok(Preference {
                    engine,
                    strength: self.strength,
                    reason: self.reason.clone(),
                    expires_at,
                })
            })
            .transpose()?;
        policy.validate()?;
        Ok(policy)
    }
}
pub(super) struct Staffing {
    pub snapshot: Snapshot,
    dialog: Option<Dialog>,
    target: Option<Target>,
    pending: Option<(Operation, Value)>,
    io: Option<(Operation, mpsc::Receiver<Reply>)>,
    polled: Option<Instant>,
}
impl Default for Staffing {
    fn default() -> Self {
        Self {
            snapshot: Snapshot::default(),
            dialog: None,
            target: None,
            pending: None,
            io: None,
            polled: None,
        }
    }
}
impl Staffing {
    pub fn is_open(&self) -> bool {
        self.dialog.is_some()
    }
    pub fn engine(&self) -> LaunchEngine {
        LaunchEngine::for_staffing(&self.snapshot.policy)
    }
    pub fn open(&mut self) {
        self.dialog = Some(Dialog::new(&self.snapshot));
        self.pending = Some((Operation::Reload, json!({"refresh":true})));
    }
    pub fn tick(&mut self, targets: impl FnOnce() -> Vec<Target> + Send + 'static) -> bool {
        let mut changed = false;
        let result = self.io.as_ref().and_then(|(op, rx)| match rx.try_recv() {
            Ok(r) => Some((*op, r)),
            Err(mpsc::TryRecvError::Disconnected) => {
                Some((*op, Err("Staffing request stopped".into())))
            }
            Err(mpsc::TryRecvError::Empty) => None,
        });
        if let Some((op, result)) = result {
            self.io = None;
            changed = true;
            match result {
                Ok((snapshot, target)) => {
                    if snapshot.version >= self.snapshot.version {
                        self.snapshot = snapshot;
                        self.target = Some(target);
                    }
                    if op == Operation::Save {
                        self.dialog = None;
                    } else if op == Operation::Reload && self.dialog.is_some() {
                        self.dialog = Some(Dialog::new(&self.snapshot));
                    }
                }
                Err(error) => {
                    if op != Operation::Poll {
                        if let Some(d) = &mut self.dialog {
                            d.error = Some(error);
                        }
                    }
                }
            }
        }
        if self.io.is_some() {
            return changed;
        }
        if self.pending.is_none()
            && self
                .polled
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(30))
        {
            self.pending = Some((Operation::Poll, json!({})));
        }
        let Some((op, params)) = self.pending.take() else {
            return changed;
        };
        self.polled = Some(Instant::now());
        let save_target = self.target.clone();
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("cm-staffing-ui".into())
            .spawn(move || {
                // Token resolution may fetch over SSH, so resolve hosts here,
                // never on the TUI input thread. Writes use one chosen host.
                let targets = if op == Operation::Save {
                    save_target.into_iter().collect()
                } else {
                    targets()
                };
                let mut best: Option<(Snapshot, Target)> = None;
                let mut error =
                    "No connected host serves staffing policy; Ctrl+r to reload".to_string();
                for target in targets {
                    let method = if op == Operation::Save {
                        "staffing.set"
                    } else {
                        "staffing.get"
                    };
                    match crate::client_session::rpc_messaging_board(
                        &target.1,
                        &target.2,
                        method,
                        params.clone(),
                    ) {
                        Ok(value) => match serde_json::from_value::<Snapshot>(value) {
                            Ok(snapshot) if snapshot.policy.validate().is_ok() => {
                                if best
                                    .as_ref()
                                    .is_none_or(|(old, _)| snapshot.version > old.version)
                                {
                                    best = Some((snapshot, target));
                                }
                            }
                            _ => error = "Invalid staffing policy response".into(),
                        },
                        Err(e) => error = e.to_string(),
                    }
                }
                let _ = tx.send(best.ok_or(error));
            });
        match worker {
            Ok(_) => self.io = Some((op, rx)),
            Err(e) => {
                if let Some(d) = &mut self.dialog {
                    d.error = Some(e.to_string());
                }
            }
        }
        true
    }
    pub fn event(&mut self, event: &CrosstermEvent) -> bool {
        let Some(d) = self.dialog.as_mut() else {
            return false;
        };
        let CrosstermEvent::Key(key) = event else {
            return true;
        };
        let saving = self
            .io
            .as_ref()
            .is_some_and(|(op, _)| *op == Operation::Save)
            || self
                .pending
                .as_ref()
                .is_some_and(|(op, _)| *op == Operation::Save);
        if matches!(key.code, KeyCode::Esc | KeyCode::F(9)) && !saving {
            self.dialog = None;
            self.pending = None;
            return true;
        }
        let busy = self
            .io
            .as_ref()
            .is_some_and(|(op, _)| *op != Operation::Poll)
            || self
                .pending
                .as_ref()
                .is_some_and(|(op, _)| *op != Operation::Poll);
        if busy {
            return true;
        }
        match key.code {
            KeyCode::Esc | KeyCode::F(9) => self.dialog = None,
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.pending = Some((Operation::Reload, json!({"refresh":true})));
            }
            KeyCode::Tab | KeyCode::Down => d.row = (d.row + 1) % ROW_COUNT,
            KeyCode::BackTab | KeyCode::Up => d.row = (d.row + ROW_COUNT - 1) % ROW_COUNT,
            KeyCode::Enter => match d.policy() {
                Ok(policy) => {
                    d.error = None;
                    self.pending = Some((
                        Operation::Save,
                        json!({"expected_version":d.version,"policy":policy}),
                    ));
                }
                Err(error) => d.error = Some(error),
            },
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if !text_row(d.row) => {
                match d.row {
                    0 => {
                        d.policy.default_engine = if d.policy.default_engine == Engine::Codex {
                            Engine::Claude
                        } else {
                            Engine::Codex
                        }
                    }
                    row if row < PREFERENCE_ROW => {
                        let i = (row - 1) / 2;
                        let kind = KINDS[i];
                        match d.policy.rules.get(kind).map(Rule::engine) {
                            None => {
                                d.policy
                                    .rules
                                    .insert(kind.into(), Rule::Engine(Engine::Codex));
                            }
                            Some(Engine::Codex) => {
                                d.policy
                                    .rules
                                    .insert(kind.into(), Rule::Engine(Engine::Claude));
                            }
                            Some(Engine::Claude) => {
                                d.policy.rules.remove(kind);
                                d.models[i].clear();
                            }
                        }
                    }
                    PREFERENCE_ROW => {
                        d.preference = match d.preference {
                            None => Some(Engine::Codex),
                            Some(Engine::Codex) => Some(Engine::Claude),
                            Some(Engine::Claude) => None,
                        }
                    }
                    STRENGTH_ROW => {
                        d.strength = if d.strength == Strength::Strong {
                            Strength::Lean
                        } else {
                            Strength::Strong
                        }
                    }
                    _ => (),
                }
            }
            KeyCode::Backspace if text_row(d.row) => {
                d.text_field().unwrap().pop();
            }
            KeyCode::Char(c)
                if text_row(d.row)
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let limit = if model_index(d.row).is_some() {
                    cm_daemon::models::MAX_MODEL_LEN
                } else {
                    500
                };
                let value = d.text_field().unwrap();
                if value.chars().count() < limit && !c.is_control() {
                    value.push(c);
                }
            }
            _ => (),
        }
        true
    }
    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let Some(d) = &self.dialog else {
            return;
        };
        let width = area.width.min(100);
        let height = area.height.min((ROW_COUNT + 6) as u16);
        let area = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" Staffing policy · v{} ", d.version));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let mut rows = vec![format!("Default: {}", d.policy.default_engine.wire())];
        for (i, kind) in KINDS.iter().enumerate() {
            rows.push(format!(
                "{kind}: {}",
                d.policy
                    .rules
                    .get(*kind)
                    .map_or("no rule", |r| r.engine().wire())
            ));
            rows.push(format!(
                "  model: {}",
                if d.models[i].is_empty() {
                    "(task/host default)"
                } else {
                    &d.models[i]
                }
            ));
        }
        rows.push(format!(
            "Current preference: {}",
            d.preference.map_or("none", |e| e.wire())
        ));
        rows.push(format!(
            "Preference strength: {}",
            if d.strength == Strength::Strong {
                "strong"
            } else {
                "lean"
            }
        ));
        rows.push(format!("Reason: {}", d.reason));
        rows.push(format!("Expires (blank = no expiry): {}", d.expires));
        let lines: Vec<Line> = rows
            .into_iter()
            .enumerate()
            .map(|(i, s)| {
                // Text editing appends at the end; keep that end visible.
                let s = if i == d.row && text_row(i) && s.width() + 2 > inner.width as usize {
                    let mut width = 0;
                    let tail: String = s
                        .chars()
                        .rev()
                        .take_while(|c| {
                            width += c.width().unwrap_or(0);
                            width <= inner.width.saturating_sub(3) as usize
                        })
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    format!("…{tail}")
                } else {
                    s
                };
                Line::styled(
                    format!("{} {s}", if i == d.row { "›" } else { " " }),
                    if i == d.row {
                        Style::default()
                            .fg(theme::TEXT)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(theme::MUTED)
                    },
                )
            })
            .collect();
        let footer_height = inner.height.min(4);
        let body = Rect {
            height: inner.height - footer_height,
            ..inner
        };
        let footer = Rect {
            y: inner.y + body.height,
            height: footer_height,
            ..inner
        };
        let scroll = d.row.saturating_sub(body.height.saturating_sub(1) as usize) as u16;
        frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), body);
        let busy = self
            .io
            .as_ref()
            .is_some_and(|(op, _)| *op != Operation::Poll)
            || self
                .pending
                .as_ref()
                .is_some_and(|(op, _)| *op != Operation::Poll);
        let status = if let Some(error) = &d.error {
            Line::styled(error.clone(), Style::default().fg(theme::ERROR))
        } else if busy {
            Line::from("Loading/saving…")
        } else {
            Line::from("Model: explicit → task → work rule → host")
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from("Tab/↑/↓ field · ←/→ engine · type model/reason/expiry"),
                Line::from("Enter save · Esc back · Ctrl+r reload/discard"),
                Line::from("Blank model uses task/host; blank expiry never expires"),
                status,
            ]),
            footer,
        );
    }
}
impl App {
    pub fn staffing_tick(&mut self) {
        let pool = self.host_pool.clone();
        if self.global_settings.staffing.tick(move || {
            if cfg!(test) {
                return Vec::new();
            }
            let local = cm_daemon::host_id::HostId::local();
            let mut hosts = vec![local.clone()];
            hosts.extend(pool.host_ids().into_iter().filter(|h| *h != local));
            hosts
                .iter()
                .filter_map(|h| {
                    let socket = pool.live_socket_path(h)?;
                    Some((h.to_string(), socket, pool.operator_token_for(h)))
                })
                .collect()
        }) {
            self.needs_redraw = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    fn key(code: KeyCode) -> CrosstermEvent {
        CrosstermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }
    #[test]
    fn staffing_editor_uses_version_and_validates_preference() {
        let mut ui = Staffing::default();
        ui.snapshot = Snapshot {
            version: 8,
            policy: Policy::default(),
            updated_at: None,
        };
        ui.open();
        ui.pending = None;
        let d = ui.dialog.as_mut().unwrap();
        d.row = PREFERENCE_ROW;
        ui.event(&key(KeyCode::Right)); // enable a Codex preference
        ui.event(&key(KeyCode::Enter));
        assert!(ui.pending.is_none());
        assert!(ui.dialog.as_ref().unwrap().error.is_some());
        let d = ui.dialog.as_mut().unwrap();
        d.reason = "higher limits".into();
        d.expires = "2026-11-08T00:00:00Z".into();
        ui.event(&key(KeyCode::Enter));
        let (op, body) = ui.pending.as_ref().unwrap();
        assert!(*op == Operation::Save);
        assert_eq!(body["expected_version"], 8);
        assert_eq!(body["policy"]["preference"]["engine"], "codex");
    }
    #[test]
    fn staffing_editor_round_trips_rule_models_and_indefinite_preference() {
        let mut snapshot = Snapshot {
            policy: Policy::default(),
            ..Snapshot::default()
        };
        snapshot.policy.preference = Some(Preference {
            engine: Engine::Codex,
            strength: Strength::Strong,
            reason: "foreseeable future".into(),
            expires_at: None,
        });
        let mut dialog = Dialog::new(&snapshot);
        assert!(dialog.expires.is_empty());
        let policy = dialog.policy().unwrap();
        assert_eq!(policy, snapshot.policy);
        assert_eq!(policy.rules["busywork"].model(), Some("gpt-6-luna"));
        let busywork = KINDS.iter().position(|k| *k == "busywork").unwrap();
        dialog.models[busywork] = "claude-opus-5-5".into();
        assert!(dialog.policy().is_err());
        dialog.models[busywork].clear();
        assert!(dialog.policy().unwrap().rules["busywork"].model().is_none());
        dialog.expires = "not a date".into();
        assert!(dialog.policy().is_err());
    }
    #[test]
    fn staffing_cancel_loading_and_small_terminal_render() {
        let mut ui = Staffing::default();
        ui.open();
        ui.event(&key(KeyCode::Esc));
        assert!(!ui.is_open() && ui.pending.is_none());
        ui.open();
        ui.pending = None;
        for (w, h) in [(1, 1), (20, 6), (100, 24)] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
            terminal.draw(|f| ui.draw(f, f.area())).unwrap();
        }
    }
    #[test]
    fn staffing_launch_default_follows_active_policy_and_expiry() {
        let mut ui = Staffing::default();
        assert_eq!(ui.engine(), LaunchEngine::Codex);
        ui.snapshot.policy.default_engine = Engine::Claude;
        assert_eq!(ui.engine(), LaunchEngine::Claude);
        ui.snapshot.policy.preference = Some(Preference {
            engine: Engine::Codex,
            strength: Strength::Strong,
            reason: "limits".into(),
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
        });
        assert_eq!(ui.engine(), LaunchEngine::Codex);
        ui.snapshot.policy.preference.as_mut().unwrap().expires_at =
            Some(chrono::Utc::now() - chrono::Duration::seconds(1));
        assert_eq!(ui.engine(), LaunchEngine::Claude);
    }

    #[test]
    fn staffing_background_poll_preserves_draft_and_save_error() {
        let mut ui = Staffing::default();
        ui.snapshot.version = 3;
        ui.open();
        ui.pending = None;
        ui.dialog.as_mut().unwrap().policy.default_engine = Engine::Claude;
        let (tx, rx) = mpsc::channel();
        ui.io = Some((Operation::Poll, rx));
        tx.send(Ok((
            Snapshot {
                version: 4,
                ..Snapshot::default()
            },
            ("local".into(), PathBuf::from("unused"), "token".into()),
        )))
        .unwrap();
        ui.tick(Vec::new);
        assert_eq!(ui.snapshot.version, 4);
        let d = ui.dialog.as_ref().unwrap();
        assert_eq!(d.version, 3);
        assert_eq!(d.policy.default_engine, Engine::Claude);

        let (tx, rx) = mpsc::channel();
        ui.io = Some((Operation::Save, rx));
        tx.send(Err("policy changed; reload".into())).unwrap();
        ui.tick(Vec::new);
        let d = ui.dialog.as_ref().unwrap();
        assert_eq!(d.version, 3);
        assert_eq!(d.error.as_deref(), Some("policy changed; reload"));
        assert_eq!(d.policy.default_engine, Engine::Claude);
    }
}
