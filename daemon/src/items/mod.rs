//! Work items and boards (doc/items-board.md).
//!
//! Items live in the planning API on cm-manager. Agents reach them only
//! through their own daemon, which knows who is calling: it resolves the
//! caller's messaging participant id, task and board, resolves holder names,
//! checks editing rights, stamps the `actor`, and forwards.

pub mod api;
pub mod heartbeat;
pub mod rpc;

/// Start the items background work (the holder-state heartbeat).
pub fn start(state: &std::sync::Arc<std::sync::Mutex<crate::state::DaemonState>>) {
    heartbeat::start(state);
}
