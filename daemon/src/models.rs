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
//!
//! Reasoning effort works the same way, alongside the model: every agent
//! argv carries it (`claude --effort <level>`, `codex -c
//! model_reasoning_effort="<level>"`), resolved explicit → the task's
//! `metadata.efforts.<engine>` → `[models] claude_effort` / `codex_effort`
//! (default `xhigh`), and recorded on the session as `effort` /
//! `effort_source`. CM's levels are `low`, `medium`, `high`, `xhigh` and
//! `max`; both engines take the same words, and codex has no `max`.

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

/// `tasks.metadata` key holding a task's per-engine default reasoning
/// efforts: `{"claude": "<level>", "codex": "<level>"}`. Subtasks inherit it.
pub const TASK_EFFORTS_METADATA_KEY: &str = "efforts";

/// The default reasoning effort for both engines.
pub const DEFAULT_EFFORT: &str = "xhigh";

/// CM's effort levels, lowest first. Claude takes all of them; codex
/// stops at `xhigh`.
pub const EFFORT_LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

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
    /// Default reasoning effort for claude sessions.
    pub claude_effort: String,
    /// Default reasoning effort for codex sessions.
    pub codex_effort: String,
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
            claude_effort: DEFAULT_EFFORT.into(),
            codex_effort: DEFAULT_EFFORT.into(),
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
        validate_effort("claude", &self.claude_effort).map_err(|e| format!("models.claude_effort: {e}"))?;
        validate_effort("codex", &self.codex_effort).map_err(|e| format!("models.codex_effort: {e}"))?;
        Ok(())
    }

    /// The configured default effort for `engine` (`None` for bash).
    pub fn effort_for(&self, engine: &str) -> Option<&str> {
        match engine_key(engine)? {
            "claude" => Some(self.claude_effort.as_str()),
            _ => Some(self.codex_effort.as_str()),
        }
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

/// The default effort from the process-wide config (`None` for bash).
pub fn default_effort(engine: &str) -> Option<String> {
    models().effort_for(engine).map(str::to_string)
}

/// The effort levels `engine` accepts, lowest first (empty for bash).
pub fn effort_levels(engine: &str) -> &'static [&'static str] {
    match engine_key(engine) {
        Some("claude") => &EFFORT_LEVELS,
        Some(_) => &EFFORT_LEVELS[..4],
        None => &[],
    }
}

/// Trim, lowercase and check an effort level for `engine`.
pub fn validate_effort(engine: &str, effort: &str) -> Result<String, String> {
    let e = effort.trim().to_ascii_lowercase();
    if e.is_empty() {
        return Err("effort must be a non-empty string".into());
    }
    let levels = effort_levels(engine);
    if levels.contains(&e.as_str()) {
        return Ok(e);
    }
    if EFFORT_LEVELS.contains(&e.as_str()) {
        return Err(format!("effort '{e}' is not available for {engine}; use one of {}", levels.join(", ")));
    }
    Err(format!("effort '{}' is not one of {}", e.escape_debug(), EFFORT_LEVELS.join(", ")))
}

/// The argv words that select `effort` for `engine` (empty for bash).
/// Codex takes it as a config override; the value is TOML-quoted.
pub fn effort_args(engine: &str, effort: &str) -> Vec<String> {
    match engine_key(engine) {
        Some("claude") => vec!["--effort".into(), effort.into()],
        Some(_) => vec!["-c".into(), format!("model_reasoning_effort=\"{effort}\"")],
        None => Vec::new(),
    }
}

/// The effort an agent argv selects, if any: the word after `--effort`
/// (or `--effort=<level>`) for claude, the `model_reasoning_effort`
/// override after a `-c` / `--config` for codex (quoted or bare).
pub fn argv_effort(engine: &str, argv: &[String]) -> Option<String> {
    match engine_key(engine)? {
        "claude" => argv.iter().enumerate().find_map(|(i, a)| {
            if a == "--effort" {
                argv.get(i + 1).cloned()
            } else {
                a.strip_prefix("--effort=").map(str::to_string)
            }
        }),
        _ => argv.iter().enumerate().find_map(|(i, a)| {
            let kv = if a == "-c" || a == "--config" {
                argv.get(i + 1)?.as_str()
            } else {
                a.strip_prefix("--config=")?
            };
            let v = kv.strip_prefix("model_reasoning_effort=")?.trim();
            Some(v.trim_matches(|c| c == '"' || c == '\'').to_string())
        }),
    }
}

/// The task's own default effort for `engine` from its metadata bag.
pub fn task_effort<'a>(metadata: &'a Value, engine: &str) -> Option<&'a str> {
    metadata
        .get(TASK_EFFORTS_METADATA_KEY)?
        .get(engine_key(engine)?)?
        .as_str()
        .map(str::trim)
        .filter(|m| !m.is_empty())
}

/// [`resolve`] for the effort: `explicit` → `inherited` → the config
/// default. `Ok(None)` for bash.
pub fn resolve_effort(
    engine: &str,
    explicit: Option<&str>,
    inherited: Option<(&str, ModelSource)>,
) -> Result<Option<(String, ModelSource)>, String> {
    if !takes_model(engine) {
        if explicit.is_some_and(|e| !e.trim().is_empty()) {
            return Err(format!("effort applies to claude and codex sessions, not '{engine}'"));
        }
        return Ok(None);
    }
    if let Some(e) = explicit.filter(|e| !e.trim().is_empty()) {
        return validate_effort(engine, e).map(|e| Some((e, ModelSource::Explicit)));
    }
    let mut fallback = ModelSource::Engine;
    if let Some((e, source)) = inherited {
        match validate_effort(engine, e) {
            Ok(e) => return Ok(Some((e, source))),
            Err(_) => fallback = invalid_inherited(source),
        }
    }
    let e = default_effort(engine).expect("agent engine has a default effort");
    validate_effort(engine, &e).map(|e| Some((e, fallback)))
}

/// The two per-session agent settings CM resolves and records the same
/// way: the model and the reasoning effort.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setting {
    Model,
    Effort,
}

impl Setting {
    pub fn name(self) -> &'static str {
        match self {
            Setting::Model => "model",
            Setting::Effort => "effort",
        }
    }

    /// [`resolve`] or [`resolve_effort`].
    pub fn resolve(
        self,
        engine: &str,
        explicit: Option<&str>,
        inherited: Option<(&str, ModelSource)>,
    ) -> Result<Option<(String, ModelSource)>, String> {
        match self {
            Setting::Model => resolve(engine, explicit, inherited),
            Setting::Effort => resolve_effort(engine, explicit, inherited),
        }
    }

    /// [`validate_model`] or [`validate_effort`].
    pub fn validate(self, engine: &str, value: &str) -> Result<String, String> {
        match self {
            Setting::Model => validate_model(value),
            Setting::Effort => validate_effort(engine, value),
        }
    }

    /// [`transcript_model`] or [`transcript_effort`].
    pub fn from_transcript(self, engine: &str, path: &std::path::Path) -> Option<String> {
        match self {
            Setting::Model => transcript_model(engine, path),
            Setting::Effort => transcript_effort(engine, path),
        }
    }
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
    /// Read from the conversation's transcript: a session recorded before
    /// CM recorded models keeps the model it actually ran.
    Transcript,
    /// A resume or revive of a conversation with no recorded model and no
    /// model in its transcript: the task or config default, logged.
    DefaultOnResume,
    /// The task's default is not valid for this engine (a codex task
    /// default of `max` effort): the config default, logged.
    TaskInvalidFallback,
    /// The task default lookup missed the restore's time budget: the
    /// config default for now. Provisional: the next respawn looks again.
    LookupTimeout,
}

impl ModelSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ModelSource::Explicit => "explicit",
            ModelSource::Task => "task",
            ModelSource::Recorded => "recorded",
            ModelSource::Engine => "engine_default",
            ModelSource::Transcript => "transcript",
            ModelSource::DefaultOnResume => "default_on_resume",
            ModelSource::TaskInvalidFallback => "task_invalid_fallback",
            ModelSource::LookupTimeout => "lookup_timeout",
        }
    }

    /// A stand-in recorded only because the real answer wasn't available
    /// yet; a respawn resolves again instead of reusing it.
    pub fn is_provisional(self) -> bool {
        self == ModelSource::LookupTimeout
    }

    /// The inverse of [`ModelSource::as_str`], for persisted sources.
    pub fn parse(s: &str) -> Option<Self> {
        [
            ModelSource::Explicit,
            ModelSource::Task,
            ModelSource::Recorded,
            ModelSource::Engine,
            ModelSource::Transcript,
            ModelSource::DefaultOnResume,
            ModelSource::TaskInvalidFallback,
            ModelSource::LookupTimeout,
        ]
        .into_iter()
        .find(|m| m.as_str() == s)
    }
}

/// The engine a model obviously belongs to, if any: an exact configured
/// default or preset first, then the id's family (`claude-*` and the
/// Anthropic family names → claude; `gpt-*`, `o<digit>*`, `codex*` and the
/// luna / sol / astra names → codex). `None` means CM can't tell, and the
/// caller must say.
pub fn infer_engine(model: &str) -> Option<&'static str> {
    let m = model.trim();
    let cfg = models();
    if m == cfg.claude || cfg.claude_choices.iter().any(|c| c == m) {
        return Some("claude");
    }
    if m == cfg.codex || cfg.codex_choices.iter().any(|c| c == m) {
        return Some("codex");
    }
    let lower = m.to_ascii_lowercase();
    let first = lower.split(['-', '.', '/', ':', '@']).next().unwrap_or("");
    if lower.starts_with("claude") || ["opus", "sonnet", "haiku", "fable"].contains(&first) {
        return Some("claude");
    }
    let o_series = first.len() >= 2
        && first.starts_with('o')
        && first[1..].chars().all(|c| c.is_ascii_digit());
    let codex_name = lower
        .split(['-', '.', '/', ':', '@'])
        .any(|w| ["luna", "sol", "astra"].contains(&w));
    if lower.starts_with("gpt-") || lower.starts_with("codex") || o_series || codex_name {
        return Some("codex");
    }
    None
}

/// Refuse a model that obviously belongs to the other engine (a
/// `claude-*` id on a codex spawn, a `gpt-*` id on a claude one). A model
/// CM can't place passes: strings stay opaque.
pub fn check_engine(engine: &str, model: &str) -> Result<(), String> {
    match (engine_key(engine), infer_engine(model)) {
        (Some(want), Some(got)) if want != got => Err(format!(
            "model '{}' is a {got} model, but this is a {want} session",
            model.trim()
        )),
        _ => Ok(()),
    }
}

/// The task-metadata slot (`claude` / `codex`) a filed model belongs to:
/// `model_engine` when given (refusing a model that obviously belongs to
/// the other engine), else the engine the model's id names. A model CM
/// can't place needs `model_engine`.
pub fn task_model_slot(model: &str, model_engine: Option<&str>) -> Result<(&'static str, String), String> {
    let model = validate_model(model)?;
    match model_engine.map(str::trim).filter(|e| !e.is_empty()) {
        Some(e) => {
            let slot = engine_key(e).ok_or_else(|| format!("model_engine must be \"claude\" or \"codex\", not '{e}'"))?;
            check_engine(slot, &model)?;
            Ok((slot, model))
        }
        None => infer_engine(&model).map(|slot| (slot, model.clone())).ok_or_else(|| {
            format!("can't tell which engine model '{model}' is for; pass model_engine=\"claude\" or \"codex\"")
        }),
    }
}

/// How much of a transcript's end is searched for the model first.
const TRANSCRIPT_TAIL_BYTES: u64 = 8 << 20;
/// The most of a transcript read front-to-back when the tail names no
/// model (a long conversation whose last turns carry none).
const TRANSCRIPT_SCAN_BYTES: u64 = 64 << 20;

/// The model a conversation last ran, from its transcript: the `model`
/// of claude's newest assistant entry (`message.model`, skipping
/// `<synthetic>`), or codex's newest `turn_context` (`payload.model`).
/// `None` when the file is unreadable or names none.
pub fn transcript_model(engine: &str, path: &std::path::Path) -> Option<String> {
    transcript_value(engine, path, TranscriptField::Model)
}

/// The reasoning effort a conversation last ran at, from its transcript:
/// the top-level `effort` of claude's newest assistant entry, or codex's
/// newest `turn_context` (`payload.effort`).
pub fn transcript_effort(engine: &str, path: &std::path::Path) -> Option<String> {
    transcript_value(engine, path, TranscriptField::Effort)
}

#[derive(Clone, Copy)]
enum TranscriptField {
    Model,
    Effort,
}

fn transcript_value(engine: &str, path: &std::path::Path, field: TranscriptField) -> Option<String> {
    use std::io::{BufRead, Read, Seek, SeekFrom};
    let engine = engine_key(engine)?;
    // `Model"` also matches claude's `requestedModel"`.
    let needle = match field {
        TranscriptField::Model => "odel\"",
        TranscriptField::Effort => "\"effort\"",
    };
    let value_of = |line: &str| -> Option<String> {
        if !line.contains(needle) {
            return None;
        }
        let v: Value = serde_json::from_str(line).ok()?;
        let x = match (engine, field) {
            // `requestedModel` is the id the session asked for, with any
            // context suffix (`claude-opus-5-5[1m]`); `message.model` is
            // the bare id the API answered with, so resuming on it would
            // drop a 1M session to the default context.
            ("claude", TranscriptField::Model) if v.get("type")?.as_str()? == "assistant" => v
                .get("requestedModel")
                .filter(|m| m.as_str().is_some_and(|m| m != "<synthetic>" && validate_model(m).is_ok()))
                .or_else(|| v.get("message")?.get("model"))?,
            ("claude", TranscriptField::Effort) if v.get("type")?.as_str()? == "assistant" => v.get("effort")?,
            ("codex", TranscriptField::Model) if v.get("type")?.as_str()? == "turn_context" => {
                v.get("payload")?.get("model")?
            }
            ("codex", TranscriptField::Effort) if v.get("type")?.as_str()? == "turn_context" => {
                v.get("payload")?.get("effort")?
            }
            _ => return None,
        };
        let x = x.as_str()?;
        match field {
            TranscriptField::Model => (x != "<synthetic>").then(|| validate_model(x).ok()).flatten(),
            TranscriptField::Effort => validate_effort(engine, x).ok(),
        }
    };
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(TRANSCRIPT_TAIL_BYTES);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut tail = Vec::new();
    f.by_ref().take(TRANSCRIPT_TAIL_BYTES).read_to_end(&mut tail).ok()?;
    let tail = String::from_utf8_lossy(&tail);
    // A partial first line (mid-file start) just fails to parse.
    if let Some(x) = tail.lines().rev().find_map(value_of) {
        return Some(x);
    }
    if start == 0 {
        return None;
    }
    f.seek(SeekFrom::Start(0)).ok()?;
    let mut last = None;
    for line in std::io::BufReader::new(f.take(TRANSCRIPT_SCAN_BYTES)).lines() {
        let Ok(line) = line else { break };
        if let Some(x) = value_of(&line) {
            last = Some(x);
        }
    }
    last
}

/// The codex rollout file for a thread id, searched newest day first under
/// `$CODEX_HOME/sessions` (default `~/.codex/sessions`).
pub fn codex_rollout_path(thread_id: &str) -> Option<std::path::PathBuf> {
    let root = std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".codex")))?
        .join("sessions");
    let suffix = format!("-{thread_id}.jsonl");
    let sorted_desc = |dir: &std::path::Path| -> Vec<std::path::PathBuf> {
        let mut v: Vec<_> = std::fs::read_dir(dir)
            .map(|rd| rd.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        v.sort();
        v.reverse();
        v
    };
    for year in sorted_desc(&root).into_iter().filter(|p| p.is_dir()) {
        for month in sorted_desc(&year).into_iter().filter(|p| p.is_dir()) {
            for day in sorted_desc(&month).into_iter().filter(|p| p.is_dir()) {
                for f in sorted_desc(&day) {
                    if f.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(&suffix)) {
                        return Some(f);
                    }
                }
            }
        }
    }
    None
}

/// The transcript file of a session's conversation: its recorded path
/// when that file exists, else the file its resume id names (claude's
/// per-worktree project dir; the codex rollout carrying the thread id).
pub fn session_transcript(
    engine: &str,
    worktree: Option<&std::path::Path>,
    transcript_path: Option<&str>,
    transcript_id: Option<&str>,
) -> Option<std::path::PathBuf> {
    if let Some(p) = transcript_path.map(std::path::PathBuf::from).filter(|p| p.is_file()) {
        return Some(p);
    }
    let id = transcript_id.map(str::trim).filter(|id| !id.is_empty())?;
    match engine_key(engine)? {
        "claude" => crate::transcript_detect::claude_transcript_path(worktree?, id).filter(|p| p.is_file()),
        _ => {
            let uuid = crate::session::codex_rollout_uuid_from_stem(id).unwrap_or_else(|| id.to_string());
            codex_rollout_path(&uuid)
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
        let m = validate_model(m)?;
        check_engine(engine, &m)?;
        return Ok(Some((m, ModelSource::Explicit)));
    }
    let mut fallback = ModelSource::Engine;
    if let Some((m, source)) = inherited {
        match validate_model(m) {
            Ok(m) => return Ok(Some((m, source))),
            Err(_) => fallback = invalid_inherited(source),
        }
    }
    let m = default_model(engine).expect("agent engine has a default");
    validate_model(&m).map(|m| Some((m, fallback)))
}

/// The source of the config default that replaces an inherited value the
/// engine can't take: a task default is reported as such, so the caller
/// can log it and `list_sessions` shows why the default applied.
fn invalid_inherited(source: ModelSource) -> ModelSource {
    if source == ModelSource::Task {
        ModelSource::TaskInvalidFallback
    } else {
        ModelSource::Engine
    }
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
        // A bad inherited value falls through (a task's says so); a bad
        // explicit one is an error.
        assert_eq!(r("codex", None, Some(("bad model", ModelSource::Task))).unwrap().1, ModelSource::TaskInvalidFallback);
        assert_eq!(r("codex", None, Some(("bad model", ModelSource::Recorded))).unwrap().1, ModelSource::Engine);
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

    #[test]
    fn effort_levels_validate_per_engine() {
        assert_eq!(validate_effort("claude-code", " XHigh ").unwrap(), "xhigh");
        assert_eq!(validate_effort("claude-code", "max").unwrap(), "max");
        assert!(validate_effort("codex", "max").unwrap_err().contains("not available for codex"));
        assert_eq!(validate_effort("codex", "low").unwrap(), "low");
        for bad in ["", "extreme", "x high", "-c"] {
            assert!(validate_effort("codex", bad).is_err(), "{bad:?}");
        }
        let cfg = ModelConfig::default();
        assert_eq!(cfg.effort_for("claude-code"), Some(DEFAULT_EFFORT));
        assert_eq!(cfg.effort_for("codex"), Some(DEFAULT_EFFORT));
        let bad: ModelConfig = toml::from_str("codex_effort = \"max\"").unwrap();
        assert!(bad.validate().unwrap_err().contains("models.codex_effort"));
    }

    #[test]
    fn effort_args_and_argv_effort_round_trip() {
        assert_eq!(effort_args("claude-code", "xhigh"), v(&["--effort", "xhigh"]));
        assert_eq!(effort_args("codex", "high"), v(&["-c", "model_reasoning_effort=\"high\""]));
        assert!(effort_args("bash", "high").is_empty());
        let claude = v(&["claude", "--model", "m", "--effort", "max"]);
        assert_eq!(argv_effort("claude-code", &claude).as_deref(), Some("max"));
        assert_eq!(argv_effort("claude-code", &v(&["claude", "--effort=low"])).as_deref(), Some("low"));
        let codex = v(&["python3", "native_codex.py", "--", "-c", "x=1", "-c", "model_reasoning_effort=\"high\""]);
        assert_eq!(argv_effort("codex", &codex).as_deref(), Some("high"));
        assert_eq!(argv_effort("codex", &v(&["codex", "--config=model_reasoning_effort=low"])).as_deref(), Some("low"));
        assert_eq!(argv_effort("codex", &v(&["codex", "-c", "x=1"])), None);
    }

    #[test]
    fn effort_resolution_order() {
        let r = |engine, explicit, inherited| resolve_effort(engine, explicit, inherited).unwrap();
        assert_eq!(r("bash", None, None), None);
        assert!(resolve_effort("bash", Some("high"), None).is_err());
        assert_eq!(r("codex", Some("low"), Some(("high", ModelSource::Task))), Some(("low".into(), ModelSource::Explicit)));
        assert_eq!(r("codex", None, Some(("high", ModelSource::Task))), Some(("high".into(), ModelSource::Task)));
        // A task default the engine can't take is replaced, and says so.
        assert_eq!(
            r("codex", None, Some(("max", ModelSource::Task))),
            Some((DEFAULT_EFFORT.into(), ModelSource::TaskInvalidFallback))
        );
        assert_eq!(r("claude-code", None, Some(("max", ModelSource::Task))), Some(("max".into(), ModelSource::Task)));
        assert_eq!(
            resolve("codex", None, Some(("-bad", ModelSource::Task))).unwrap().map(|(_, s)| s),
            Some(ModelSource::TaskInvalidFallback)
        );
        for s in [ModelSource::TaskInvalidFallback, ModelSource::LookupTimeout] {
            assert_eq!(ModelSource::parse(s.as_str()), Some(s));
        }
        assert!(ModelSource::LookupTimeout.is_provisional() && !ModelSource::Engine.is_provisional());
        assert_eq!(r("claude-code", None, None), Some((DEFAULT_EFFORT.into(), ModelSource::Engine)));
        assert!(resolve_effort("codex", Some("max"), None).is_err());
        assert_eq!(task_effort(&json!({"efforts": {"codex": "low"}}), "codex"), Some("low"));
    }

    fn write_jsonl(lines: &[Value]) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        f
    }

    #[test]
    fn transcript_detection_reads_the_newest_turn_for_both_engines() {
        let claude = write_jsonl(&[
            json!({"type": "assistant", "effort": "high", "message": {"model": "claude-sonnet-5-5"}}),
            json!({"type": "assistant", "effort": "medium", "message": {"model": "claude-opus-5-5"}}),
            json!({"type": "assistant", "message": {"model": "<synthetic>"}}),
            json!({"type": "user", "message": {"model": "not-this"}}),
        ]);
        assert_eq!(transcript_model("claude-code", claude.path()).as_deref(), Some("claude-opus-5-5"));
        assert_eq!(transcript_effort("claude-code", claude.path()).as_deref(), Some("medium"));
        let codex = write_jsonl(&[
            json!({"type": "session_meta", "payload": {"model": "not-this"}}),
            json!({"type": "turn_context", "payload": {"model": "gpt-6-luna", "effort": "high"}}),
            json!({"type": "turn_context", "payload": {"model": "gpt-6-astra", "effort": "xhigh"}}),
            json!({"type": "response_item", "payload": {}}),
        ]);
        assert_eq!(transcript_model("codex", codex.path()).as_deref(), Some("gpt-6-astra"));
        assert_eq!(transcript_effort("codex", codex.path()).as_deref(), Some("xhigh"));
        let none = write_jsonl(&[json!({"type": "user"})]);
        assert_eq!(transcript_model("codex", none.path()), None);
        assert_eq!(transcript_effort("claude-code", none.path()), None);
        assert_eq!(transcript_model("claude-code", std::path::Path::new("/nonexistent/x.jsonl")), None);
    }

    /// A 1M-context session asks for `claude-opus-5-5[1m]` (recorded as
    /// `requestedModel`) while the API answers as the bare id in
    /// `message.model`. Detection must keep the suffix, or a revive
    /// resumes the session at the default context.
    #[test]
    fn transcript_detection_keeps_the_requested_context_suffix() {
        let one_m = write_jsonl(&[
            json!({"type": "assistant", "requestedModel": "claude-opus-5-5[1m]", "effort": "xhigh",
                   "message": {"model": "claude-opus-5-5"}}),
        ]);
        assert_eq!(transcript_model("claude-code", one_m.path()).as_deref(), Some("claude-opus-5-5[1m]"));
        // Older transcripts carry no requestedModel: message.model stands.
        let old = write_jsonl(&[json!({"type": "assistant", "message": {"model": "claude-sonnet-5-5"}})]);
        assert_eq!(transcript_model("claude-code", old.path()).as_deref(), Some("claude-sonnet-5-5"));
        // A synthetic requestedModel falls back too.
        let synth = write_jsonl(&[
            json!({"type": "assistant", "requestedModel": "<synthetic>", "message": {"model": "claude-haiku-5-5"}}),
        ]);
        assert_eq!(transcript_model("claude-code", synth.path()).as_deref(), Some("claude-haiku-5-5"));
        // The real shape: the newest turn wins and its suffix survives.
        let mixed = write_jsonl(&[
            json!({"type": "assistant", "requestedModel": "claude-opus-5-5", "message": {"model": "claude-opus-5-5"}}),
            json!({"type": "assistant", "requestedModel": "claude-opus-5-5[1m]", "message": {"model": "claude-opus-5-5"}}),
        ]);
        assert_eq!(transcript_model("claude-code", mixed.path()).as_deref(), Some("claude-opus-5-5[1m]"));
    }
}
