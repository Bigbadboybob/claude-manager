//! Continuous Tasks — durable, manually-triggerable units of work.
//!
//! A continuous task is pinned to one long-lived worktree and fired on demand
//! (Phase 2 — the trigger funnel, manual-only). Continuity across fires is the
//! per-task `NOTES.md` (the default prompt instructs read-NOTES-first), not a
//! persistent process.
//!
//! ## What's here
//!
//! Phase 2 of DESIGN_CONTINUOUS_TASKS.md ships the DATA layer:
//!
//!   - [`task`] — the [`ContinuousTask`] record + on-disk persistence under
//!     `~/.cm/continuous-tasks/<id>/state.json`. A structural twin of
//!     `crate::workflow::run`.
//!   - [`runlog`] — the append-only `runs.jsonl` audit. A structural twin of
//!     `crate::workflow::events`.
//!
//! The trigger funnel + FRESH executor + `continuous.*` handlers live in
//! `crate::control::methods` (a later slice) and consume this contract. The
//! scheduler (Phase 3), persistent executor (Phase 3), queue/Consumer
//! (Phase 4) and downstream fan-out (Phase 6) are later phases — their struct
//! fields are defined here but inert.

pub mod dispatch_pending;
pub mod queue;
pub mod runlog;
pub mod scheduler;
pub mod task;

pub use scheduler::ContinuousScheduler;
pub use task::{
    ContinuousTask, Engine, InFlight, ModePreset, Retention, RunMode, RunRecord, RunStatus,
    Schedule,
};
