//! Explicit model choice for every agent spawn.
//!
//! CM never leaves a claude or codex session on its engine's own default
//! model. Every agent argv carries the model flag (`claude --model <id>`,
//! `codex -m <id>`), and `start_session` refuses an agent argv without one.
//! Bash sessions have no model.
//!
//! Resolution order: an explicit choice (MCP `model=`, a TUI form's Model
//! field) → the task's default (`metadata.models.<engine>`, inherited from
//! the nearest ancestor that sets it) → the engine default from config
//! (`[models]` in `daemon.toml`, or `~/.cm/tui-settings.toml` for
//! TUI-composed spawns). The resolved model is recorded on the session so
//! revive, resume and restore reuse it.
//!
//! Model strings are opaque: CM checks only that one is non-empty and safe
//! to put in argv. An unknown model fails in the engine.

use std::sync::RwLock;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Claude default: the full current Opus id, never an alias.
pub const DEFAULT_CLAUDE_MODEL: &str = "claude-opus-5-5";
/// Codex default: the "sol" workhorse codex-lb serves.
pub const DEFAULT_CODEX_MODEL: &str = "gpt-6.1-sol";

/// `tasks.metadata` key holding a task's per-engine default models:
/// `{"claude": "<id>", "codex": "<id>"}`. Subtasks inherit it.
pub const TASK_MODELS_METADATA_KEY: &str = "models";

/// Longest accepted model string.
pub const MAX_MODEL_LEN: usize = 128;

/// `[models]`: the engine defaults and the presets the TUI's Model field
/// cycles through. Free text is accepted too; the presets are a shortlist.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelConfig {
    /// Default for claude sessions.
    pub claude: String,
    /// Default for codex sessions.
    pub codex: String,
    /// Presets offered for claude sessions.
    pub claude_choices: Vec<String>,
    /// Presets offered for codex sessions.
    pub codex_choices: Vec<String>,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            claude: DEFAULT_CLAUDE_MODEL.into(),
            codex: DEFAULT_CODEX_MODEL.into(),
            claude_choices: vec![
                DEFAULT_CLAUDE_MODEL.into(),
                "claude-sonnet-5-5".into(),
                "claude-haiku-5-5".into(),
            ],
            codex_choices: vec![
                DEFAULT_CODEX_MODEL.into(),
                "gpt-6-luna".into(),
                "gpt-6-astra".into(),
            ],
        }
    }
}

impl ModelConfig {
    /// Every default and preset must be a valid model string; a bad
    /// config fails its load rather than a later spawn.
    pub fn validate(&self) -> Result<(), String> {
        for (key, value) in [("models.claude", &self.claude), ("models.codex", &self.codex)] {
            validate_model(value).map_err(|e| format!("{key}: {e}"))?;
        }
        for (key, list) in [
            ("models.claude_choices", &self.claude_choices),
            ("models.codex_choices", &self.codex_choices),
        ] {
            for value in list {
                validate_model(value).map_err(|e| format!("{key}: {e}"))?;
            }
        }
        Ok(())
    }

    /// The configured default for `engine` (`None` for bash).
    pub fn default_for(&self, engine: &str) -> Option<&str> {
        match engine_key(engine)? {
            "claude" => Some(self.claude.as_str()),
            _ => Some(self.codex.as_str()),
        }
    }

    /// Presets for `engine`, the default first and without duplicates.
    pub fn choices_for(&self, engine: &str) -> Vec<String> {
        let (default, list) = match engine_key(engine) {
            Some("claude") => (&self.claude, &self.claude_choices),
            Some(_) => (&self.codex, &self.codex_choices),
            None => return Vec::new(),
        };
        let mut out = vec![default.clone()];
        for m in list {
            if !out.contains(m) {
                out.push(m.clone());
            }
        }
        out
    }
}

static MODELS: RwLock<Option<ModelConfig>> = RwLock::new(None);

/// Install the process-wide model config. The daemon sets it from
/// `daemon.toml` (startup and `reload_config`), the TUI from
/// `~/.cm/tui-settings.toml`. Unset means [`ModelConfig::default`].
pub fn set_models(cfg: ModelConfig) {
    *MODELS.write().unwrap_or_else(|p| p.into_inner()) = Some(cfg);
}

/// The process-wide model config.
pub fn models() -> ModelConfig {
    MODELS
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
        .unwrap_or_default()
}

/// The engine default from the process-wide config (`None` for bash).
pub fn default_model(engine: &str) -> Option<String> {
    models().default_for(engine).map(str::to_string)
}

/// The metadata/config slot for a session type: `claude` for
/// `claude-code` (or `claude`), `codex` for `codex`, `None` for anything
/// without a model (bash).
pub fn engine_key(engine: &str) -> Option<&'static str> {
    match engine.trim() {
        "claude-code" | "claude" => Some("claude"),
        "codex" => Some("codex"),
        _ => None,
    }
}

/// Whether `engine` takes a model (claude and codex do; bash doesn't).
pub fn takes_model(engine: &str) -> bool {
    engine_key(engine).is_some()
}

/// Trim and check a model string: non-empty, at most [`MAX_MODEL_LEN`]
/// characters, starts with a letter or digit (so it can't read as a
/// flag), and contains only `A-Z a-z 0-9 . _ - : / @ + [ ]`. That keeps
/// it safe both as an argv word and inside the TOML string codex's
/// backend override quotes it in. Nothing else is assumed about it.
pub fn validate_model(model: &str) -> Result<String, String> {
    let m = model.trim();
    if m.is_empty() {
        return Err("model must be a non-empty string".into());
    }
    if m.chars().count() > MAX_MODEL_LEN {
        return Err(format!("model is longer than {MAX_MODEL_LEN} characters"));
    }
    if !m.chars().next().is_some_and(|c| c.is_ascii_alphanumeric()) {
        return Err(format!("model '{m}' must start with a letter or digit"));
    }
    if let Some(bad) = m
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || "._-:/@+[]".contains(*c)))
    {
        return Err(format!("model '{}' contains '{}'", m.escape_debug(), bad.escape_debug()));
    }
    Ok(m.to_string())
}

/// The argv words that select `model` for `engine` (empty for bash).
pub fn model_args(engine: &str, model: &str) -> Vec<String> {
    match engine_key(engine) {
        Some("claude") => vec!["--model".into(), model.into()],
        Some(_) => vec!["-m".into(), model.into()],
        None => Vec::new(),
    }
}

/// The model an agent argv selects, if any: the word after `--model`
/// (claude) or `-m` / `--model` (codex), or a `--model=<id>` word.
/// The codex argv can be wrapped (systemd-run, the native launcher), so
/// the pair is looked for anywhere in it.
pub fn argv_model<'a>(engine: &str, argv: &'a [String]) -> Option<&'a str> {
    let flags: &[&str] = match engine_key(engine)? {
        "claude" => &["--model"],
        _ => &["-m", "--model"],
    };
    argv.iter().enumerate().find_map(|(i, a)| {
        if flags.contains(&a.as_str()) {
            argv.get(i + 1).map(String::as_str)
        } else {
            a.strip_prefix("--model=")
        }
    })
}

/// The task's own default for `engine` from its metadata bag.
pub fn task_model<'a>(metadata: &'a Value, engine: &str) -> Option<&'a str> {
    metadata
        .get(TASK_MODELS_METADATA_KEY)?
        .get(engine_key(engine)?)?
        .as_str()
        .map(str::trim)
        .filter(|m| !m.is_empty())
}

/// Where a session's model came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelSource {
    /// Named by the caller (MCP `model=`, a TUI Model field).
    Explicit,
    /// The task's (or an ancestor's) default.
    Task,
    /// Recorded on the session being revived, resumed or restored.
    Recorded,
    /// The engine default from config.
    Engine,
}

impl ModelSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ModelSource::Explicit => "explicit",
            ModelSource::Task => "task",
            ModelSource::Recorded => "recorded",
            ModelSource::Engine => "engine_default",
        }
    }
}

/// Apply the resolution order for one spawn: `explicit` → `inherited`
/// (a recorded model for a respawn, or the task default) → the engine
/// default. Returns `Ok(None)` for bash. An invalid explicit model is an
/// error; an invalid inherited one (a hand-edited row) falls through to
/// the next step so it can't wedge every spawn on that task.
pub fn resolve(
    engine: &str,
    explicit: Option<&str>,
    inherited: Option<(&str, ModelSource)>,
) -> Result<Option<(String, ModelSource)>, String> {
    if !takes_model(engine) {
        if explicit.is_some_and(|m| !m.trim().is_empty()) {
            return Err(format!("model applies to claude and codex sessions, not '{engine}'"));
        }
        return Ok(None);
    }
    if let Some(m) = explicit.filter(|m| !m.trim().is_empty()) {
        return validate_model(m).map(|m| Some((m, ModelSource::Explicit)));
    }
    if let Some((m, source)) = inherited {
        if let Ok(m) = validate_model(m) {
            return Ok(Some((m, source)));
        }
    }
    let m = default_model(engine).expect("agent engine has a default");
    validate_model(&m).map(|m| Some((m, ModelSource::Engine)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn v(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn validate_accepts_ids_and_rejects_argv_hazards() {
        for ok in ["claude-opus-5-5", "gpt-6.1-sol", "claude-opus-5-5[1m]", "org/model:v2@x+y", " gpt-6-luna "] {
            assert_eq!(validate_model(ok).unwrap(), ok.trim(), "{ok}");
        }
        for bad in ["", "   ", "-m", "--model", "a b", "a\"b", "a\\b", "a\nb", "a'b", "a;b", "a$b"] {
            assert!(validate_model(bad).is_err(), "{bad:?}");
        }
        assert!(validate_model(&"a".repeat(MAX_MODEL_LEN)).is_ok());
        assert!(validate_model(&"a".repeat(MAX_MODEL_LEN + 1)).is_err());
    }

    #[test]
    fn model_args_and_argv_model_round_trip() {
        assert_eq!(model_args("claude-code", "m1"), v(&["--model", "m1"]));
        assert_eq!(model_args("codex", "m2"), v(&["-m", "m2"]));
        assert!(model_args("bash", "m3").is_empty());
        let claude = v(&["claude", "--dangerously-skip-permissions", "--model", "m1"]);
        assert_eq!(argv_model("claude-code", &claude), Some("m1"));
        let wrapped = v(&["python3", "native_codex.py", "--", "resume", "-m", "m2", "sid"]);
        assert_eq!(argv_model("codex", &wrapped), Some("m2"));
        assert_eq!(argv_model("codex", &v(&["codex", "--model=m4"])), Some("m4"));
        assert_eq!(argv_model("claude-code", &v(&["claude", "-m", "x"])), None, "-m is not claude's flag");
        assert_eq!(argv_model("bash", &v(&["bash", "--model", "x"])), None);
    }

    #[test]
    fn task_model_reads_the_engine_slot() {
        let meta = json!({"models": {"claude": "c1", "codex": " x1 "}});
        assert_eq!(task_model(&meta, "claude-code"), Some("c1"));
        assert_eq!(task_model(&meta, "codex"), Some("x1"));
        assert_eq!(task_model(&meta, "bash"), None);
        assert_eq!(task_model(&json!({"models": {"codex": ""}}), "codex"), None);
        assert_eq!(task_model(&json!({}), "codex"), None);
        assert_eq!(task_model(&Value::Null, "codex"), None);
    }

    /// Pins the resolution order. Uses the built-in defaults: no test
    /// calls `set_models`.
    #[test]
    fn resolution_order_is_explicit_then_inherited_then_engine() {
        let r = |engine, explicit, inherited| resolve(engine, explicit, inherited).unwrap();
        assert_eq!(r("bash", None, None), None);
        assert!(resolve("bash", Some("m"), None).is_err(), "bash takes no model");
        assert_eq!(
            r("codex", Some("gpt-6-luna"), Some(("task-m", ModelSource::Task))),
            Some(("gpt-6-luna".into(), ModelSource::Explicit))
        );
        assert_eq!(
            r("codex", None, Some(("task-m", ModelSource::Task))),
            Some(("task-m".into(), ModelSource::Task))
        );
        assert_eq!(
            r("claude-code", Some("  "), Some(("rec", ModelSource::Recorded))),
            Some(("rec".into(), ModelSource::Recorded))
        );
        assert_eq!(r("claude-code", None, None), Some((DEFAULT_CLAUDE_MODEL.into(), ModelSource::Engine)));
        assert_eq!(r("codex", None, None), Some((DEFAULT_CODEX_MODEL.into(), ModelSource::Engine)));
        // A bad inherited value falls through; a bad explicit one is an error.
        assert_eq!(r("codex", None, Some(("bad model", ModelSource::Task))).unwrap().1, ModelSource::Engine);
        assert!(resolve("codex", Some("bad model"), None).is_err());
    }

    #[test]
    fn config_defaults_presets_and_validation() {
        let cfg = ModelConfig::default();
        cfg.validate().unwrap();
        assert_eq!(cfg.default_for("claude-code"), Some(DEFAULT_CLAUDE_MODEL));
        assert_eq!(cfg.default_for("codex"), Some(DEFAULT_CODEX_MODEL));
        assert_eq!(cfg.default_for("bash"), None);
        assert!(cfg.choices_for("codex").contains(&"gpt-6-luna".to_string()));
        let custom: ModelConfig = toml::from_str("codex = \"gpt-6-luna\"").unwrap();
        assert_eq!(custom.choices_for("codex")[0], "gpt-6-luna", "the default leads the presets");
        assert_eq!(custom.claude, DEFAULT_CLAUDE_MODEL, "unset keys keep their defaults");
        let bad: ModelConfig = toml::from_str("claude = \"opus latest\"").unwrap();
        assert!(bad.validate().unwrap_err().contains("models.claude"));
    }
}
