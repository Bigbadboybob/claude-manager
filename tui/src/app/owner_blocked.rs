//! Items blocked on Owner (doc/items-board.md, status `blocked_on_owner`).
//!
//! Every 20 s the TUI asks a daemon for `board.owner_blocked` (one small,
//! filtered read of the shared planning API, not a board poll) and marks each
//! holder's sidebar row and workspace header with an amber diamond that
//! alternates ◇/◆ about once a second plus a bold `OWNER` tag, ranked just
//! below a pending ⚑ alert. The status bar counts the items.

use super::App;
use crate::theme;
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_secs(20);
const BLINK_MS: u128 = 1000;

#[derive(Default)]
pub(crate) struct OwnerBlocked {
    /// `[{board, n, title, decision, since, holders: [{session_uid, …}]}]`
    pub(crate) items: Vec<Value>,
    sessions: HashSet<String>,
    last_poll: Option<Instant>,
    rx: Option<mpsc::Receiver<Result<Value, String>>>,
    last_frame: u128,
}

impl OwnerBlocked {
    pub(crate) fn set_items(&mut self, items: Vec<Value>) {
        self.sessions = items
            .iter()
            .flat_map(|i| i["holders"].as_array().into_iter().flatten())
            .filter_map(|h| h["session_uid"].as_str().map(str::to_owned))
            .collect();
        self.items = items;
    }

    pub(crate) fn count(&self) -> usize {
        self.items.len()
    }
}

pub(crate) fn owner_style() -> Style {
    Style::default().fg(theme::OWNER_BLOCKED).add_modifier(Modifier::BOLD)
}

/// The bold amber ` OWNER` tag that follows a marked row's name.
pub(crate) fn owner_tag() -> Span<'static> {
    Span::styled(" OWNER", owner_style())
}

impl App {
    /// True when `uid` holds at least one item blocked on Owner.
    pub(crate) fn owner_blocked_session(&self, uid: &str) -> bool {
        self.owner_blocked.sessions.contains(uid)
    }

    /// ◇/◆, alternating about once a second (movement, never a hue cycle).
    pub(crate) fn owner_indicator(&self) -> (&'static str, Style) {
        let on = (self.start_time.elapsed().as_millis() / BLINK_MS) % 2 == 1;
        (if on { "\u{25c6}" } else { "\u{25c7}" }, owner_style())
    }

    /// Drain the poll reply, keep the diamond moving, and start the next poll.
    pub fn owner_blocked_tick(&mut self) {
        if let Some(rx) = &self.owner_blocked.rx {
            match rx.try_recv() {
                Ok(result) => {
                    self.owner_blocked.rx = None;
                    // A failed poll keeps the last known list: a daemon blip
                    // must not make a waiting decision silently disappear.
                    if let Ok(v) = result {
                        let items = v.as_array().cloned().unwrap_or_default();
                        self.owner_blocked.set_items(items);
                        self.needs_redraw = true;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => self.owner_blocked.rx = None,
            }
        }
        if self.owner_blocked.count() > 0 {
            let frame = self.start_time.elapsed().as_millis() / BLINK_MS;
            if frame != self.owner_blocked.last_frame {
                self.owner_blocked.last_frame = frame;
                self.needs_redraw = true;
            }
        }
        let due = self.owner_blocked.last_poll.is_none_or(|t| t.elapsed() >= POLL);
        if due && self.owner_blocked.rx.is_none() {
            self.owner_blocked.last_poll = Some(Instant::now());
            let targets = self.owner_rpc_targets();
            let (tx, rx) = mpsc::channel();
            self.owner_blocked.rx = Some(rx);
            std::thread::spawn(move || {
                // Items live in the shared planning API: any one daemon
                // answers for every board, so the first success wins.
                let mut last = "No daemon is reachable".to_owned();
                for (host, socket, token) in targets {
                    match crate::client_session::rpc_messaging_board(
                        &socket,
                        &token,
                        "board.owner_blocked",
                        serde_json::json!({}),
                    ) {
                        Ok(v) => {
                            let _ = tx.send(Ok(v));
                            return;
                        }
                        Err(e) => last = format!("{host}: {e}"),
                    }
                }
                let _ = tx.send(Err(last));
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::make_simple_session_with_uid;
    use super::super::*;
    use super::*;
    use crate::session::Session;
    use serde_json::json;
    use std::collections::HashMap;

    fn app_with(uids: &[&str]) -> App {
        let _g = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", tmp.path()) };
        let mut app = App::new(crate::config::Config {
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
        app.keybinding_helper_visible = false;
        let mut ws = Workspace {
            id: "ws-1".into(),
            name: "swarm".into(),
            is_closed: false,
            is_cloud: false,
            repo_url: None,
            worktree_path: None,
            main_repo_path: None,
            worker_vm: None,
            worker_zone: None,
            host_id: cm_daemon::host_id::HostId::local(),
            color: None,
            pinned: false,
            sessions: vec![],
            tombstones: vec![],
        };
        for uid in uids {
            let session = Session::new("/bin/true", &[], 80, 24, None, HashMap::new(), None).unwrap();
            ws.sessions.push(make_simple_session_with_uid((*uid).into(), uid, "codex", session, None));
        }
        app.workspaces = vec![ws];
        app
    }

    fn render(app: &mut App, width: u16, sidebar_only: bool) -> String {
        use ratatui::{backend::TestBackend, Terminal};
        let mut terminal = Terminal::new(TestBackend::new(width, 12)).unwrap();
        if sidebar_only {
            terminal.draw(|f| app.draw_session_list(f, f.area())).unwrap();
        } else {
            terminal.draw(|f| app.draw(f)).unwrap();
        }
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn sidebar_marks_holder_row_and_workspace_header_and_status_bar_counts() {
        let mut app = app_with(&["lane-a", "lane-b"]);
        let plain = render(&mut app, 60, true);
        assert!(!plain.contains("OWNER"), "{plain}");
        app.owner_blocked.set_items(vec![
            json!({"board": "sfd", "n": 7, "title": "gpu", "holders": [{"session_uid": "lane-a"}]}),
        ]);
        // Status view: the holder's row only.
        let flat = render(&mut app, 60, true);
        let rows: Vec<&str> = flat.lines().filter(|l| l.contains("OWNER")).collect();
        assert_eq!(rows.len(), 1, "{flat}");
        assert!(rows[0].contains("lane-a"), "{flat}");
        // Task view: the workspace header too.
        app.sidebar_view = SidebarView::Task;
        let marked = render(&mut app, 60, true);
        let rows: Vec<&str> = marked.lines().filter(|l| l.contains("OWNER")).collect();
        assert_eq!(rows.len(), 2, "workspace header + lane-a row:\n{marked}");
        assert!(rows.iter().any(|r| r.contains("swarm") && !r.contains("lane")), "{marked}");
        assert!(rows.iter().any(|r| r.contains("lane-a")), "{marked}");
        assert!(!marked.lines().any(|l| l.contains("lane-b") && l.contains("OWNER")));
        assert!(rows.iter().all(|r| r.contains('\u{25c7}') || r.contains('\u{25c6}')), "{marked}");
        app.control_bound = true; // no degraded-mode banner over the status bar
        let full = render(&mut app, 140, false);
        assert!(full.contains("\u{25c6}1 OWNER"), "status bar count:\n{full}");
    }

    #[test]
    fn a_pending_alert_outranks_the_owner_diamond() {
        let mut app = app_with(&["lane-a"]);
        app.owner_blocked.set_items(vec![
            json!({"board": "sfd", "n": 7, "holders": [{"session_uid": "lane-a"}]}),
        ]);
        app.alerts.insert("lane-a".into(), "please look".into());
        let out = render(&mut app, 60, true);
        let row = out.lines().find(|l| l.contains("lane-a")).unwrap();
        let glyph_cell = row.trim_start().chars().next().unwrap();
        assert!(glyph_cell != '\u{25c7}' && glyph_cell != '\u{25c6}', "{row}");
        assert!(row.contains("OWNER"), "the tag still shows: {row}");
    }

    #[test]
    fn the_diamond_alternates_about_once_a_second() {
        let app = app_with(&[]);
        let first = app.owner_indicator().0;
        std::thread::sleep(std::time::Duration::from_millis(1050));
        assert_ne!(app.owner_indicator().0, first);
        assert_eq!(app.owner_indicator().1, owner_style());
    }

    #[test]
    fn holders_of_owner_blocked_items_are_marked() {
        let mut ob = OwnerBlocked::default();
        ob.set_items(vec![
            json!({"board": "sfd", "n": 3, "holders": [{"session_uid": "u1"}, {"session_uid": "u2"}]}),
            json!({"board": "sfd", "n": 4, "holders": []}),
        ]);
        assert_eq!(ob.count(), 2);
        assert!(ob.sessions.contains("u1") && ob.sessions.contains("u2"));
        assert!(!ob.sessions.contains("u3"));
        ob.set_items(vec![]);
        assert!(ob.sessions.is_empty());
    }
}
