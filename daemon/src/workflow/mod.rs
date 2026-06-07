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
