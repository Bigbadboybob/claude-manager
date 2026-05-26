//! TUI control socket: a Unix-domain JSON-RPC-ish endpoint at
//! `~/.cm/tui.sock` that lets MCP servers (or any local client) drive
//! the TUI.
//!
//! Architecture:
//!   - One accept-loop thread (`server`). For each connection it reads
//!     one length-prefixed JSON request, packages it with a oneshot
//!     reply channel, and pushes onto a shared queue.
//!   - The TUI main loop drains the queue every tick and dispatches each
//!     request to a method handler (`methods`). The main loop is the only
//!     thing that mutates `App` state, so handlers run synchronously on
//!     it without any extra locking.
//!   - Replies travel back through the oneshot channel; the server thread
//!     writes them out and closes the connection.
//!
//! Wire format (also captured in `protocol.rs`): 4-byte big-endian length
//! prefix, then a UTF-8 JSON object. One request per connection in v1
//! (streaming subscriptions are a later phase).

pub mod methods;
pub mod queue;
pub mod server;

// Wire types relocated to the `cm-daemon` crate in slice 4 of
// doc/persistent-host-daemon.md. Re-exported here as `crate::control::protocol`
// so every existing TUI callsite continues to compile unchanged. The
// re-export goes away when `server` / `queue` / `methods` follow the
// App-state migration into the daemon (the deferred half of slice 4).
pub use cm_daemon::control::protocol;

#[cfg(test)]
mod tests;
