//! The Model field shared by the launch forms (A-n, A-s, A-f, A-F).
//!
//! CM never leaves the model to the engine: every agent spawn names one.
//! The field opens prefilled with the task's default model (inherited
//! through its parents) or else the engine default from `[models]` in
//! `~/.cm/tui-settings.toml`. ←/→ cycle the configured presets; typing
//! edits it freely. Only one form is open at a time, so the field lives on
//! `App` (like the A-n Reports-to choice) rather than in each form.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::theme;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ModelField {
    /// The engine the field is for (`claude` / `codex`); empty = bash,
    /// which takes no model.
    engine: String,
    pub text: String,
    /// Where the prefill came from, shown dimmed until edited.
    origin: Option<&'static str>,
    /// The bound task's per-engine defaults (`claude` / `codex` → model),
    /// kept so an engine change re-prefills from the right one.
    task_defaults: std::collections::BTreeMap<String, String>,
}

impl ModelField {
    /// Prefill for `session_type` with no task: the engine default.
    pub(crate) fn new(session_type: &str) -> Self {
        Self::for_task(session_type, Default::default())
    }

    /// Prefill for `session_type`: the task's default for that engine when
    /// it has one, else the engine default.
    pub(crate) fn for_task(
        session_type: &str,
        task_defaults: std::collections::BTreeMap<String, String>,
    ) -> Self {
        let mut field = ModelField { task_defaults, ..Self::default() };
        field.retarget(session_type);
        field
    }

    /// Prefill with a session's recorded model (a fork keeps its source's).
    /// Untouched, it submits as "no explicit model" and the host reuses
    /// the recorded one.
    pub(crate) fn recorded(session_type: &str, model: Option<&str>) -> Self {
        let mut field = Self::new(session_type);
        if let Some(m) = model.filter(|_| field.takes_model()) {
            field.text = m.to_string();
            field.origin = Some("source session");
        }
        field
    }

    /// Re-prefill for another engine (the form's engine changed).
    pub(crate) fn retarget(&mut self, session_type: &str) {
        let Some(engine) = cm_daemon::models::engine_key(session_type) else {
            self.engine.clear();
            self.text.clear();
            self.origin = None;
            return;
        };
        let (text, origin) = match self.task_defaults.get(engine) {
            Some(m) => (m.clone(), "task default"),
            None => (cm_daemon::models::default_model(engine).unwrap_or_default(), "engine default"),
        };
        self.engine = engine.to_string();
        self.text = text;
        self.origin = Some(origin);
    }

    pub(crate) fn takes_model(&self) -> bool {
        !self.engine.is_empty()
    }

    /// Handle a key while the field is focused. Returns false for keys the
    /// form should handle itself (Tab, Enter, Esc, …).
    pub(crate) fn key(&mut self, key: KeyEvent) -> bool {
        if !self.takes_model() {
            return false;
        }
        let plain = !key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Left | KeyCode::Right => {
                self.cycle(key.code == KeyCode::Right);
                true
            }
            KeyCode::Backspace => {
                self.text.pop();
                self.origin = None;
                true
            }
            KeyCode::Char(c) if plain && !c.is_whitespace() => {
                self.text.push(c);
                self.origin = None;
                true
            }
            _ => false,
        }
    }

    /// Step through the configured presets (`[models] <engine>_choices`).
    pub(crate) fn cycle(&mut self, forward: bool) {
        let choices = cm_daemon::models::models().choices_for(&self.engine);
        if !self.takes_model() || choices.is_empty() {
            return;
        }
        let n = choices.len();
        let next = match choices.iter().position(|c| *c == self.text.trim()) {
            Some(i) if forward => (i + 1) % n,
            Some(i) => (i + n - 1) % n,
            None if forward => 0,
            None => n - 1,
        };
        self.text = choices[next].clone();
        self.origin = None;
    }

    /// The explicit model to send, validated. `Ok(None)` for bash and for
    /// an untouched or emptied field: the spawn then resolves the task
    /// default, else the engine default (what the prefill showed), so a
    /// parent's default still reaches a new subtask.
    pub(crate) fn value(&self) -> Result<Option<String>, String> {
        let text = self.text.trim();
        if !self.takes_model() || self.origin.is_some() || text.is_empty() {
            return Ok(None);
        }
        cm_daemon::models::validate_model(text)
            .map(Some)
            .map_err(|e| format!("Model: {e}"))
    }

    /// One form line: `   Model: gpt-6.1-sol  (engine default)`.
    pub(crate) fn line(&self, label: &'static str, active: bool) -> Line<'static> {
        let dim = Style::default().fg(theme::DIM);
        let white = Style::default().fg(theme::TEXT);
        let style = if active { white.add_modifier(Modifier::BOLD) } else { white };
        let mut spans = vec![Span::styled(label, dim)];
        if !self.takes_model() {
            spans.push(Span::styled("— (bash has no model)", dim));
            return Line::from(spans);
        }
        spans.push(Span::styled(super::input::sanitize_for_display(&self.text), style));
        if active {
            spans.push(Span::styled("\u{2588}", white));
            spans.push(Span::styled("  \u{2190}/\u{2192} presets \u{00b7} type to edit", dim));
        } else if let Some(origin) = self.origin {
            spans.push(Span::styled(format!("  ({origin})"), dim));
        }
        Line::from(spans)
    }
}

/// A-e on a task: the task's OWN default model per engine. Empty means it
/// sets none and inherits (from its parent, else the engine default).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TaskModelsEdit {
    /// `[claude, codex]`, as edited.
    pub text: [String; 2],
    orig: [String; 2],
    /// What an empty slot falls back to, for the hint.
    inherited: [String; 2],
}

pub(crate) const TASK_MODEL_ENGINES: [&str; 2] = ["claude", "codex"];

impl TaskModelsEdit {
    pub(crate) fn new(own: [Option<String>; 2], inherited: [Option<String>; 2]) -> Self {
        let text = own.map(Option::unwrap_or_default);
        let inherited = [0, 1].map(|i| {
            inherited[i]
                .clone()
                .or_else(|| cm_daemon::models::default_model(TASK_MODEL_ENGINES[i]))
                .unwrap_or_default()
        });
        TaskModelsEdit { orig: text.clone(), text, inherited }
    }

    /// Edit slot `i` (0 = claude, 1 = codex). ←/→ cycle "inherit" plus the
    /// presets; typing edits; Backspace deletes.
    pub(crate) fn key(&mut self, i: usize, key: KeyEvent) -> bool {
        let plain = !key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL);
        let slot = &mut self.text[i];
        match key.code {
            KeyCode::Left | KeyCode::Right => {
                let mut options = vec![String::new()];
                options.extend(cm_daemon::models::models().choices_for(TASK_MODEL_ENGINES[i]));
                let n = options.len();
                let at = options.iter().position(|o| o == slot.trim()).unwrap_or(0);
                let next = if key.code == KeyCode::Right { (at + 1) % n } else { (at + n - 1) % n };
                *slot = options[next].clone();
                true
            }
            KeyCode::Backspace => {
                slot.pop();
                true
            }
            KeyCode::Char(c) if plain && !c.is_whitespace() => {
                slot.push(c);
                true
            }
            _ => false,
        }
    }

    /// The changed slots as `(engine, Some(model) | None = clear)`.
    pub(crate) fn changes(&self) -> Result<Vec<(String, Option<String>)>, String> {
        let mut out = Vec::new();
        for i in 0..2 {
            let now = self.text[i].trim();
            if now == self.orig[i].trim() {
                continue;
            }
            let model = if now.is_empty() {
                None
            } else {
                Some(cm_daemon::models::validate_model(now).map_err(|e| {
                    format!("{} model: {e}", TASK_MODEL_ENGINES[i])
                })?)
            };
            out.push((TASK_MODEL_ENGINES[i].to_string(), model));
        }
        Ok(out)
    }

    pub(crate) fn line(&self, i: usize, label: &'static str, active: bool) -> Line<'static> {
        let dim = Style::default().fg(theme::DIM);
        let white = Style::default().fg(theme::TEXT);
        let style = if active { white.add_modifier(Modifier::BOLD) } else { white };
        let mut spans = vec![Span::styled(label, dim)];
        if self.text[i].is_empty() {
            spans.push(Span::styled(
                format!("inherit ({})", super::input::sanitize_for_display(&self.inherited[i])),
                if active { style } else { dim },
            ));
        } else {
            spans.push(Span::styled(super::input::sanitize_for_display(&self.text[i]), style));
        }
        if active {
            spans.push(Span::styled("\u{2588}", white));
            spans.push(Span::styled("  \u{2190}/\u{2192} presets \u{00b7} type \u{00b7} empty inherits", dim));
        }
        Line::from(spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn model_field_prefills_cycles_edits_and_validates() {
        let f = ModelField::new("codex");
        assert_eq!(f.text, cm_daemon::models::DEFAULT_CODEX_MODEL);
        assert_eq!(f.value(), Ok(None), "an untouched prefill leaves resolution to the spawn");
        let defaults = [("claude".to_string(), "claude-sonnet-5-5".to_string())].into_iter().collect();
        let mut t = ModelField::for_task("claude-code", defaults);
        assert_eq!(t.text, "claude-sonnet-5-5");
        t.retarget("codex");
        assert_eq!(t.text, cm_daemon::models::DEFAULT_CODEX_MODEL, "no codex task default");
        t.retarget("claude");
        assert_eq!(t.text, "claude-sonnet-5-5");
        t.retarget("bash");
        assert_eq!(t.value(), Ok(None));
        assert!(!t.takes_model());

        let mut f = ModelField::new("codex");
        let choices = cm_daemon::models::models().choices_for("codex");
        assert!(f.key(k(KeyCode::Right)));
        assert_eq!(f.text, choices[1 % choices.len()]);
        assert_eq!(f.value(), Ok(Some(choices[1 % choices.len()].clone())));
        assert!(f.key(k(KeyCode::Left)));
        assert_eq!(f.text, choices[0]);

        f.text.clear();
        for c in "gpt-6-luna".chars() {
            assert!(f.key(k(KeyCode::Char(c))));
        }
        assert_eq!(f.value(), Ok(Some("gpt-6-luna".into())));
        assert!(!f.key(k(KeyCode::Tab)), "the form keeps Tab");
        f.text = "bad model".into();
        assert!(f.value().is_err());
        f.text.clear();
        assert_eq!(f.value(), Ok(None));
    }

    #[test]
    fn task_models_edit_reports_only_changed_slots() {
        let mut e = TaskModelsEdit::new([Some("claude-sonnet-5-5".into()), None], [None, Some("gpt-6-luna".into())]);
        assert_eq!(e.changes(), Ok(vec![]));
        e.text[0].clear();
        for c in "gpt-6-luna".chars() {
            assert!(e.key(1, k(KeyCode::Char(c))));
        }
        assert_eq!(
            e.changes(),
            Ok(vec![("claude".into(), None), ("codex".into(), Some("gpt-6-luna".into()))])
        );
        assert!(e.key(1, k(KeyCode::Right)), "cycling past the end returns to inherit");
        e.text[1] = "a b".into();
        assert!(e.changes().is_err());
    }
}
