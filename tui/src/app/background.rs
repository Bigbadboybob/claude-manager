//! Daemon RPCs a keypress triggers run on a worker, never the input thread.
//!
//! A remote RPC costs a round trip over the host's SSH link, and a slow or
//! failing link turns that into its whole budget (5 s for most calls, 150 s
//! for `start_workflow`) during which no key reaches any pane. The worker
//! returns a closure that the main loop applies on a later tick
//! (`drain_background`), so the UI-side effects stay on the UI thread.
use super::*;
use std::sync::mpsc::{self, Receiver, TryRecvError};

/// What a finished background job does to the App.
pub(super) type Apply = Box<dyn FnOnce(&mut App) + Send>;

pub(super) struct BackgroundJob {
    /// Jobs with the same key are refused while one is in flight.
    key: Option<String>,
    result: Receiver<Apply>,
}

impl App {
    /// Run `work` on a worker and apply its result on a later tick. Returns
    /// false (with a status line) when a job with the same `key` is still
    /// running or no worker could be started.
    pub(super) fn run_in_background(
        &mut self,
        key: Option<String>,
        work: impl FnOnce() -> Apply + Send + 'static,
    ) -> bool {
        if key.is_some() && self.background_jobs.iter().any(|j| j.key == key) {
            self.set_status_msg("Already in progress — wait for it to finish");
            return false;
        }
        let (tx, result) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("cm-background-rpc".into())
            .spawn(move || {
                let _ = tx.send(work());
            });
        if let Err(e) = spawned {
            self.set_status_msg(&format!("Could not start a worker: {e}"));
            return false;
        }
        self.background_jobs.push(BackgroundJob { key, result });
        true
    }

    pub(super) fn background_in_flight(&self, key: &str) -> bool {
        self.background_jobs.iter().any(|j| j.key.as_deref() == Some(key))
    }

    pub(crate) fn drain_background(&mut self) {
        let mut i = 0;
        while i < self.background_jobs.len() {
            match self.background_jobs[i].result.try_recv() {
                Ok(apply) => {
                    self.background_jobs.remove(i);
                    apply(self);
                    self.needs_redraw = true;
                }
                Err(TryRecvError::Empty) => i += 1,
                Err(TryRecvError::Disconnected) => {
                    self.background_jobs.remove(i);
                    self.set_status_msg("A background request stopped without a result");
                }
            }
        }
    }
}

#[cfg(test)]
impl App {
    /// Apply every in-flight background job, as the main loop would.
    pub(crate) fn settle_background(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !self.background_jobs.is_empty() {
            assert!(Instant::now() < deadline, "a background job never finished");
            std::thread::sleep(Duration::from_millis(5));
            self.drain_background();
        }
    }
}
