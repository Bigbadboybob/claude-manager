//! Request queue: bridge between the socket-server thread and the TUI
//! main loop. Each entry is a (Request, reply_sender) tuple. The server
//! thread pushes; the main loop drains and replies.
//!
//! Reply channel uses `mpsc::sync_channel(1)` with capacity 1 so the
//! server thread blocks naturally until the main loop processes the
//! request — avoiding an unbounded buffer of unanswered replies.

use std::collections::VecDeque;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use super::protocol::{Request, Response};

/// One pending request: the parsed envelope plus a reply channel.
pub struct Pending {
    pub request: Request,
    pub reply: mpsc::SyncSender<Response>,
}

/// Shared FIFO between server threads and the main loop. The main loop pops
/// requests within a per-tick time budget and dispatches without the lock.
#[derive(Clone, Default)]
pub struct Queue {
    inner: Arc<Mutex<VecDeque<Pending>>>,
}

impl Queue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Submit a request, return the (sync) receiver for its reply. The
    /// caller blocks on `recv_timeout` until the main loop processes
    /// the queue entry and sends a Response back.
    pub fn submit(&self, request: Request) -> mpsc::Receiver<Response> {
        let (tx, rx) = mpsc::sync_channel::<Response>(1);
        if let Ok(mut q) = self.inner.lock() {
            q.push_back(Pending { request, reply: tx });
        }
        rx
    }

    /// Drain the queue. Called by the main loop each tick. Returns
    /// every pending entry; the main loop processes them in order and
    /// uses each entry's `reply` to send a response back.
    pub fn drain(&self) -> Vec<Pending> {
        match self.inner.lock() {
            Ok(mut q) => q.drain(..).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Take one request without removing the rest of the FIFO. Lets the UI
    /// yield between expensive requests while preserving submission order.
    pub fn push_front(&self, pending: Pending) {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).push_front(pending);
    }

    pub fn pop(&self) -> Option<Pending> {
        self.inner.lock().ok()?.pop_front()
    }
}

/// Default timeout the server thread uses when waiting for the main
/// loop to reply. Generous because some methods (like `start_session`)
/// do non-trivial work; if the main loop takes longer than this, we'd
/// rather log+drop than hang forever.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(30);
