//! Multi-agent workflow framework.
//!
//! A workflow is a TOML-defined state machine of agent roles (e.g. worker,
//! reviewer, manager). At runtime, each role is bound to a local terminal
//! session. Role activation is driven by idle detection (static
//! transitions from the TOML) or by MCP tool calls the agent makes
//! (dynamic transitions).
//!
//! ## What's here vs. what stays in `tui/src/workflow/`
//!
//! Slice 6 of doc/persistent-host-daemon.md relocates the workflow
//! submodules that have no `App` / `Session` dependency:
//!
//!   - [`toml_schema`] — workflow definition + TOML loader.
//!   - [`run`] — state machine + on-disk persistence under
//!     `~/.cm/workflow-runs/<id>/state.json`.
//!   - [`events`] — `events.jsonl` file-tail.
//!   - [`history`] — append-only run history records.
//!   - [`template`] — activation-prompt rendering.
//!   - [`transcript`] — claude / codex transcript reader; uses
//!     [`toml_schema::Engine`].
//!   - [`spawn`] — `mcp_server/server.py` path resolver.
//!
//! The controller (`tui/src/workflow/controller.rs`) is the one
//! workflow module that still lives in the TUI: it mutates `App` and
//! holds onto `Session` handles, neither of which has moved to the
//! daemon yet (slices 7–10). When App state migrates, the controller
//! follows.

pub mod events;
pub mod finalize;
pub mod fresh_reset;
pub mod history;
pub mod poller;
pub mod pty_tracker;
pub mod run;
pub mod spawn;
pub mod template;
pub mod toml_schema;
pub mod transcript;

pub use run::{RoleBinding, RunStatus, TriggerKind, WorkflowRun};
pub use toml_schema::Workflow;

/// Idle-nudge backstop ceiling. A role with no static `on_idle`
/// transition must advance the workflow with a dynamic tool call
/// (`workflow_transition` / `workflow_done`). When it instead ends a
/// turn with prose and no tool call, the idle gate (daemon poller +
/// TUI controller) re-delivers a nudge. This caps how many consecutive
/// times a single stuck stretch is re-prompted before the gate gives up
/// and leaves the run idle for an operator to notice — measured as
/// `WorkflowRun::trailing_activation_streak(role) - 1`.
pub const MAX_CONSECUTIVE_IDLE_NUDGES: usize = 3;

/// The distinct prompt the idle-nudge backstop delivers when a role with
/// no static `on_idle` transition ends a turn without advancing the
/// workflow. Deliberately NOT the role's own activation prompt — it
/// names the stall and the single required action. Workflow-agnostic
/// (it doesn't assume which role to hand to); the role's normal prompt
/// already carries that context. Plain text — no `{{ }}` template
/// placeholders, so it renders verbatim on both fire paths.
pub const IDLE_NUDGE_PROMPT: &str = "\
⚠️ Your last turn ended without advancing the workflow — the run is paused on you right now, waiting for a tool call that never came.

End your turn with EXACTLY ONE workflow tool call. That call is the only thing that moves the run forward, and it IS your verdict — put your reasoning inside the call's argument, not in a prose reply:
  - workflow_transition — hand control to another role (put your directive message in `prompt`).
  - workflow_done — the work is complete (put your verdict and the evidence in `reason`).

Do not reply with prose. Make the tool call now.";
