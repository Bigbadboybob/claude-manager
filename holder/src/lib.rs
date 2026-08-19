//! `cm-holder` — the tiny, near-frozen PTY-holder process.
//! DESIGN_HOLDER_BRAIN_SPLIT phase 2 (the holder MVP).
//!
//! The library half exists so the behavioral suite can drive a real
//! [`holder::Holder`] in-process: the tests own one end of a
//! socketpair and ACT as the brain, while the holder loop runs on a
//! thread of the same (test) process — which is therefore the
//! parent of every spawned session child, exactly as the production
//! topology requires for `waitid`. The `cm-holder` binary
//! (`main.rs`) is the thin bootstrap + brain-respawn loop around
//! [`holder::Holder::serve`]; its supervision hardening (breaker,
//! pinned-FD rollback, wedge watchdog consequences, signal
//! forwarding) is phase 6.

#![cfg(target_os = "linux")]

pub mod holder;
pub mod reap;
pub mod spawn;
pub mod supervisor;
