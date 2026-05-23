//! Daemon-side control plane. Phase 1 of doc/persistent-host-daemon.md
//! migrates these submodules one at a time:
//!
//!   - [`protocol`] — wire types. Stateless; moved in slice 4.
//!   - [`wire`] — per-connection length-prefixed JSON framing
//!     (read_request / write_response). Slice 10a-shell.
//!   - [`dispatch`] — method router. Placeholder in 10a-shell that
//!     returns `UnknownMethod` for every method; 10b started routing
//!     `ping`, 10c-b adds `start_session`.
//!   - [`methods`] — JSON-RPC method bodies that take `&mut DaemonState`.
//!     Seeded by 10c-b with `start_session`. 10c-d will add
//!     `send_input`, `kill_session`, `read_session_output` as the
//!     TUI's `tui/src/control/methods.rs` relocates here function-by-
//!     function.
//!   - `server`, `queue` — still in `tui/src/control/` because they
//!     touch the TUI's `App` state. They follow the `App`-state
//!     migration (10b–10e), at which point the deferred half of
//!     slice 4 (`server.rs` + `queue.rs` relocation) lands here.

pub mod dispatch;
pub mod methods;
pub mod protocol;
pub mod stream;
pub mod wire;
