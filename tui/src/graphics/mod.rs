//! Kitty graphics passthrough for session panes: images drawn with Unicode
//! placeholders by programs in a pane appear in the outer terminal. See
//! doc/kitty-graphics-passthrough.md.

pub mod command;
pub mod filter;
pub mod outer;
pub mod pane;
pub mod placeholder;
pub mod tap;

use std::path::PathBuf;
use std::sync::mpsc;

pub use pane::{PaneCtx, PaneGraphics};
pub use tap::Tap;

/// Build the reader tap and UI-side state for a newly attached pane, or
/// `None` when passthrough is off. `replay_bytes` is the daemon's count of
/// replayed bytes ahead of live output (absent on older daemons).
pub fn for_attached_pane(
    replay_bytes: Option<u64>,
    daemon_socket: PathBuf,
    operator_token: String,
) -> Option<(Tap, PaneGraphics)> {
    if !outer::enabled() {
        return None;
    }
    let (tx, rx) = mpsc::channel();
    let fetcher: pane::Fetcher = Box::new(move |request| {
        let (done, rx) = mpsc::channel();
        let (socket, token) = (daemon_socket.clone(), operator_token.clone());
        std::thread::spawn(move || {
            let _ = done.send(crate::client_session::rpc_graphics_read_file(&socket, &token, &request));
        });
        rx
    });
    Some((Tap::new(tx, replay_bytes), PaneGraphics::new(rx, fetcher)))
}
