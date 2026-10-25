//! F7: Owner availability picker and the status-bar level.
//!
//! The level is a replicated messaging event (see
//! `cm_daemon::owner_availability`). The status bar reads the local daemon's
//! projection file, and every [`POLL`] asks each configured host for the
//! level off the input thread; it shows whichever changed last. The
//! projection alone can be absent (a laptop daemon that predates
//! availability) or stale (a replica that stopped syncing), and then a level
//! set from the board or another host never showed (2026-10-18). The picker
//! sets the level through the local daemon's `messaging.availability`, which
//! forwards to the messaging hub; a local daemon that predates availability
//! (or does not serve Owner) is skipped for the next configured host.

use super::*;
use serde_json::{json, Value};
use std::sync::mpsc;

/// Picker rows: (level sent to the daemon, what it means).
const CHOICES: [(&str, &str); 5] = [
    ("away", "not available"),
    ("around", "intermittently available"),
    ("focused", "concentrating on work"),
    ("on-call", "available for requests"),
    ("unset", "no availability information"),
];
/// How often the status bar re-reads the projection.
const REFRESH: Duration = Duration::from_secs(5);
/// How often the status bar asks the hosts for the level.
const POLL: Duration = Duration::from_secs(30);

struct Dialog {
    cursor: usize,
    saving: Option<mpsc::Receiver<Result<Value, String>>>,
    error: Option<String>,
}

pub(super) struct Availability {
    root: PathBuf,
    current: Value,
    /// The newest level a host returned: a save reply or a poll.
    remote: Value,
    read_at: Option<Instant>,
    polled_at: Option<Instant>,
    poll: Option<mpsc::Receiver<Vec<Value>>>,
    dialog: Option<Dialog>,
}

use cm_daemon::owner_availability::changed_at_ms as changed_at;

impl Availability {
    pub(super) fn load() -> Self {
        Self::at(cm_daemon::messaging::rpc::default_root())
    }

    fn at(root: PathBuf) -> Self {
        let mut a = Self {
            root,
            current: Value::Null,
            remote: Value::Null,
            read_at: None,
            polled_at: None,
            poll: None,
            dialog: None,
        };
        a.refresh(true);
        a
    }

    pub(super) fn is_open(&self) -> bool {
        self.dialog.is_some()
    }

    /// Re-read the projection at most every few seconds. Returns true when
    /// the visible value changed.
    fn refresh(&mut self, force: bool) -> bool {
        if !force && self.read_at.is_some_and(|t| t.elapsed() < REFRESH) {
            return false;
        }
        self.read_at = Some(Instant::now());
        let local = cm_daemon::owner_availability::projection_path(&self.root)
            .exists()
            .then(|| cm_daemon::owner_availability::exposure(&self.root));
        let next = match local {
            Some(local) if self.remote.is_null() || changed_at(&local) >= changed_at(&self.remote) => local,
            _ if self.remote.is_object() => cm_daemon::owner_availability::with_current_age(&self.remote),
            _ => cm_daemon::owner_availability::exposure(&self.root),
        };
        let changed = next["level"] != self.current["level"] || next["changed_at"] != self.current["changed_at"];
        self.current = next;
        changed
    }

    /// `● focused 14m` for the status bar; None while unset.
    pub(super) fn status_span(&self) -> Option<Span<'static>> {
        let level = self.current["level"].as_str()?;
        let color = match level {
            "away" => theme::ERROR,
            "around" => theme::ATTN,
            "focused" => theme::AFTERGLOW,
            _ => theme::OK,
        };
        let age = self.current["age_s"].as_i64().map(|s| {
            let s = s.max(0);
            if s >= 86_400 {
                format!(" {}d", s / 86_400)
            } else if s >= 3600 {
                format!(" {}h", s / 3600)
            } else {
                format!(" {}m", s / 60)
            }
        });
        Some(Span::styled(
            format!("\u{25cf} {level}{} ", age.unwrap_or_default()),
            Style::default().fg(color),
        ))
    }

    /// Keep `v` (an exposure from a host) when it changed after the newest
    /// one seen so far.
    fn absorb(&mut self, v: &Value) {
        if v.is_object() && (self.remote.is_null() || changed_at(v) > changed_at(&self.remote)) {
            self.remote = v.clone();
        }
    }

    fn poll_due(&self) -> bool {
        self.poll.is_none() && self.polled_at.is_none_or(|t| t.elapsed() >= POLL)
    }

    /// Ask every host for the level on a worker. `targets` as
    /// [`App::owner_rpc_targets`]; a host that fails is skipped.
    fn start_poll(&mut self, targets: Vec<(String, PathBuf, String)>) {
        self.polled_at = Some(Instant::now());
        if targets.is_empty() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new().name("cm-availability".into()).spawn(move || {
            let replies = targets
                .iter()
                .filter_map(|(_, socket, token)| {
                    crate::client_session::rpc_messaging_board(socket, token, "messaging.availability", json!({"action": "get"}))
                        .ok()
                })
                .map(|v| v["owner_availability"].clone())
                .collect();
            let _ = tx.send(replies);
        });
        if spawned.is_ok() {
            self.poll = Some(rx);
        }
    }

    /// Apply a finished poll. Returns true when the visible value changed.
    fn finish_poll(&mut self) -> bool {
        let replies = match self.poll.as_ref().map(mpsc::Receiver::try_recv) {
            Some(Ok(replies)) => replies,
            Some(Err(mpsc::TryRecvError::Empty)) | None => return false,
            Some(Err(mpsc::TryRecvError::Disconnected)) => Vec::new(),
        };
        self.poll = None;
        for v in &replies {
            self.absorb(v);
        }
        self.refresh(true)
    }

    fn cursor_for_current(&self) -> usize {
        let level = self.current["level"].as_str().unwrap_or("unset");
        CHOICES.iter().position(|(l, _)| *l == level).unwrap_or(4)
    }
}

impl App {
    /// Status-bar refresh, the host poll and the result of an in-flight
    /// save. Cheap on this thread: a small file read every few seconds; the
    /// RPCs run on a worker.
    pub fn availability_tick(&mut self) {
        if self.availability.refresh(false) {
            self.needs_redraw = true;
        }
        if self.availability.poll_due() {
            let targets = self.owner_rpc_targets();
            self.availability.start_poll(targets);
        }
        if self.availability.finish_poll() {
            self.needs_redraw = true;
        }
        let Some(dialog) = self.availability.dialog.as_mut() else {
            return;
        };
        let Some(result) = dialog.saving.as_ref().and_then(|rx| rx.try_recv().ok()) else {
            return;
        };
        dialog.saving = None;
        self.needs_redraw = true;
        match result {
            Ok(v) => {
                if v["owner_availability"].is_object() {
                    self.availability.absorb(&v["owner_availability"]);
                    self.availability.refresh(true);
                }
                self.availability.dialog = None;
                let level = self.availability.current["level"].as_str().unwrap_or("unset").to_owned();
                self.status_msg = Some((format!("Availability: {level}"), Instant::now()));
            }
            Err(e) => dialog.error = Some(e),
        }
    }

    /// Operator RPC targets for Owner-scoped calls: the local daemon first
    /// (the messages pane's host), then every other configured host, so a
    /// laptop daemon that predates a method falls through to one that has
    /// it. `(host name, socket, operator token)`; resolved non-blocking.
    pub(crate) fn owner_rpc_targets(&self) -> Vec<(String, PathBuf, String)> {
        // Tests never reach a real daemon, inside or outside the sandbox: a
        // board/F7 write from a test must not touch live items or levels.
        if cfg!(test) {
            return Vec::new();
        }
        let local = cm_daemon::host_id::HostId::local();
        let mut hosts = vec![local.clone()];
        hosts.extend(self.host_pool.host_ids().into_iter().filter(|h| *h != local));
        hosts
            .iter()
            .filter_map(|h| {
                let socket = self.host_pool.live_socket_path(h)?;
                Some((h.to_string(), socket, self.host_pool.operator_token_for(h)))
            })
            .collect()
    }

    fn availability_save(&mut self, cursor: usize) {
        let targets = self.owner_rpc_targets();
        if targets.is_empty() {
            if let Some(d) = self.availability.dialog.as_mut() {
                d.error = Some("No daemon is reachable".into());
            }
            return;
        }
        let level = CHOICES[cursor].0;
        let (tx, rx) = mpsc::channel();
        if let Some(d) = self.availability.dialog.as_mut() {
            d.cursor = cursor;
            d.error = None;
            d.saving = Some(rx);
        }
        std::thread::spawn(move || {
            let params = json!({
                "action": "set",
                "level": level,
                "source": "tui",
                "request_id": format!("tui-availability-{}", uuid::Uuid::new_v4()),
            });
            let mut errors = Vec::new();
            for (host, socket, token) in targets {
                match crate::client_session::rpc_messaging_board(&socket, &token, "messaging.availability", params.clone()) {
                    Ok(v) => {
                        let _ = tx.send(Ok(v));
                        return;
                    }
                    Err(e) => errors.push(format!("{host}: {e}")),
                }
            }
            let _ = tx.send(Err(errors.join("; ")));
        });
    }

    /// F7 opens the picker; while open it consumes every key so nothing
    /// reaches a session.
    pub(super) fn availability_event(&mut self, event: &CrosstermEvent) -> bool {
        let CrosstermEvent::Key(key) = event else {
            return self.availability.is_open();
        };
        if self.availability.dialog.is_none() {
            if key.code != KeyCode::F(7) || !key.modifiers.is_empty() {
                return false;
            }
            self.availability.refresh(true);
            let cursor = self.availability.cursor_for_current();
            self.availability.dialog = Some(Dialog { cursor, saving: None, error: None });
            return true;
        }
        let dialog = self.availability.dialog.as_mut().unwrap();
        if dialog.saving.is_some() {
            // Esc still closes; the in-flight request finishes on its own and
            // the status bar picks the result up from the projection.
            if key.code == KeyCode::Esc {
                self.availability.dialog = None;
            }
            return true;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => dialog.cursor = dialog.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => dialog.cursor = (dialog.cursor + 1).min(CHOICES.len() - 1),
            KeyCode::Char(c @ '1'..='5') => {
                let cursor = c as usize - '1' as usize;
                self.availability_save(cursor);
            }
            KeyCode::Enter => {
                let cursor = dialog.cursor;
                self.availability_save(cursor);
            }
            KeyCode::Esc | KeyCode::F(7) => self.availability.dialog = None,
            _ => {}
        }
        true
    }

    pub(super) fn draw_availability(&self, frame: &mut Frame, area: Rect) {
        let Some(dialog) = &self.availability.dialog else {
            return;
        };
        let width = 56.min(area.width);
        let height = (if dialog.error.is_some() { 14 } else { 12 }).min(area.height);
        let area = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Owner availability ")
            .border_style(Style::default().fg(theme::TEXT));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let current = self.availability.current["level"].as_str().unwrap_or("unset");
        let mut lines = vec![Line::from("For agents. Notifications arrive at every level."), Line::from("")];
        for (i, (level, meaning)) in CHOICES.iter().enumerate() {
            let selected = i == dialog.cursor;
            let marker = if *level == current { "\u{25cf}" } else { " " };
            let mut style = Style::default().fg(theme::TEXT);
            if selected {
                style = style.add_modifier(Modifier::REVERSED | Modifier::BOLD);
            }
            lines.push(Line::styled(format!(" {} {marker} {level:<8} {meaning}", i + 1), style));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(if dialog.saving.is_some() {
            "Saving…"
        } else {
            "1–5 or ↑/↓ + Enter set · Esc cancel"
        }));
        if let Some(error) = &dialog.error {
            lines.push(Line::styled(error.clone(), Style::default().fg(theme::ERROR)));
        }
        frame.render_widget(
            Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }),
            inner,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, level: Option<&str>, changed_at: &str) {
        let path = cm_daemon::owner_availability::projection_path(root);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, json!({"level":level,"changed_at":changed_at,"event_id":"e"}).to_string()).unwrap();
    }

    fn test_app(home: &Path) -> App {
        let _g = crate::test_support::home_lock();
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", home) };
        let app = App::new(crate::config::Config {
            api_url: String::new(),
            api_token: String::new(),
            gcp_project: String::new(),
            gcp_zone: String::new(),
            repos: HashMap::new(),
        });
        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        app
    }

    #[test]
    fn f7_opens_over_every_view_consumes_keys_and_shows_the_level() {
        use crossterm::event::{Event, KeyEvent, KeyModifiers};
        let tmp = tempfile::tempdir().unwrap();
        let mut app = test_app(tmp.path());
        app.availability = Availability::at(tmp.path().join(".cm"));
        write(&tmp.path().join(".cm"), Some("around"), "2026-01-01T00:00:00Z");
        app.availability.refresh(true);
        let key = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
        let screen = |app: &mut App| {
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 35)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            terminal.backend().buffer().content.iter().map(|c| c.symbol()).collect::<String>()
        };
        assert!(screen(&mut app).contains("\u{25cf} around"), "status bar shows the level");
        for (mode, messaging) in [(ViewMode::Sessions, false), (ViewMode::Planning, false), (ViewMode::Sessions, true)] {
            app.view_mode = mode.clone();
            app.messages.visible = messaging;
            assert!(app.handle_event(&key(KeyCode::F(7))));
            assert!(app.is_input_mode());
            assert_eq!(app.availability.dialog.as_ref().unwrap().cursor, 1, "opens on the current level");
            app.handle_event(&Event::Key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::ALT)));
            assert_eq!(app.view_mode, mode, "the picker consumes view-switch keys");
            assert!(screen(&mut app).contains("Owner availability"));
            app.handle_event(&key(KeyCode::Esc));
            assert!(!app.availability.is_open());
        }
        app.messages.visible = false;
        // A save's reply (from the worker thread): an error keeps the picker
        // open with the message; success closes it and updates the bar.
        app.handle_event(&key(KeyCode::F(7)));
        let reply = |app: &mut App, result: Result<Value, String>| {
            let (tx, rx) = mpsc::channel();
            tx.send(result).unwrap();
            app.availability.dialog.as_mut().unwrap().saving = Some(rx);
            assert!(app.handle_event(&key(KeyCode::Char('q'))), "keys are consumed while saving");
            app.availability_tick();
        };
        reply(&mut app, Err("coordinator_unavailable: hub offline".into()));
        assert_eq!(app.availability.dialog.as_ref().unwrap().error.as_deref(), Some("coordinator_unavailable: hub offline"));
        // A real reply: a daemon store's answer to the set, stamped now, so
        // it is newer than the January projection above.
        let hub = tempfile::tempdir().unwrap();
        let saved = cm_daemon::messaging::Store::open(hub.path())
            .unwrap()
            .availability("owner", &json!({"action":"set","level":"focused","request_id":"f7","source":"tui"}))
            .unwrap();
        reply(&mut app, Ok(saved));
        assert!(!app.availability.is_open());
        assert!(screen(&mut app).contains("\u{25cf} focused 0m"));
    }

    #[test]
    fn availability_status_span_reflects_projection_and_hides_when_unset() {
        let tmp = tempfile::tempdir().unwrap();
        let mut a = Availability::at(tmp.path().to_path_buf());
        assert!(a.status_span().is_none(), "unset shows nothing");
        let long_ago = "2026-01-01T00:00:00Z";
        write(tmp.path(), Some("focused"), long_ago);
        assert!(a.refresh(true));
        let span = a.status_span().unwrap();
        assert!(span.content.starts_with("\u{25cf} focused ") && span.content.trim_end().ends_with('d'), "{}", span.content);
        assert_eq!(a.cursor_for_current(), 2);
        write(tmp.path(), None, long_ago);
        a.refresh(true);
        assert!(a.status_span().is_none());
        assert_eq!(a.cursor_for_current(), 4);
    }

    #[test]
    fn a_host_reply_shows_until_a_newer_local_projection_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let mut a = Availability::at(tmp.path().to_path_buf());
        a.absorb(&json!({"level":"away","set":true,"changed_at":"2026-01-01T00:00:00Z","age_s":0}));
        a.refresh(true);
        assert_eq!(a.current["level"], "away", "no local projection: show the host's value");
        assert!(a.current["age_s"].as_i64().unwrap() > 86_400, "age recomputed from changed_at");
        write(tmp.path(), Some("on-call"), "2025-12-31T00:00:00Z");
        a.refresh(true);
        assert_eq!(a.current["level"], "away", "an older (stale) projection does not win");
        write(tmp.path(), Some("on-call"), "2026-01-02T00:00:00Z");
        a.refresh(true);
        assert_eq!(a.current["level"], "on-call", "a newer projection wins");
        a.absorb(&json!({"level":"focused","set":true,"changed_at":"2025-06-01T00:00:00Z"}));
        assert_eq!(a.remote["level"], "away", "an older host reply never replaces a newer one");
    }

    /// A host that answers `messaging.availability` with `level`.
    fn availability_host(dir: &Path, level: &str, changed_at: &str) -> PathBuf {
        use cm_daemon::control::protocol::Response;
        use cm_daemon::control::wire;
        let socket = dir.join(format!("{level}.sock"));
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let reply = json!({"owner_availability":{"level":level,"set":true,"changed_at":changed_at}});
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut stream) = conn else { return };
                let Ok(Some(req)) = wire::read_request(&mut stream) else { continue };
                assert_eq!(req.method, "messaging.availability");
                assert_eq!(req.params["action"], "get");
                let resp = Response { id: req.id, ok: true, result: Some(reply.clone()), error: None };
                let _ = wire::write_response(&mut stream, &resp);
            }
        });
        socket
    }

    /// 2026-10-18: with no local projection (laptop daemon predating
    /// availability) a level Owner set on the board never reached the bar.
    /// The bar now polls the hosts off the input thread and shows the newest;
    /// a host that fails is skipped.
    #[test]
    fn with_no_local_projection_the_bar_polls_the_hosts_and_shows_the_newest_level() {
        let tmp = tempfile::tempdir().unwrap();
        let mut a = Availability::at(tmp.path().join(".cm"));
        assert!(a.poll_due());
        let target = |name: &str, socket: PathBuf| (name.to_owned(), socket, "token".to_owned());
        let started = Instant::now();
        a.start_poll(vec![
            target("gone", tmp.path().join("missing.sock")),
            target("manager", availability_host(tmp.path(), "focused", "2026-10-18T17:40:00Z")),
            target("sessions", availability_host(tmp.path(), "away", "2026-10-18T23:22:00Z")),
        ]);
        assert!(started.elapsed() < Duration::from_millis(500), "the poll never blocks the caller");
        assert!(!a.poll_due(), "one poll at a time");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !a.finish_poll() {
            assert!(Instant::now() < deadline, "the poll never finished");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(a.current["level"], "away", "the newest change wins across hosts");
        assert!(a.status_span().unwrap().content.starts_with("\u{25cf} away"));
        assert!(a.poll.is_none() && !a.poll_due(), "the next poll waits for its interval");
    }
}
