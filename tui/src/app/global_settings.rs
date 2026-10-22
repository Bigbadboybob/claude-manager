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
    /// File-only (no F9 row): launch Codex with `--no-alt-screen` so its
    /// history reaches CM's scrollback. Applies to the next TUI-composed
    /// Codex spawn/A-R; restart the TUI after editing.
    codex_inline_scrollback: bool,
    /// Messages view conversation sidebar width in cells; 0 fits the
    /// longest name. Set with `<` / `>` in Messages.
    messages_sidebar_width: u16,
    /// The live activity badge on sidebar rows and org chart cards
    /// (`⚙ 3m`, `⧗ cargo test`, `⚠ rate limit`). The info pane always
    /// shows it.
    activity_badges: bool,
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
    fn global_settings_models_section_defaults_overrides_and_rejects() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.toml");
        let settings = GlobalSettings::load_from(path.clone());
        assert_eq!(settings.models(), &cm_daemon::models::ModelConfig::default());
        std::fs::write(&path, "[models]\ncodex = \"gpt-6-luna\"\n").unwrap();
        let settings = GlobalSettings::load_from(path.clone());
        assert_eq!(settings.models().codex, "gpt-6-luna");
        assert_eq!(settings.models().claude, cm_daemon::models::DEFAULT_CLAUDE_MODEL);
        assert!(settings.load_error.is_none());
        std::fs::write(&path, "[models]\nclaude = \"opus latest\"\n").unwrap();
        let settings = GlobalSettings::load_from(path.clone());
        assert_eq!(settings.models(), &cm_daemon::models::ModelConfig::default());
        assert!(settings.load_error.as_deref().unwrap().contains("models.claude"));
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

    /// The activity badge toggle: on by default, ↓ then Space flips it,
    /// Enter saves both rows, and a reload keeps it.
    #[test]
    fn activity_badges_toggle_saves_and_reloads() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.toml");
        let mut settings = GlobalSettings::load_from(path.clone());
        assert!(settings.activity_badges());
        settings.handle_event(&key(KeyCode::F(9)));
        settings.handle_event(&key(KeyCode::Down));
        settings.handle_event(&key(KeyCode::Char(' ')));
        assert_eq!(settings.tint_strength(), 2.0, "the badge row leaves the tint alone");
        settings.handle_event(&key(KeyCode::Up));
        settings.handle_event(&key(KeyCode::Right));
        settings.handle_event(&key(KeyCode::Enter));
        let reloaded = GlobalSettings::load_from(path.clone());
        assert!(!reloaded.activity_badges());
        assert_eq!(reloaded.tint_strength(), 2.25);
        std::fs::write(&path, "activity_badges = true\n").unwrap();
        assert!(GlobalSettings::load_from(path).activity_badges());
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
            codex_inline_scrollback: true,
            messages_sidebar_width: 0,
            activity_badges: true,
        }
    }
}

struct Dialog {
    draft: Preferences,
    error: Option<String>,
    /// The focused row: 0 section tint, 1 activity badges.
    row: u8,
}

pub(super) struct GlobalSettings {
    path: PathBuf,
    saved: Preferences,
    /// File-only `[models]` section (no F9 row). See `cm_daemon::models`.
    models: cm_daemon::models::ModelConfig,
    load_error: Option<String>,
    dialog: Option<Dialog>,
}

/// Parse and validate `[models]`; a missing section means the defaults.
fn load_models(path: &Path) -> anyhow::Result<cm_daemon::models::ModelConfig> {
    let models = match read_document(path)?.remove("models") {
        Some(value) => value.try_into().context("Invalid [models] section")?,
        None => cm_daemon::models::ModelConfig::default(),
    };
    models.validate().map_err(anyhow::Error::msg)?;
    Ok(models)
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
        let settings = Self::load_from(
            dirs::home_dir()
                .unwrap_or_default()
                .join(".cm/tui-settings.toml"),
        );
        // Process-wide, so only the production load sets it (tests use
        // `load_from` and must not flip other tests' argv).
        cm_daemon::mcp_config::set_codex_inline_scrollback(
            settings.saved.codex_inline_scrollback,
        );
        cm_daemon::models::set_models(settings.models.clone());
        settings
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
        let (saved, mut load_error) = match loaded {
            Ok(p) => (p, None),
            Err(e) => (Preferences::default(), Some(format!("{e:#}"))),
        };
        let models = match load_models(&path) {
            Ok(m) => m,
            Err(e) => {
                load_error.get_or_insert(format!("{e:#}"));
                cm_daemon::models::ModelConfig::default()
            }
        };
        Self {
            path,
            saved,
            models,
            load_error,
            dialog: None,
        }
    }

    /// The `[models]` section: engine default models for TUI-composed
    /// spawns and the Model field's presets.
    pub(super) fn models(&self) -> &cm_daemon::models::ModelConfig {
        &self.models
    }

    pub(super) fn tint_strength(&self) -> f64 {
        self.dialog
            .as_ref()
            .map_or(self.saved.sidebar_tint_strength, |d| {
                d.draft.sidebar_tint_strength
            })
    }

    /// Whether rows and cards show the activity badge (the draft while
    /// F9 is open, for a live preview).
    pub(super) fn activity_badges(&self) -> bool {
        self.dialog.as_ref().map_or(self.saved.activity_badges, |d| d.draft.activity_badges)
    }

    pub(super) fn is_open(&self) -> bool {
        self.dialog.is_some()
    }

    pub(super) fn messages_sidebar_width(&self) -> u16 {
        self.saved.messages_sidebar_width
    }

    /// Save the Messages sidebar width (0 = fit names) right away.
    pub(super) fn set_messages_sidebar_width(&mut self, width: u16) -> anyhow::Result<()> {
        self.write_key("messages_sidebar_width", toml::Value::Integer(width.into()))?;
        self.saved.messages_sidebar_width = width;
        Ok(())
    }

    fn save(&self, draft: Preferences) -> anyhow::Result<()> {
        self.write_keys(&[
            ("sidebar_tint_strength", toml::Value::Float(draft.sidebar_tint_strength)),
            ("activity_badges", toml::Value::Boolean(draft.activity_badges)),
        ])
    }

    fn write_key(&self, key: &str, value: toml::Value) -> anyhow::Result<()> {
        self.write_keys(&[(key, value)])
    }

    fn write_keys(&self, keys: &[(&str, toml::Value)]) -> anyhow::Result<()> {
        // Read on save to preserve unrelated keys, and refuse malformed files.
        let mut document = read_document(&self.path)?;
        for (key, value) in keys {
            document.insert((*key).into(), value.clone());
        }
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
                row: 0,
            });
            return true;
        }
        let dialog = self.dialog.as_mut().unwrap();
        match key.code {
            KeyCode::Up | KeyCode::Down | KeyCode::Tab | KeyCode::BackTab => {
                dialog.row = 1 - dialog.row;
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') | KeyCode::Char('-') | KeyCode::Char('+')
                if dialog.row == 1 =>
            {
                dialog.draft.activity_badges = !dialog.draft.activity_badges;
            }
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
        let height = (if dialog.error.is_some() { 19 } else { 15 }).min(area.height);
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
        let row_style = |focused: bool| {
            let s = Style::default().fg(theme::TEXT);
            if focused { s.add_modifier(Modifier::BOLD) } else { s }
        };
        let pointer = |focused: bool| if focused { "› " } else { "  " };
        let mut lines = vec![
            Line::from("All projects on this laptop."),
            Line::from(""),
            Line::styled(
                format!("{}Section tint:  ◀  {strength:.2}×  ▶", pointer(dialog.row == 0)),
                row_style(dialog.row == 0),
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
            Line::styled(
                format!(
                    "{}Activity badges:  [{}]",
                    pointer(dialog.row == 1),
                    if dialog.draft.activity_badges { "x" } else { " " }
                ),
                row_style(dialog.row == 1),
            ),
            Line::from("What each agent is doing, on rows and F6 cards."),
            Line::from(""),
            Line::from("Live preview · Opacity unchanged."),
            Line::from("↑/↓ row · Home reset · Enter save · Esc cancel"),
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
