//! F9 Staffing: central Owner policy; all RPC work stays off the input thread.
use super::*;
use cm_daemon::staffing::{Engine, Policy, Preference, Snapshot, Strength, KINDS};
use crossterm::event::KeyModifiers;
use serde_json::{json, Value};
use std::sync::mpsc;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

type Target = (String, PathBuf, String);
type Reply = Result<(Snapshot, Target), String>;
#[derive(Clone, Copy, PartialEq)]
enum Operation {
    Poll,
    Reload,
    Save,
}
struct Dialog {
    version: u64,
    policy: Policy,
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
            preference: p.map(|p| p.engine),
            strength: p.map_or(Strength::Strong, |p| p.strength),
            reason: p.map_or(String::new(), |p| p.reason.clone()),
            expires: p.map_or(String::new(), |p| p.expires_at.to_rfc3339()),
            row: 0,
            error: None,
        }
    }
    fn policy(&self) -> Result<Policy, String> {
        let mut policy = self.policy.clone();
        policy.preference = self
            .preference
            .map(|engine| -> Result<Preference, String> {
                let expires_at = chrono::DateTime::parse_from_rfc3339(&self.expires)
                    .map_err(|_| {
                        "Expiry needs a date and timezone, e.g. 2026-11-08T00:00:00Z".to_string()
                    })?
                    .with_timezone(&chrono::Utc);
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
            KeyCode::Tab | KeyCode::Down => d.row = (d.row + 1) % 11,
            KeyCode::BackTab | KeyCode::Up => d.row = (d.row + 10) % 11,
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
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if d.row < 9 => match d.row {
                0 => {
                    d.policy.default_engine = if d.policy.default_engine == Engine::Codex {
                        Engine::Claude
                    } else {
                        Engine::Codex
                    }
                }
                1..=6 => {
                    let kind = KINDS[d.row - 1];
                    match d.policy.rules.get(kind) {
                        None => {
                            d.policy.rules.insert(kind.into(), Engine::Codex);
                        }
                        Some(Engine::Codex) => {
                            d.policy.rules.insert(kind.into(), Engine::Claude);
                        }
                        Some(Engine::Claude) => {
                            d.policy.rules.remove(kind);
                        }
                    }
                }
                7 => {
                    d.preference = match d.preference {
                        None => Some(Engine::Codex),
                        Some(Engine::Codex) => Some(Engine::Claude),
                        Some(Engine::Claude) => None,
                    }
                }
                8 => {
                    d.strength = if d.strength == Strength::Strong {
                        Strength::Lean
                    } else {
                        Strength::Strong
                    }
                }
                _ => (),
            },
            KeyCode::Backspace if d.row >= 9 => {
                if d.row == 9 {
                    d.reason.pop();
                } else {
                    d.expires.pop();
                }
            }
            KeyCode::Char(c)
                if d.row >= 9
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let value = if d.row == 9 {
                    &mut d.reason
                } else {
                    &mut d.expires
                };
                if value.chars().count() < 500 && !c.is_control() {
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
        let height = area.height.min(20);
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
        rows.extend(KINDS.iter().map(|kind| {
            format!(
                "{kind}: {}",
                d.policy.rules.get(*kind).map_or("no rule", |e| e.wire())
            )
        }));
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
        rows.push(format!("Expires (RFC3339): {}", d.expires));
        let mut lines: Vec<Line> = rows
            .into_iter()
            .enumerate()
            .map(|(i, s)| {
                // Text editing appends at the end; keep that end visible.
                let s = if i == d.row && i >= 9 && s.width() + 2 > inner.width as usize {
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
        lines.push(Line::from(
            "Tab/↑/↓ field · ←/→ change · type reason/expiry",
        ));
        lines.push(Line::from("Enter save · Esc back · Ctrl+r reload/discard"));
        lines.push(Line::from(
            "Explicit choice → strong preference → rule → lean → default",
        ));
        if self
            .io
            .as_ref()
            .is_some_and(|(op, _)| *op != Operation::Poll)
            || self
                .pending
                .as_ref()
                .is_some_and(|(op, _)| *op != Operation::Poll)
        {
            lines.push(Line::from("Loading/saving…"));
        }
        if let Some(error) = &d.error {
            lines.push(Line::styled(
                error.clone(),
                Style::default().fg(theme::ERROR),
            ));
        }
        let scroll = d
            .row
            .saturating_sub(inner.height.saturating_sub(2) as usize) as u16;
        frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), inner);
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
        d.row = 7;
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
            expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
        });
        assert_eq!(ui.engine(), LaunchEngine::Codex);
        ui.snapshot.policy.preference.as_mut().unwrap().expires_at =
            chrono::Utc::now() - chrono::Duration::seconds(1);
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
