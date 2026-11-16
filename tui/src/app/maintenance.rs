//! The status bar's `⟳ CM deploy` marker while a CM maintenance notice is in
//! force (see `cm_daemon::maintenance`). Like the availability level, it
//! reads the local daemon's notice files every few seconds and asks every
//! configured host (`daemon.maintenance get`) off the input thread every
//! [`POLL`]; a notice on any host shows the marker. A host that predates the
//! RPC answers nothing and counts as no notice.

use super::*;
use serde_json::{json, Value};
use std::sync::mpsc;

const REFRESH: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_secs(20);

pub(super) struct Maintenance {
    root: PathBuf,
    local: bool,
    remote: bool,
    read_at: Option<Instant>,
    polled_at: Option<Instant>,
    poll: Option<mpsc::Receiver<bool>>,
}

impl Maintenance {
    pub(super) fn load() -> Self {
        Self::at(cm_daemon::messaging::rpc::default_root())
    }

    pub(super) fn at(root: PathBuf) -> Self {
        let mut m = Self { root, local: false, remote: false, read_at: None, polled_at: None, poll: None };
        m.refresh(true);
        m
    }

    pub(super) fn active(&self) -> bool {
        self.local || self.remote
    }

    /// Re-read the local files at most every few seconds. True on a change.
    fn refresh(&mut self, force: bool) -> bool {
        if !force && self.read_at.is_some_and(|t| t.elapsed() < REFRESH) {
            return false;
        }
        self.read_at = Some(Instant::now());
        let next = cm_daemon::maintenance::current_now(&self.root).is_some();
        std::mem::replace(&mut self.local, next) != next
    }

    /// `⟳ CM deploy` for the status bar while a notice is in force.
    pub(super) fn status_span(&self) -> Option<Span<'static>> {
        self.active().then(|| {
            Span::styled("\u{27f3} CM deploy ", Style::default().fg(theme::ATTN).add_modifier(Modifier::BOLD))
        })
    }

    fn start_poll(&mut self, targets: Vec<(String, PathBuf, String)>) {
        self.polled_at = Some(Instant::now());
        if targets.is_empty() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new().name("cm-maintenance".into()).spawn(move || {
            let any = targets.iter().any(|(_, socket, token)| {
                crate::client_session::rpc_messaging_board(socket, token, "daemon.maintenance", json!({"action": "get"}))
                    .is_ok_and(|v: Value| v["active"] == true)
            });
            let _ = tx.send(any);
        });
        if spawned.is_ok() {
            self.poll = Some(rx);
        }
    }

    /// Apply a finished poll. True on a change.
    fn finish_poll(&mut self) -> bool {
        let next = match self.poll.as_ref().map(mpsc::Receiver::try_recv) {
            Some(Ok(any)) => any,
            Some(Err(mpsc::TryRecvError::Empty)) | None => return false,
            Some(Err(mpsc::TryRecvError::Disconnected)) => false,
        };
        self.poll = None;
        std::mem::replace(&mut self.remote, next) != next
    }
}

impl App {
    pub fn maintenance_tick(&mut self) {
        if self.maintenance.refresh(false) {
            self.needs_redraw = true;
        }
        if self.maintenance.poll.is_none() && self.maintenance.polled_at.is_none_or(|t| t.elapsed() >= POLL) {
            let targets = self.owner_rpc_targets();
            self.maintenance.start_poll(targets);
        }
        if self.maintenance.finish_poll() {
            self.needs_redraw = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_marker_follows_the_local_notice() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = Maintenance::at(tmp.path().into());
        assert!(!m.active() && m.status_span().is_none());
        cm_daemon::maintenance::apply_local_now(tmp.path(), &json!({"action":"set"})).unwrap();
        assert!(m.refresh(true));
        assert_eq!(m.status_span().unwrap().content, "\u{27f3} CM deploy ");
        cm_daemon::maintenance::apply_local_now(tmp.path(), &json!({"action":"clear"})).unwrap();
        assert!(m.refresh(true));
        assert!(!m.active());
        // A host reporting a notice shows it too.
        let (tx, rx) = mpsc::channel();
        m.poll = Some(rx);
        tx.send(true).unwrap();
        assert!(m.finish_poll());
        assert!(m.active());
    }
}
