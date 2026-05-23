//! Multi-agent workflow framework.
//!
//! A workflow is a TOML-defined state machine of agent roles (e.g.
//! worker, reviewer, manager). At runtime, each role is bound to a
//! local terminal session. Role activation is driven by idle detection
//! (static transitions from the TOML) or by MCP tool calls the agent
//! makes (dynamic transitions).
//!
//! ## Phase-1 module layout
//!
//! Slice 6 of doc/persistent-host-daemon.md relocated all workflow
//! submodules without `App` / `Session` deps into the `cm-daemon`
//! crate. They're re-exported here so existing `crate::workflow::*`
//! callsites keep compiling unchanged.
//!
//! [`controller`] is the one workflow module that still lives in the
//! TUI: it mutates `App` and holds `Session` handles, and follows the
//! state migration (slices 7–10) into the daemon.

pub mod controller;

// Re-exports of the relocated submodules — keep the
// `crate::workflow::run::*` etc. paths working for the existing
// callsites in `app.rs`, `agent/`, `agent_memory.rs`,
// `control/methods.rs`, and friends.
pub use cm_daemon::workflow::{
    events, history, run, spawn, template, toml_schema, transcript,
};
pub use cm_daemon::workflow::{RoleBinding, RunStatus, TriggerKind, Workflow, WorkflowRun};
