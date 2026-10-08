//! Status-bar indicator of Owner's unread DMs and mentions (`✉2 @1`),
//! visible in every view without opening Messages.
//!
//! A low-rate off-thread poll of the local daemon's `messaging.attention`
//! (one store pass, no response decorations), plus an immediate refresh when
//! the Messages pane closes, so reading clears it promptly. Daemons that
//! predate the method answer with the `attention.unread` total from a cheap
//! `messaging.dms` call instead; the indicator then shows that total as DMs.
use super::*;
use serde_json::{json, Value};
use std::sync::mpsc;

const POLL: Duration = Duration::from_secs(15);

#[derive(Default)]
pub(super) struct Unread {
    pub(super) dms: u64,
    pub(super) mentions: u64,
    rx: Option<mpsc::Receiver<Result<Value, String>>>,
    polled: Option<Instant>,
    messages_was_visible: bool,
}

impl Unread {
    pub(super) fn total(&self) -> u64 {
        self.dms + self.mentions
    }

    /// Apply a `messaging.attention` reply (or an old daemon's `attention`
    /// decoration). Returns true when the counts changed.
    fn accept(&mut self, v: &Value) -> bool {
        // messaging.counts answers at the top level; attention nests it.
        let a = if v["attention"].is_object() { &v["attention"] } else { v };
        if !["dms", "mentions", "unread"].iter().any(|k| a[*k].is_u64()) {
            return false;
        }
        let (dms, mentions) = match (a["dms"].as_u64(), a["mentions"].as_u64()) {
            (Some(d), Some(m)) => (d, m),
            _ => (a["unread"].as_u64().unwrap_or(0), 0),
        };
        let changed = (dms, mentions) != (self.dms, self.mentions);
        self.dms = dms;
        self.mentions = mentions;
        changed
    }
}

/// The indicator at decreasing widths: `✉2 @1 F8`, `✉2 @1`, `✉3`, `✉`.
/// Empty when there is nothing unread.
pub(super) fn indicator_variants(dms: u64, mentions: u64) -> Vec<String> {
    if dms + mentions == 0 {
        return Vec::new();
    }
    let mut parts = Vec::new();
    if dms > 0 {
        parts.push(format!("\u{2709}{dms}"));
    }
    if mentions > 0 {
        parts.push(format!("@{mentions}"));
    }
    let counts = parts.join(" ");
    vec![
        format!(" {counts} F8 "),
        format!(" {counts} "),
        format!(" \u{2709}{} ", dms + mentions),
        " \u{2709} ".into(),
    ]
}

impl Unread {
    /// Counts carried by a mark-read or react reply.
    pub(super) fn accept_counts(&mut self, counts: &Value) -> bool {
        self.accept(counts)
    }
}

impl App {
    pub fn unread_tick(&mut self) {
        if let Some(result) = self.unread.rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.unread.rx = None;
            match result {
                Ok(v) => {
                    if self.unread.accept(&v) {
                        self.needs_redraw = true;
                    }
                }
                Err(e) => eprintln!("cm-tui: unread messages indicator: {e}"),
            }
        }
        // Refresh right after Messages closes: reading clears the count.
        let closed_now = self.unread.messages_was_visible && !self.messages.visible;
        self.unread.messages_was_visible = self.messages.visible;
        let due = closed_now || self.unread.polled.is_none_or(|t| t.elapsed() >= POLL);
        if !due || self.unread.rx.is_some() {
            return;
        }
        let host = cm_daemon::host_id::HostId::local();
        let Some(socket) = self.host_pool.live_socket_path(&host) else {
            return;
        };
        if cfg!(test) {
            return;
        }
        let token = self.host_pool.operator_token_for(&host);
        self.unread.polled = Some(Instant::now());
        let (tx, rx) = mpsc::channel();
        self.unread.rx = Some(rx);
        std::thread::spawn(move || {
            let call = |m: &str, p: Value| {
                crate::client_session::rpc_messaging_board(&socket, &token, m, p).map_err(|e| e.to_string())
            };
            // Cursor counts (one cheap call), then the older attention
            // summary, then the total an old daemon's responses carry.
            let unsupported = |e: &str| {
                let l = e.to_lowercase();
                l.contains("not implemented") || (l.contains("unknown") && l.contains("method"))
            };
            let result = call("messaging.counts", json!({}))
                .or_else(|e| if unsupported(&e) { call("messaging.attention", json!({})) } else { Err(e) })
                .or_else(|e| if unsupported(&e) { call("messaging.dms", json!({"limit": 1})) } else { Err(e) });
            let _ = tx.send(result);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> App {
        let tmp = tempfile::tempdir().unwrap();
        let _g = crate::test_support::home_lock();
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", tmp.path()) };
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

    fn status_bar(app: &mut App, width: u16) -> String {
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 12)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let y = buf.area.height - 1;
        (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    #[test]
    fn status_bar_shows_unread_in_every_view_and_shortens_instead_of_pushing() {
        let mut app = test_app();
        app.control_bound = true; // no real control socket in tests
        assert!(!status_bar(&mut app, 120).contains('\u{2709}'), "hidden when zero");
        app.unread.dms = 2;
        app.unread.mentions = 1;
        for mode in [ViewMode::Sessions, ViewMode::Planning] {
            app.view_mode = mode;
            let wide = status_bar(&mut app, 120);
            assert!(wide.trim_end().ends_with("\u{2709}2 @1 F8"), "{wide:?}");
            assert!(wide.contains("0r 0b 0q"), "task counts stay: {wide:?}");
        }
        // Narrow: the indicator shortens (never the task counts).
        // The bar's own fixed text ("○ claude-manager … 0r 0b 0q") needs 28
        // cells; from there the indicator only takes what is left.
        let bar = |w: u16, app: &mut App| status_bar(app, w);
        assert!(bar(40, &mut app).trim_end().ends_with("\u{2709}2 @1 F8"));
        assert!(bar(36, &mut app).trim_end().ends_with("\u{2709}2 @1"));
        assert!(bar(33, &mut app).trim_end().ends_with("\u{2709}3"));
        assert!(bar(31, &mut app).trim_end().ends_with("\u{2709}"));
        assert!(!bar(30, &mut app).contains('\u{2709}'), "no room: hidden");
        for w in [40u16, 36, 33, 31, 30] {
            assert!(bar(w, &mut app).contains("0r 0b 0q"), "task counts never pushed off at {w}");
        }
    }

    #[test]
    fn background_cleanup_and_unread_share_a_narrow_bar() {
        let mut app = test_app();
        app.control_bound = true;
        let host = cm_daemon::host_id::HostId::local();
        let job = |n: &str| {
            super::worktree_cleanup::Menu::new(
                app.host_pool.clone(),
                vec![host.clone()],
                host.clone(),
                format!("ws-{n}"),
                None,
                None,
                n.into(),
                false,
            )
        };
        let jobs = vec![job("a"), job("b")];
        app.cleanup_jobs = jobs;
        app.unread.dms = 2;
        app.unread.mentions = 1;
        // Base bar (28) + "⟲ cleanup: 2 jobs running " (26): the cleanup item
        // keeps its place and the indicator takes only what is left.
        let bar = |w: u16, app: &mut App| status_bar(app, w);
        let cleanup = "\u{27f2} cleanup: 2 jobs running";
        let full = bar(64, &mut app);
        assert!(full.contains(cleanup) && full.trim_end().ends_with("\u{2709}2 @1 F8"), "{full:?}");
        let tight = bar(58, &mut app);
        assert!(tight.contains(cleanup) && tight.trim_end().ends_with("\u{2709}3"), "{tight:?}");
        let glyph = bar(57, &mut app);
        assert!(glyph.contains(cleanup) && glyph.trim_end().ends_with("\u{2709}"), "{glyph:?}");
        let none = bar(54, &mut app);
        assert!(none.contains(cleanup) && !none.contains('\u{2709}'), "{none:?}");
        for w in [64u16, 58, 57, 54] {
            assert!(bar(w, &mut app).contains("0r 0b 0q"), "task counts kept at {w}");
        }
    }

    #[test]
    fn opening_messages_with_unread_starts_on_the_inbox() {
        use crossterm::event::{Event, KeyEvent, KeyModifiers};
        let mut app = test_app();
        app.unread.dms = 1;
        assert!(app.handle_event(&Event::Key(KeyEvent::new(KeyCode::F(8), KeyModifiers::NONE))));
        assert!(app.messages.visible);
        assert_eq!(app.messages.target_for_test(), &json!({"inbox": true}));
    }

    #[test]
    fn indicator_counts_split_and_degrade_with_width() {
        assert!(indicator_variants(0, 0).is_empty(), "hidden when zero");
        assert_eq!(indicator_variants(2, 1), vec![" \u{2709}2 @1 F8 ", " \u{2709}2 @1 ", " \u{2709}3 ", " \u{2709} "]);
        assert_eq!(indicator_variants(0, 4)[1], " @4 ");
        let mut u = Unread::default();
        assert!(u.accept(&json!({"attention":{"unread":5,"dms":2,"mentions":1}})));
        assert_eq!((u.dms, u.mentions), (2, 1));
        assert!(!u.accept(&json!({"attention":{"unread":5,"dms":2,"mentions":1}})), "unchanged");
        // messaging.counts answers at the top level.
        assert!(u.accept(&json!({"conversations":{},"dms":4,"mentions":2,"unread":6})));
        assert_eq!((u.dms, u.mentions), (4, 2));
        // An older daemon only reports the total.
        assert!(u.accept(&json!({"attention":{"unread":3}})));
        assert_eq!((u.dms, u.mentions), (3, 0));
        assert!(!u.accept(&json!({"items":[]})), "no attention object: keep the last counts");
    }
}
