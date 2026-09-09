//! Laptop-wide viewer preferences, separate from session/daemon state.

use super::*;
use anyhow::Context;
use serde::Deserialize;

const DEFAULT_TINT: f64 = 2.0;
const MAX_TINT: f64 = 5.0;

#[derive(Clone, Copy, Deserialize)]
#[serde(default)]
struct Preferences {
    sidebar_tint_strength: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> CrosstermEvent {
        CrosstermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn global_settings_defaults_and_rejects_invalid_values_without_rewriting() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.toml");
        assert_eq!(GlobalSettings::load_from(path.clone()).tint_strength(), 2.0);
        assert!(!path.exists());
        for text in [
            "sidebar_tint_strength = -1.0",
            "sidebar_tint_strength = 6.0",
            "sidebar_tint_strength = nan",
            "sidebar_tint_strength = 'bad'",
            "broken = [",
        ] {
            std::fs::write(&path, text).unwrap();
            let settings = GlobalSettings::load_from(path.clone());
            assert_eq!(settings.tint_strength(), 2.0);
            assert!(settings.load_error.is_some(), "{text}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        }
    }

    #[test]
    fn global_settings_preview_cancel_save_reload_and_preserve_other_keys() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.toml");
        std::fs::write(
            &path,
            "sidebar_tint_strength = 1.5\n[future]\nvalue = true\n",
        )
        .unwrap();
        let mut settings = GlobalSettings::load_from(path.clone());
        settings.handle_event(&key(KeyCode::F(9)));
        settings.handle_event(&key(KeyCode::Right));
        assert_eq!(settings.tint_strength(), 1.75);
        settings.handle_event(&key(KeyCode::Esc));
        assert_eq!(settings.tint_strength(), 1.5);
        assert_eq!(GlobalSettings::load_from(path.clone()).tint_strength(), 1.5);
        settings.handle_event(&key(KeyCode::F(9)));
        settings.handle_event(&key(KeyCode::Right));
        settings.handle_event(&key(KeyCode::Enter));
        assert!(!settings.is_open());
        assert_eq!(
            GlobalSettings::load_from(path.clone()).tint_strength(),
            1.75
        );
        assert_eq!(
            read_document(&path).unwrap()["future"]["value"].as_bool(),
            Some(true)
        );
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
    }

    #[test]
    fn global_settings_save_failure_keeps_dialog_and_original_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.toml");
        let mut settings = GlobalSettings::load_from(path.clone());
        settings.handle_event(&key(KeyCode::F(9)));
        settings.handle_event(&key(KeyCode::Left));
        std::fs::write(&path, "broken = [").unwrap();
        settings.handle_event(&key(KeyCode::Enter));
        assert!(settings.dialog.as_ref().unwrap().error.is_some());
        assert_eq!(settings.tint_strength(), 1.75);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "broken = [");
        settings.handle_event(&key(KeyCode::Esc));
        assert_eq!(settings.tint_strength(), 2.0);
    }

    #[test]
    fn global_settings_adjustments_are_bounded_and_home_restores_default() {
        let temp = tempfile::tempdir().unwrap();
        let mut settings = GlobalSettings::load_from(temp.path().join("settings.toml"));
        settings.handle_event(&key(KeyCode::F(9)));
        for _ in 0..30 {
            settings.handle_event(&key(KeyCode::Left));
        }
        assert_eq!(settings.tint_strength(), 0.0);
        for _ in 0..30 {
            settings.handle_event(&key(KeyCode::Right));
        }
        assert_eq!(settings.tint_strength(), MAX_TINT);
        settings.handle_event(&key(KeyCode::Home));
        assert_eq!(settings.tint_strength(), DEFAULT_TINT);
        assert!(
            settings.handle_event(&key(KeyCode::Char('q'))),
            "modal keys must be consumed"
        );
    }
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            sidebar_tint_strength: DEFAULT_TINT,
        }
    }
}

struct Dialog {
    draft: Preferences,
    error: Option<String>,
}

pub(super) struct GlobalSettings {
    path: PathBuf,
    saved: Preferences,
    load_error: Option<String>,
    dialog: Option<Dialog>,
}

fn read_document(path: &Path) -> anyhow::Result<toml::Table> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(toml::from_str(&text).context("Invalid settings TOML")?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(toml::Table::new()),
        Err(e) => Err(e).context("Cannot read settings"),
    }
}

impl GlobalSettings {
    pub(super) fn load() -> Self {
        Self::load_from(
            dirs::home_dir()
                .unwrap_or_default()
                .join(".cm/tui-settings.toml"),
        )
    }

    fn load_from(path: PathBuf) -> Self {
        let loaded = read_document(&path).and_then(|document| {
            let preferences: Preferences = toml::Value::Table(document).try_into()?;
            anyhow::ensure!(
                preferences.sidebar_tint_strength.is_finite()
                    && (0.0..=MAX_TINT).contains(&preferences.sidebar_tint_strength),
                "sidebar_tint_strength must be between 0 and 5"
            );
            Ok(preferences)
        });
        let (saved, load_error) = match loaded {
            Ok(p) => (p, None),
            Err(e) => (Preferences::default(), Some(format!("{e:#}"))),
        };
        Self {
            path,
            saved,
            load_error,
            dialog: None,
        }
    }

    pub(super) fn tint_strength(&self) -> f64 {
        self.dialog
            .as_ref()
            .map_or(self.saved.sidebar_tint_strength, |d| {
                d.draft.sidebar_tint_strength
            })
    }

    pub(super) fn is_open(&self) -> bool {
        self.dialog.is_some()
    }

    fn save(&self, draft: Preferences) -> anyhow::Result<()> {
        // Read on save to preserve unrelated keys, and refuse malformed files.
        let mut document = read_document(&self.path)?;
        document.insert(
            "sidebar_tint_strength".into(),
            toml::Value::Float(draft.sidebar_tint_strength),
        );
        let text = toml::to_string_pretty(&document)?;
        let parent = self.path.parent().context("Settings path has no parent")?;
        std::fs::create_dir_all(parent)?;
        let temporary = parent.join(format!(".tui-settings-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> std::io::Result<()> {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&temporary, &self.path)
        })();
        let _ = std::fs::remove_file(&temporary);
        result.context("Cannot save settings")
    }

    /// Consume every event while open so preview keys never reach a session.
    pub(super) fn handle_event(&mut self, event: &CrosstermEvent) -> bool {
        let CrosstermEvent::Key(key) = event else {
            return self.is_open();
        };
        if self.dialog.is_none() {
            if key.code != KeyCode::F(9) || !key.modifiers.is_empty() {
                return false;
            }
            self.dialog = Some(Dialog {
                draft: self.saved,
                error: self.load_error.clone(),
            });
            return true;
        }
        let dialog = self.dialog.as_mut().unwrap();
        match key.code {
            KeyCode::Left | KeyCode::Char('-') => {
                dialog.draft.sidebar_tint_strength =
                    (dialog.draft.sidebar_tint_strength - 0.25).max(0.0);
            }
            KeyCode::Right | KeyCode::Char('+') => {
                dialog.draft.sidebar_tint_strength =
                    (dialog.draft.sidebar_tint_strength + 0.25).min(MAX_TINT);
            }
            KeyCode::Home => dialog.draft = Preferences::default(),
            KeyCode::Esc => self.dialog = None,
            KeyCode::Enter => {
                let draft = dialog.draft;
                match self.save(draft) {
                    Ok(()) => {
                        self.saved = draft;
                        self.load_error = None;
                        self.dialog = None;
                    }
                    Err(e) => self.dialog.as_mut().unwrap().error = Some(format!("{e:#}")),
                }
            }
            _ => {}
        }
        true
    }

    pub(super) fn draw(&self, frame: &mut Frame, area: Rect) {
        let Some(dialog) = &self.dialog else {
            return;
        };
        let width = 60.min(area.width);
        let height = (if dialog.error.is_some() { 16 } else { 12 }).min(area.height);
        let area = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Global Settings ")
            .border_style(Style::default().fg(theme::TEXT));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let strength = dialog.draft.sidebar_tint_strength;
        let mut lines = vec![
            Line::from("All projects on this laptop."),
            Line::from(""),
            Line::styled(
                format!("Section tint:  ◀  {strength:.2}×  ▶"),
                Style::default()
                    .fg(theme::TEXT)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::from("←/→ 0.25 steps · 0 off · Default 2×"),
            Line::from(""),
            Line::from(
                vec!["neutral", "blue", "green", "pink"]
                    .into_iter()
                    .map(|name| {
                        Span::styled(
                            format!(" {name} "),
                            Style::default()
                                .fg(theme::TEXT)
                                .bg(theme::sidebar_section_bg(Some(name), strength)),
                        )
                    })
                    .collect::<Vec<_>>(),
            ),
            Line::from(""),
            Line::from("Live preview · Opacity unchanged."),
            Line::from("Home reset · Enter save · Esc cancel"),
        ];
        if let Some(error) = &dialog.error {
            lines.push(Line::styled(
                error.clone(),
                Style::default().fg(theme::ERROR),
            ));
        }
        frame.render_widget(
            Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }),
            inner,
        );
    }
}
