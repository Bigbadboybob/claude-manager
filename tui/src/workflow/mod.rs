//! Multi-agent workflow framework.
//!
//! A workflow is a TOML-defined state machine of agent roles (e.g.
//! worker, reviewer, manager). At runtime, each role is bound to a
//! local terminal session. Role activation is driven by idle detection
//! (static transitions from the TOML) or by MCP tool calls the agent
//! makes (dynamic transitions).
//!
//! ## Module layout
//!
//! Slice 6 of doc/persistent-host-daemon.md relocated all workflow
//! submodules without `App` / `Session` deps into the `cm-daemon`
//! crate. They're re-exported here so existing `crate::workflow::*`
//! callsites keep compiling unchanged. After the daemon-side
//! orchestration relocation the TUI owns NO workflow logic — it's a
//! pure observer. The former `controller` module (by the end just a
//! duplicate `WorkflowResolver` kept in sync with the daemon's by a
//! parity test — dead code, since the TUI renders nothing) was deleted;
//! its render coverage moved to `cm_daemon::workflow::poller`'s
//! `daemon_resolver_renders_all_template_shapes`.

/// Workflow OBSERVATION glue (tick logging, and — incrementally — the
/// run-state-mirror plumbing being extracted out of `app.rs`).
pub(crate) mod observer;

// Re-exports of the relocated submodules — keep the
// `crate::workflow::run::*` etc. paths working for the existing
// callsites in `app.rs`, `agent/`, `agent_memory.rs`,
// `control/methods.rs`, and friends.
pub use cm_daemon::workflow::{
    history, run, spawn, template, toml_schema, transcript,
};
pub use cm_daemon::workflow::{RunStatus, Workflow, WorkflowRun};
