//! Runtime state for an active workflow run, plus persistence.
//!
//! Each run lives in `~/.cm/workflow-runs/<run-id>/` as:
//!   - `state.json`   — the `WorkflowRun` struct below
//!   - `events.jsonl` — MCP tool calls appended by the workflow_tools in mcp_server/

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunStatus {
    Running,
    Paused,
    Done,
    Detached,
}

/// What caused a role's most recent activation.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriggerKind {
    /// The role that started the run (no prior role).
    Initial,
    /// Fired by a static transition after the previous role went idle.
    StaticIdle { from_role: String },
    /// Fired by an MCP `workflow_transition` tool call.
    McpTransition {
        from_role: String,
        prompt: String,
        event_id: String,
    },
}

/// One row in the run's activation history.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub iteration: u32,
    pub role: String,
    pub session_id: Option<String>,
    /// Captured when this role's turn ends (on transition-out). `None` while the role
    /// is still active or if capture failed.
    pub last_message: Option<String>,
    pub activated_at: u64,
    pub deactivated_at: Option<u64>,
    pub trigger: TriggerKind,
}

/// How a role is bound to a concrete TerminalSession in the TUI.
///
/// We identify the session durably by its label (unique within a task). The
/// `current_session_id` updates when a `fresh`-context role is respawned and the
/// underlying Claude/Codex conversation ID changes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoleBinding {
    pub session_label: String,
    #[serde(default)]
    pub current_session_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowRun {
    pub run_id: String,
    pub workflow_name: String,
    /// The `TaskEntry`-identifying key: worktree_path string for local tasks,
    /// `task:<id>` for cloud tasks. Matches the manifest key convention.
    pub task_key: String,
    pub role_sessions: BTreeMap<String, RoleBinding>,
    pub active_role: Option<String>,
    pub iteration: u32,
    pub paused: bool,
    pub status: RunStatus,
    pub history: Vec<HistoryEntry>,
    pub started_at: u64,
    #[serde(default)]
    pub done_reason: Option<String>,
    /// Byte offset into events.jsonl that we've already processed. Lets us resume
    /// after a TUI restart without re-firing old events.
    #[serde(default)]
    pub events_offset: u64,
}

impl WorkflowRun {
    pub fn new(
        run_id: String,
        workflow_name: String,
        task_key: String,
        role_sessions: BTreeMap<String, RoleBinding>,
        initial_role: String,
    ) -> Self {
        let now = now_unix();
        let initial_history = HistoryEntry {
            iteration: 1,
            role: initial_role.clone(),
            session_id: role_sessions
                .get(&initial_role)
                .and_then(|b| b.current_session_id.clone()),
            last_message: None,
            activated_at: now,
            deactivated_at: None,
            trigger: TriggerKind::Initial,
        };
        WorkflowRun {
            run_id,
            workflow_name,
            task_key,
            role_sessions,
            active_role: Some(initial_role),
            iteration: 1,
            paused: false,
            status: RunStatus::Running,
            history: vec![initial_history],
            started_at: now,
            done_reason: None,
            events_offset: 0,
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self.status, RunStatus::Running | RunStatus::Paused)
    }

    pub fn role_binding(&self, role: &str) -> Option<&RoleBinding> {
        self.role_sessions.get(role)
    }

    /// Most recent captured `last_message` for a given role. Used by prompt templating.
    pub fn last_message_for(&self, role: &str) -> Option<&str> {
        self.history
            .iter()
            .rev()
            .find(|h| h.role == role && h.last_message.is_some())
            .and_then(|h| h.last_message.as_deref())
    }

    /// Close out the active role's history entry with a captured last_message and
    /// timestamp. Used when transitioning away.
    pub fn close_active_role(&mut self, last_message: Option<String>) {
        if let Some(entry) = self.history.last_mut() {
            if entry.deactivated_at.is_none() {
                entry.deactivated_at = Some(now_unix());
                entry.last_message = last_message;
            }
        }
    }

    /// Append a new activation history entry and update active_role/iteration.
    pub fn activate_role(&mut self, role: String, trigger: TriggerKind) {
        // Iteration increments when we cycle back to the first role in role_order.
        // For simplicity here, iteration tracks total activations / roles.len(), but
        // we leave precise iteration accounting to the caller that holds the Workflow.
        self.iteration += 1;
        let session_id = self
            .role_sessions
            .get(&role)
            .and_then(|b| b.current_session_id.clone());
        self.history.push(HistoryEntry {
            iteration: self.iteration,
            role: role.clone(),
            session_id,
            last_message: None,
            activated_at: now_unix(),
            deactivated_at: None,
            trigger,
        });
        self.active_role = Some(role);
    }

    pub fn mark_done(&mut self, reason: String) {
        self.close_active_role(None);
        self.active_role = None;
        self.status = RunStatus::Done;
        self.done_reason = Some(reason);
    }

    pub fn mark_detached(&mut self) {
        self.status = RunStatus::Detached;
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        self.status = if paused { RunStatus::Paused } else { RunStatus::Running };
    }
}

// ------------------- Persistence -------------------

pub fn runs_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".cm/workflow-runs")
}

pub fn run_dir(run_id: &str) -> PathBuf {
    runs_dir().join(run_id)
}

pub fn events_path(run_id: &str) -> PathBuf {
    run_dir(run_id).join("events.jsonl")
}

fn state_path(run_id: &str) -> PathBuf {
    run_dir(run_id).join("state.json")
}

pub fn new_run_id() -> String {
    // Compact base36-ish: seconds + a bit of randomness from the OS.
    let secs = now_unix();
    let rand_part: u32 = {
        // Use env + pid + nanos as a lightweight entropy source. Good enough for IDs
        // that are unique per-machine; we're not creating millions.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let pid = std::process::id();
        nanos ^ pid
    };
    format!("wf_{:x}{:x}", secs, rand_part)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug)]
pub enum PersistError {
    Io(io::Error),
    Json(serde_json::Error),
}

impl From<io::Error> for PersistError {
    fn from(e: io::Error) -> Self {
        PersistError::Io(e)
    }
}
impl From<serde_json::Error> for PersistError {
    fn from(e: serde_json::Error) -> Self {
        PersistError::Json(e)
    }
}

impl std::fmt::Display for PersistError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PersistError::Io(e) => write!(f, "io: {}", e),
            PersistError::Json(e) => write!(f, "json: {}", e),
        }
    }
}

/// Save atomically: write to state.json.tmp then rename.
pub fn save(run: &WorkflowRun) -> Result<(), PersistError> {
    let dir = run_dir(&run.run_id);
    fs::create_dir_all(&dir)?;
    let tmp = dir.join("state.json.tmp");
    let final_path = dir.join("state.json");
    let json = serde_json::to_string_pretty(run)?;
    fs::write(&tmp, json)?;
    fs::rename(&tmp, &final_path)?;
    Ok(())
}

pub fn load(run_id: &str) -> Result<WorkflowRun, PersistError> {
    let path = state_path(run_id);
    let contents = fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&contents)?)
}

/// Load all persisted runs. Invalid/unreadable state.json files are skipped.
pub fn load_all() -> Vec<WorkflowRun> {
    let dir = runs_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let state_file = path.join("state.json");
        if !state_file.exists() {
            continue;
        }
        if let Ok(contents) = fs::read_to_string(&state_file) {
            if let Ok(run) = serde_json::from_str::<WorkflowRun>(&contents) {
                out.push(run);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_run() -> WorkflowRun {
        let mut roles = BTreeMap::new();
        roles.insert(
            "worker".to_string(),
            RoleBinding {
                session_label: "claude".to_string(),
                current_session_id: Some("sid-1".into()),
            },
        );
        roles.insert(
            "reviewer".to_string(),
            RoleBinding {
                session_label: "reviewer".to_string(),
                current_session_id: None,
            },
        );
        WorkflowRun::new(
            "wf_test".into(),
            "feedback".into(),
            "/tmp/repo".into(),
            roles,
            "worker".into(),
        )
    }

    #[test]
    fn activate_and_close() {
        let mut run = sample_run();
        assert_eq!(run.active_role.as_deref(), Some("worker"));
        assert_eq!(run.history.len(), 1);

        run.close_active_role(Some("worker was here".into()));
        run.activate_role(
            "reviewer".into(),
            TriggerKind::StaticIdle { from_role: "worker".into() },
        );
        assert_eq!(run.active_role.as_deref(), Some("reviewer"));
        assert_eq!(run.history.len(), 2);
        assert_eq!(run.last_message_for("worker"), Some("worker was here"));
    }

    #[test]
    fn round_trip_serde() {
        let run = sample_run();
        let s = serde_json::to_string(&run).unwrap();
        let back: WorkflowRun = serde_json::from_str(&s).unwrap();
        assert_eq!(back.run_id, run.run_id);
        assert_eq!(back.role_sessions.len(), 2);
    }

    #[test]
    fn mark_done_sets_status() {
        let mut run = sample_run();
        run.mark_done("looks good".into());
        assert_eq!(run.status, RunStatus::Done);
        assert!(run.active_role.is_none());
        assert!(!run.is_active());
    }
}
