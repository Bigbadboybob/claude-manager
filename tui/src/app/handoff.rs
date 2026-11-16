//! Alt+Shift+E: hand the focused session over to another engine, model or
//! effort at the same CM identity (#25, `session.handoff`).
//!
//! The dialog has two rows: Engine (←/→ Claude / Codex) and the shared
//! Model field (←/→ presets, typing, Shift+←/→ effort). A third line says
//! what will happen: another engine is a switch (the agent writes a handoff
//! document, then the new engine takes over), a new model or effort on the
//! same engine restarts the same conversation on it, and nothing changed is
//! a transcript rotation. The host runs it once the session is idle; Owner
//! asked, so the idle and Owner-quiet waits are skipped.

use super::*;
use crossterm::event::KeyEvent;

const F_ENGINE: u8 = 0;
const F_MODEL: u8 = 1;

#[derive(Debug, Clone)]
pub(crate) struct HandoffForm {
    pub session_uid: String,
    pub label: String,
    /// The session's engine now (`claude` / `codex`).
    pub from_engine: String,
    pub to_engine: String,
    pub model: super::model_field::ModelField,
    recorded_model: Option<String>,
    recorded_effort: Option<String>,
    active_field: u8,
    pub error: Option<String>,
}

impl HandoffForm {
    pub(crate) fn new(session_uid: &str, label: &str, engine: &str, model: Option<&str>, effort: Option<&str>) -> Self {
        Self {
            session_uid: session_uid.into(),
            label: label.into(),
            from_engine: engine.into(),
            to_engine: engine.into(),
            model: super::model_field::ModelField::recorded(engine, model, effort),
            recorded_model: model.map(str::to_string),
            recorded_effort: effort.map(str::to_string),
            active_field: F_ENGINE,
            error: None,
        }
    }

    fn switch_engine(&mut self) {
        self.to_engine = if self.to_engine == "codex" { "claude".into() } else { "codex".into() };
        self.model = if self.to_engine == self.from_engine {
            super::model_field::ModelField::recorded(
                &self.to_engine,
                self.recorded_model.as_deref(),
                self.recorded_effort.as_deref(),
            )
        } else {
            super::model_field::ModelField::new(&self.to_engine)
        };
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> InputOutcome {
        if self.active_field == F_MODEL && self.model.key(key) {
            self.error = None;
            return InputOutcome::Consumed;
        }
        match key.code {
            KeyCode::Esc => return InputOutcome::Cancel,
            KeyCode::Tab | KeyCode::Down | KeyCode::BackTab | KeyCode::Up => {
                self.active_field = if self.active_field == F_ENGINE { F_MODEL } else { F_ENGINE };
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if self.active_field == F_ENGINE => self.switch_engine(),
            KeyCode::Enter => {
                if let Err(e) = self.model.value() {
                    self.error = Some(e);
                    self.active_field = F_MODEL;
                    return InputOutcome::Consumed;
                }
                return InputOutcome::Submit(SubmitAction::Handoff);
            }
            _ => {}
        }
        self.error = None;
        InputOutcome::Consumed
    }

    /// What submitting will do, for the dialog and the status line.
    pub(crate) fn effect(&self) -> &'static str {
        let changed = self.model.value().ok().flatten().is_some_and(|m| Some(m.as_str()) != self.recorded_model.as_deref())
            || self.model.effort_value().is_some_and(|e| Some(e.as_str()) != self.recorded_effort.as_deref());
        if self.to_engine != self.from_engine {
            "Switch: the agent writes a handoff document, then the new engine takes over"
        } else if changed {
            "Restart on the new model/effort, same conversation"
        } else {
            "Same engine and model: rotate to a fresh transcript"
        }
    }

    /// `session.handoff` params (`now`: Owner asked).
    pub(crate) fn rpc_params(&self) -> serde_json::Value {
        let mut p = serde_json::json!({
            "uid": self.session_uid,
            "to_engine": self.to_engine,
            "now": true,
            "reason": "Owner asked (Alt+Shift+E)",
        });
        if let Ok(Some(m)) = self.model.value() {
            p["to_model"] = serde_json::json!(m);
        }
        if let Some(e) = self.model.effort_value() {
            p["to_effort"] = serde_json::json!(e);
        }
        p
    }

    pub(crate) fn draw(&self, frame: &mut Frame, area: Rect) {
        let width = 76u16.min(area.width.saturating_sub(4));
        let height = if self.error.is_some() { 11u16 } else { 9u16 };
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let dialog = Rect::new(x, y, width, height.min(area.height));
        frame.render_widget(Clear, dialog);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::TEXT))
            .title(Span::styled(
                " Hand off this session ",
                Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);
        let dim = Style::default().fg(theme::DIM);
        let white = Style::default().fg(theme::TEXT);
        let name = |e: &str| if e == "codex" { "Codex" } else { "Claude" };
        let radio = |on: bool| if on { "\u{25cf}" } else { "\u{25cb}" };
        let pick = |on: bool| if on { white } else { dim };
        let mark = if self.active_field == F_ENGINE { "> " } else { "  " };
        let codex = self.to_engine == "codex";
        let mut lines = vec![
            Line::from(vec![
                Span::styled("  Session: ", dim),
                Span::styled(sanitize_for_display(&self.label), white),
                Span::styled(format!("  (now {}; keeps its CM identity)", name(&self.from_engine)), dim),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled(format!("{mark}Engine:  "), dim),
                Span::styled(format!("{} Claude", radio(!codex)), pick(!codex)),
                Span::styled("   ", dim),
                Span::styled(format!("{} Codex", radio(codex)), pick(codex)),
            ]),
            self.model.line(if self.active_field == F_MODEL { "> Model:   " } else { "  Model:   " }, self.active_field == F_MODEL),
            Line::from(""),
            Line::from(Span::styled(format!("  {}", self.effect()), white)),
        ];
        if let Some(e) = &self.error {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(format!("  {e}"), Style::default().fg(theme::ATTN))));
        }
        lines.push(Line::from(Span::styled("  Tab field · ←/→ choose · Enter hand off · Esc cancel", dim)));
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

impl App {
    /// Alt+Shift+E on a live Claude/Codex row.
    pub(super) fn open_handoff(&mut self) {
        let Cursor::Session(wi, si) = self.cursor.clone() else {
            self.set_status_msg("Focus a Claude or Codex session to hand it off");
            return;
        };
        let Some(ts) = self.workspaces.get(wi).and_then(|w| w.sessions.get(si)) else { return };
        if !matches!(ts.session_type.as_str(), "claude" | "codex") {
            self.set_status_msg("Only Claude and Codex sessions can be handed off");
            return;
        }
        if ts.session.exited {
            self.set_status_msg("The session is not running; A-R revives it");
            return;
        }
        let form = HandoffForm::new(&ts.uid, &ts.label, &ts.session_type, ts.model.as_deref(), ts.effort.as_deref());
        self.input_mode = InputMode::Handoff(form);
    }

    pub(super) fn submit_handoff(&mut self, form: HandoffForm) {
        let Some((_, ts)) = self
            .workspaces
            .iter()
            .flat_map(|w| w.sessions.iter().map(move |s| (w, s)))
            .find(|(_, s)| s.uid == form.session_uid)
        else {
            self.set_status_msg("Session no longer in the sidebar");
            return;
        };
        let host = ts.host_id.clone();
        let Some(socket) = self.host_pool.for_host(&host).ok().and_then(|h| h.socket_path()) else {
            self.set_status_msg(&format!("Host `{}` not reachable", host.as_str()));
            return;
        };
        let token = self.host_pool.operator_token_for(&host);
        let params = form.rpc_params();
        let effect = form.effect();
        let uid = form.session_uid.clone();
        self.set_status_msg("Handing off…");
        self.run_in_background(Some(format!("handoff:{uid}")), move || {
            let result = crate::client_session::rpc_handoff(&socket, &token, params);
            Box::new(move |app: &mut App| match result {
                Ok(_) => app.set_status_msg(&format!("{effect}. The session restarts in place and says so in its first message.")),
                Err(e) => app.set_status_msg(&format!("Handoff refused: {e}")),
            })
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn the_form_says_what_will_happen_and_sends_only_changes() {
        let mut f = HandoffForm::new("ts-a", "lane", "claude", Some("claude-opus-5-5"), Some("xhigh"));
        assert!(f.effect().contains("rotate"));
        let p = f.rpc_params();
        assert_eq!((p["to_engine"].as_str(), p.get("to_model"), p["now"].as_bool()), (Some("claude"), None, Some(true)));
        // Another engine: a switch, prefilled from that engine's defaults.
        assert!(matches!(f.key(key(KeyCode::Right)), InputOutcome::Consumed));
        assert_eq!(f.to_engine, "codex");
        assert!(f.effect().starts_with("Switch"));
        assert_eq!(f.rpc_params()["to_engine"], "codex");
        // Back again: the recorded pick returns.
        f.key(key(KeyCode::Left));
        assert_eq!(f.to_engine, "claude");
        assert_eq!(f.model.text, "claude-opus-5-5");
        // A new effort on the same engine: a restart on it.
        f.key(key(KeyCode::Tab));
        f.key(KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT));
        assert!(f.effect().starts_with("Restart"), "{}", f.effect());
        assert_eq!(f.rpc_params()["to_effort"], "high");
        assert!(matches!(f.key(key(KeyCode::Enter)), InputOutcome::Submit(SubmitAction::Handoff)));
        assert!(matches!(f.key(key(KeyCode::Esc)), InputOutcome::Cancel));
    }
}
