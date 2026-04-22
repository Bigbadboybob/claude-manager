//! Workflow definition schema, TOML loader, and validation.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Which coding agent hosts a role.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Engine {
    ClaudeCode,
    Codex,
}

impl Engine {
    pub fn as_session_type(&self) -> &'static str {
        match self {
            Engine::ClaudeCode => "claude",
            Engine::Codex => "codex",
        }
    }
}

/// How a role's context is handled across activations.
///
/// - `Persistent`: the session's conversation is resumed each activation; the agent
///   sees full prior history.
/// - `Fresh`: the underlying agent process is killed and respawned each activation
///   so the new conversation starts empty. The session slot in the sidebar survives.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Context {
    Persistent,
    Fresh,
}

/// Which signal fires a static transition.
///
/// Today only `Idle` is supported (from the TOML). Dynamic transitions come from MCP
/// tool calls at runtime and don't appear in the TOML.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TriggerOn {
    Idle,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Role {
    pub engine: Engine,
    pub context: Context,
    /// Optional prompt rendered and delivered to the PTY on the FIRST activation
    /// of this role. Supports `{{ roles.<role>.last_message }}` and `{{ goal }}`
    /// substitutions. Roles whose prompts always come from the previous role's
    /// tool call (e.g. the worker in feedback mode) can omit this.
    #[serde(default)]
    pub activation_prompt: Option<String>,
    /// Optional prompt for second+ activations. When set, persistent-context
    /// roles that already saw the first activation template don't need to re-
    /// render the full context (they have it in their conversation history);
    /// this can be a much shorter prompt that just surfaces the new material.
    /// If omitted, `activation_prompt` is reused each time.
    #[serde(default)]
    pub subsequent_activation_prompt: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Transition {
    pub from: String,
    pub on: TriggerOn,
    pub to: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Workflow {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Ordered role declarations. BTreeMap gives stable ordering by name; for presentation
    /// order in the launch modal we use `role_order` below.
    pub roles: BTreeMap<String, Role>,
    /// Presentation order for roles in the launch modal + sidebar. Optional; defaults to
    /// the order roles were first referenced or inserted (we use a separate list because
    /// TOML table ordering isn't preserved by serde).
    #[serde(default)]
    pub role_order: Vec<String>,
    #[serde(default)]
    pub transitions: Vec<Transition>,
}

#[derive(Debug)]
pub enum WorkflowError {
    Io(std::io::Error),
    Parse(toml::de::Error),
    Validation(String),
}

impl fmt::Display for WorkflowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkflowError::Io(e) => write!(f, "io: {}", e),
            WorkflowError::Parse(e) => write!(f, "parse: {}", e),
            WorkflowError::Validation(msg) => write!(f, "invalid workflow: {}", msg),
        }
    }
}

impl std::error::Error for WorkflowError {}

impl From<std::io::Error> for WorkflowError {
    fn from(e: std::io::Error) -> Self {
        WorkflowError::Io(e)
    }
}

impl From<toml::de::Error> for WorkflowError {
    fn from(e: toml::de::Error) -> Self {
        WorkflowError::Parse(e)
    }
}

impl Workflow {
    /// Parse a workflow from a TOML string and validate it.
    pub fn from_toml_str(s: &str) -> Result<Self, WorkflowError> {
        let mut wf: Workflow = toml::from_str(s)?;

        // Fill role_order from roles map if not explicitly provided.
        if wf.role_order.is_empty() {
            wf.role_order = wf.roles.keys().cloned().collect();
        }

        wf.validate()?;
        Ok(wf)
    }

    /// Load a workflow from a TOML file.
    pub fn from_file(path: &Path) -> Result<Self, WorkflowError> {
        let contents = fs::read_to_string(path)?;
        Self::from_toml_str(&contents)
    }

    /// Structural checks. Returns the first failure.
    ///
    /// We intentionally do NOT require every role to have a static outgoing transition:
    /// a role may rely on MCP tool calls to transition (that's runtime behavior and can't
    /// be statically verified). A role with neither static transition nor tool call will
    /// stall at runtime — the TUI surfaces that as "waiting on <role>" but it's not
    /// a TOML-level error.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        if self.name.trim().is_empty() {
            return Err(WorkflowError::Validation("workflow name is required".into()));
        }
        if self.roles.is_empty() {
            return Err(WorkflowError::Validation("at least one role is required".into()));
        }

        // role_order must match roles exactly.
        let declared: HashSet<&str> = self.roles.keys().map(|s| s.as_str()).collect();
        let ordered: HashSet<&str> = self.role_order.iter().map(|s| s.as_str()).collect();
        if declared != ordered {
            return Err(WorkflowError::Validation(
                "role_order must reference exactly the declared roles".into(),
            ));
        }

        // Transitions must reference known roles; no duplicates from the same (from, on).
        let mut seen: HashSet<(&str, &TriggerOn)> = HashSet::new();
        for t in &self.transitions {
            if !self.roles.contains_key(&t.from) {
                return Err(WorkflowError::Validation(format!(
                    "transition from unknown role: {}",
                    t.from
                )));
            }
            if !self.roles.contains_key(&t.to) {
                return Err(WorkflowError::Validation(format!(
                    "transition to unknown role: {}",
                    t.to
                )));
            }
            if !seen.insert((t.from.as_str(), &t.on)) {
                return Err(WorkflowError::Validation(format!(
                    "duplicate static transition from {}",
                    t.from
                )));
            }
        }

        Ok(())
    }

    /// Find the static transition fired when `role` becomes idle, if any.
    pub fn static_transition_on_idle(&self, role: &str) -> Option<&Transition> {
        self.transitions
            .iter()
            .find(|t| t.from == role && matches!(t.on, TriggerOn::Idle))
    }
}

/// Resolve the directory containing workflow TOML files.
///
/// Order of preference:
///   1. `$CM_WORKFLOWS_DIR` env var
///   2. A `workflows/` directory found by walking up from the current working directory
///   3. A `workflows/` directory found by walking up from the binary's location
///   4. `~/.cm/workflows/`
pub fn workflows_dir() -> PathBuf {
    if let Ok(p) = std::env::var("CM_WORKFLOWS_DIR") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        if let Some(p) = walk_up_for(&cwd, "workflows") {
            return p;
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(p) = walk_up_for(&exe, "workflows") {
            return p;
        }
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".cm/workflows")
}

/// Walk up from `start` (inclusive) looking for a subdirectory named `name`.
fn walk_up_for(start: &Path, name: &str) -> Option<PathBuf> {
    let mut cur: Option<&Path> = Some(start);
    while let Some(p) = cur {
        let candidate = p.join(name);
        if candidate.is_dir() {
            return Some(candidate);
        }
        cur = p.parent();
    }
    None
}

/// Load all valid workflows from the workflows directory.
///
/// Returns a name -> Workflow map. Invalid files are skipped but their errors are returned
/// alongside so the caller can surface them in the UI.
pub fn load_all(dir: &Path) -> (HashMap<String, Workflow>, Vec<(PathBuf, WorkflowError)>) {
    let mut workflows = HashMap::new();
    let mut errors = Vec::new();

    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            errors.push((dir.to_path_buf(), WorkflowError::Io(e)));
            return (workflows, errors);
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("toml") {
            continue;
        }
        match Workflow::from_file(&path) {
            Ok(wf) => {
                workflows.insert(wf.name.clone(), wf);
            }
            Err(e) => errors.push((path, e)),
        }
    }

    (workflows, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEEDBACK: &str = r#"
name = "feedback"
description = "Worker -> reviewer -> manager iteration loop"

[roles.worker]
engine = "claude-code"
context = "persistent"

[roles.reviewer]
engine = "claude-code"
context = "fresh"
activation_prompt = "Review the worker's unstaged changes. Run git diff. Worker's last: {{ roles.worker.last_message }}"

[roles.manager]
engine = "claude-code"
context = "persistent"
activation_prompt = "Worker: {{ roles.worker.last_message }}\n\nReviewer: {{ roles.reviewer.last_message }}\n\nCall workflow_transition or workflow_done."

[[transitions]]
from = "worker"
on = "idle"
to = "reviewer"

[[transitions]]
from = "reviewer"
on = "idle"
to = "manager"
"#;

    #[test]
    fn parses_feedback_workflow() {
        let wf = Workflow::from_toml_str(FEEDBACK).expect("should parse");
        assert_eq!(wf.name, "feedback");
        assert_eq!(wf.roles.len(), 3);
        assert_eq!(wf.transitions.len(), 2);
        assert_eq!(wf.roles["reviewer"].context, Context::Fresh);
        assert_eq!(wf.roles["worker"].engine, Engine::ClaudeCode);
    }

    #[test]
    fn rejects_transition_to_unknown_role() {
        let bad = r#"
name = "x"
[roles.a]
engine = "claude-code"
context = "persistent"
[[transitions]]
from = "a"
on = "idle"
to = "nonexistent"
"#;
        let err = Workflow::from_toml_str(bad).unwrap_err();
        match err {
            WorkflowError::Validation(msg) => assert!(msg.contains("unknown role")),
            _ => panic!("expected validation error, got {:?}", err),
        }
    }

    #[test]
    fn rejects_duplicate_static_transition() {
        let bad = r#"
name = "x"
[roles.a]
engine = "claude-code"
context = "persistent"
[roles.b]
engine = "claude-code"
context = "persistent"
[[transitions]]
from = "a"
on = "idle"
to = "b"
[[transitions]]
from = "a"
on = "idle"
to = "b"
"#;
        let err = Workflow::from_toml_str(bad).unwrap_err();
        match err {
            WorkflowError::Validation(msg) => assert!(msg.contains("duplicate")),
            _ => panic!("expected validation error"),
        }
    }

    #[test]
    fn static_transition_on_idle_lookup() {
        let wf = Workflow::from_toml_str(FEEDBACK).unwrap();
        assert_eq!(wf.static_transition_on_idle("worker").unwrap().to, "reviewer");
        assert_eq!(wf.static_transition_on_idle("reviewer").unwrap().to, "manager");
        assert!(wf.static_transition_on_idle("manager").is_none());
    }

    /// End-to-end template substitution — uses a synthetic template to avoid
    /// coupling to whatever the user has in their current feedback.toml. This
    /// proves the render path produces non-empty output when substitutions
    /// fire, which is what fire_transition requires to set pending_prompt.
    #[test]
    fn full_render_pipeline_non_empty_with_substitution() {
        use crate::workflow::template::{render, RoleResolver};

        let template = "Worker: {{ roles.worker.last_message }}\nReviewer: {{ roles.reviewer.last_message }}";

        struct Stub;
        impl RoleResolver for Stub {
            fn user_messages(&self, _: &str) -> Vec<String> { Vec::new() }
            fn assistant_messages(&self, role: &str) -> Vec<String> {
                match role {
                    "worker" => vec!["did the thing".into()],
                    "reviewer" => vec!["needs another pass".into()],
                    _ => Vec::new(),
                }
            }
            fn prior_user_messages(&self, _: &str) -> Vec<String> { Vec::new() }
            fn prior_assistant_messages(&self, _: &str) -> Vec<String> { Vec::new() }
            fn latest_plan(&self, _: &str) -> Option<String> { None }
            fn goal(&self) -> Option<String> { None }
        }

        let rendered = render(template, &Stub);
        assert_eq!(rendered, "Worker: did the thing\nReviewer: needs another pass");

        // Make sure what fire_transition would build is sensible:
        //   - non-empty (otherwise pending_prompt won't be set)
        //   - ends with \r after trim_end (so Enter submits instead of
        //     just inserting a newline)
        let payload = format!("{}\r", rendered.trim_end());
        assert!(payload.ends_with('\r'));
        assert!(!payload.ends_with("\n\r"), "trailing \\n before \\r = Enter gets swallowed");
    }

    /// Sanity-check that the actual `workflows/feedback.toml` shipped in the repo
    /// loads through our loader and validates. Runs only when the file is present.
    #[test]
    fn shipped_feedback_toml_loads() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("workflows")
            .join("feedback.toml");
        if !path.exists() {
            eprintln!("skipping: {} not found", path.display());
            return;
        }
        let wf = Workflow::from_file(&path).expect("feedback.toml should load");
        assert_eq!(wf.name, "feedback");
        assert_eq!(wf.role_order, vec!["worker", "reviewer", "manager"]);
        assert_eq!(wf.roles["reviewer"].context, Context::Fresh);
        assert_eq!(wf.static_transition_on_idle("worker").unwrap().to, "reviewer");
        assert_eq!(wf.static_transition_on_idle("reviewer").unwrap().to, "manager");
        assert!(wf.static_transition_on_idle("manager").is_none());
    }
}
