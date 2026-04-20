//! Multi-agent workflow framework.
//!
//! A workflow is a TOML-defined state machine of agent roles (e.g. worker, reviewer, manager).
//! At runtime, each role is bound to a local terminal session. Role activation is driven by
//! idle detection (static transitions from the TOML) or by MCP tool calls the agent makes
//! (dynamic transitions).

pub mod events;
pub mod history;
pub mod run;
pub mod spawn;
pub mod template;
pub mod toml_schema;
pub mod transcript;

pub use run::{RoleBinding, RunStatus, TriggerKind, WorkflowRun};
pub use toml_schema::Workflow;
