//! F5 account usage. The only network activity is a worker while open.
use super::*;
use serde::Deserialize;
use std::{collections::BTreeMap, sync::mpsc};

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Snapshot {
    schema: u32,
    generated_at: f64,
    accounts: Vec<Account>,
    limit_history: Vec<History>,
    tokens: Tokens,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Account {
    engine: String,
    email: String,
    alias: Option<String>,
    status: Option<String>,
    pool_host: Option<String>,
    active_on: Vec<String>,
    reserved_for: Option<Vec<String>>,
    room: Option<bool>,
    limits: Vec<Limit>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Limit {
    kind: String,
    percent: f64,
    resets_at: Option<f64>,
    read_at: Option<f64>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct History {
    engine: String,
    email: String,
    kind: String,
    points: Vec<(f64, f64)>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Tokens {
    totals: Vec<Total>,
    series: Vec<Bucket>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Total {
    window: String,
    engine: String,
    host: Option<String>,
    email: Option<String>,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Bucket {
    bucket: f64,
    engine: String,
    host: Option<String>,
    email: Option<String>,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
}

pub(super) struct Usage {
    pub(super) open: bool,
    tab: usize,
    granularity: &'static str,
    engine: usize,
    by_host: bool,
    selected: usize,
    scroll: u16,
    data: Option<Snapshot>,
    error: Option<String>,
    last_poll: Option<Instant>,
    age_redraw: Option<Instant>,
    pending: Option<mpsc::Receiver<(&'static str, Result<Snapshot, String>)>>,
}
impl Default for Usage {
    fn default() -> Self {
        Self {
            open: false,
            tab: 0,
            granularity: "hour",
            engine: 0,
            by_host: false,
            selected: 0,
            scroll: 0,
            data: None,
            error: None,
            last_poll: None,
            age_redraw: None,
            pending: None,
        }
    }
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn compact(n: u64) -> String {
    for (size, suffix) in [(1_000_000_000, "B"), (1_000_000, "M"), (1_000, "K")] {
        if n >= size {
            return format!("{:.1}{suffix}", n as f64 / size as f64);
        }
    }
    n.to_string()
}
fn countdown(t: Option<f64>) -> String {
    let t = t.map(|t| t as i64);
    match t {
        None => "?".into(),
        Some(t) if t <= now() => "now".into(),
        Some(t) => {
            let m = (t - now()) / 60;
            if m >= 1440 {
                format!("{}d{}h", m / 1440, (m % 1440) / 60)
            } else if m >= 60 {
                format!("{}h{}m", m / 60, m % 60)
            } else {
                format!("{m}m")
            }
        }
    }
}
fn label(a: &Account) -> &str {
    if a.engine == "codex" {
        a.alias
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&a.email)
    } else {
        &a.email
    }
}
fn room(a: &Account) -> Option<bool> {
    if a.engine == "claude" {
        a.room
    } else if a.status.as_deref().is_some_and(|s| s != "active") {
        Some(false)
    } else if a.status.as_deref() == Some("active") && !a.limits.is_empty() {
        Some(a.limits.iter().all(|l| l.percent < 98.0))
    } else {
        None
    }
}
fn highest(a: &Account) -> f64 {
    a.limits.iter().map(|l| l.percent).fold(0.0, f64::max)
}
fn spark(points: &[f64]) -> String {
    const GLYPHS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    points
        .iter()
        .map(|v| GLYPHS[((v.clamp(0.0, 100.0) / 100.0 * 7.0).round() as usize).min(7)])
        .collect()
}
fn weekly<'a>(d: &'a Snapshot, a: &Account) -> Vec<&'a History> {
    d.limit_history
        .iter()
        .filter(|h| {
            h.engine == a.engine
                && h.email == a.email
                && (h.kind.starts_with("weekly_") || (a.engine == "codex" && h.kind == "secondary"))
        })
        .collect()
}
fn weekly_spark(d: &Snapshot, a: &Account) -> String {
    let histories = weekly(d, a);
    let mut times = histories
        .iter()
        .flat_map(|h| h.points.iter().map(|(t, _)| *t as i64))
        .filter(|t| *t >= d.generated_at as i64 - 7 * 86400 && *t <= d.generated_at as i64)
        .collect::<Vec<_>>();
    times.sort_unstable();
    times.dedup();
    let values: Vec<f64> = times
        .iter()
        .filter_map(|at| {
            histories
                .iter()
                .filter_map(|h| {
                    h.points
                        .iter()
                        .filter(|(t, _)| (*t as i64) <= *at)
                        .max_by(|a, b| a.0.total_cmp(&b.0))
                        .map(|(_, p)| *p)
                })
                .reduce(f64::max)
        })
        .collect();
    if values.len() <= 24 {
        spark(&values)
    } else {
        let sampled: Vec<_> = (0..24)
            .map(|i| values[i * (values.len() - 1) / 23])
            .collect();
        spark(&sampled)
    }
}
impl App {
    pub fn usage_tick(&mut self) {
        if !self.usage.open {
            return;
        }
        if self
            .usage
            .age_redraw
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(1))
        {
            self.usage.age_redraw = Some(Instant::now());
            self.needs_redraw = true;
        }
        if let Some(rx) = &self.usage.pending {
            match rx.try_recv() {
                Ok((g, result)) => {
                    self.usage.pending = None;
                    if g == self.usage.granularity {
                        match result {
                            Ok(d) => {
                                self.usage.data = Some(d);
                                self.usage.error = None;
                            }
                            Err(e) => self.usage.error = Some(e),
                        }
                    } else {
                        self.usage.last_poll = None;
                    }
                    self.needs_redraw = true;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.usage.pending = None;
                    self.usage.error = Some("usage worker stopped".into());
                    self.needs_redraw = true;
                }
                _ => {}
            }
        }
        if self.usage.pending.is_some()
            || self
                .usage
                .last_poll
                .is_some_and(|t| t.elapsed() < Duration::from_secs(60))
        {
            return;
        }
        self.usage.last_poll = Some(Instant::now());
        let pool = std::sync::Arc::clone(&self.host_pool);
        let host = self.global_settings.usage_host().to_owned();
        let g = self.usage.granularity;
        let (tx, rx) = mpsc::channel();
        self.usage.pending = Some(rx);
        std::thread::spawn(move || {
            let id = cm_daemon::host_id::HostId::new(&host);
            let result = (|| -> Result<Snapshot, String> {
                let socket = pool
                    .live_socket_path(&id)
                    .ok_or_else(|| format!("{host}: host unreachable"))?;
                let token = pool.operator_token_for(&id);
                let v=crate::client_session::rpc_messaging_board(&socket,&token,"usage.snapshot",serde_json::json!({"since":(now().saturating_sub(7*86400) / 3600) * 3600,"granularity":g})).map_err(|e| format!("{host}: {e:#}"))?;
                let d: Snapshot = serde_json::from_value(v)
                    .map_err(|e| format!("invalid usage snapshot: {e}"))?;
                if d.schema != 1 {
                    return Err(format!("unsupported usage schema {}", d.schema));
                }
                Ok(d)
            })();
            let _ = tx.send((g, result));
        });
    }
    pub(super) fn usage_event(&mut self, event: &CrosstermEvent) -> bool {
        let CrosstermEvent::Key(k) = event else {
            return self.usage.open;
        };
        if !self.usage.open {
            if k.code == KeyCode::F(5) && k.modifiers.is_empty() {
                self.usage.open = true;
                self.usage.last_poll = None;
                self.needs_redraw = true;
                return true;
            }
            return false;
        }
        match k.code {
            KeyCode::Esc | KeyCode::F(5) | KeyCode::Char('q') => {
                self.usage.open = false;
                self.usage.pending = None;
            }
            KeyCode::Tab => {
                self.usage.tab = (self.usage.tab + 1) % 3;
                self.usage.scroll = 0;
            }
            KeyCode::Char(c @ '1'..='3') => {
                self.usage.tab = (c as usize) - ('1' as usize);
                self.usage.scroll = 0;
            }
            KeyCode::Char('h') | KeyCode::Char('d') if self.usage.tab == 1 => {
                let g = if k.code == KeyCode::Char('h') {
                    "hour"
                } else {
                    "day"
                };
                if g != self.usage.granularity {
                    self.usage.granularity = g;
                    self.usage.last_poll = None;
                }
            }
            KeyCode::Char('e') if self.usage.tab == 1 => {
                self.usage.engine = (self.usage.engine + 1) % 3
            }
            KeyCode::Char('g') if self.usage.tab == 1 => self.usage.by_host = !self.usage.by_host,
            KeyCode::PageDown => self.usage.scroll = self.usage.scroll.saturating_add(5),
            KeyCode::PageUp => self.usage.scroll = self.usage.scroll.saturating_sub(5),
            KeyCode::Char('j') | KeyCode::Down if self.usage.tab == 2 => {
                let n = self.usage.data.as_ref().map_or(0, |d| d.accounts.len());
                self.usage.selected = (self.usage.selected + 1).min(n.saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up if self.usage.tab == 2 => {
                self.usage.selected = self.usage.selected.saturating_sub(1)
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.usage.scroll = self.usage.scroll.saturating_add(1)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.usage.scroll = self.usage.scroll.saturating_sub(1)
            }
            _ => {}
        }
        self.needs_redraw = true;
        true
    }
    pub(super) fn draw_usage(&self, frame: &mut Frame, area: Rect) {
        if !self.usage.open {
            return;
        }
        let width = area.width.min(120);
        let height = area.height.min(40);
        let area = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Usage · F5/Esc/q close ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let tabs = ["Accounts", "Tokens", "History"]
            .iter()
            .enumerate()
            .map(|(i, s)| {
                if i == self.usage.tab {
                    format!("[{} {s}]", i + 1)
                } else {
                    format!("{} {s}", i + 1)
                }
            })
            .collect::<Vec<_>>()
            .join("  ");
        let mut lines = vec![
            tabs,
            format!(
                "{} · {}",
                self.global_settings.usage_host(),
                self.usage
                    .data
                    .as_ref()
                    .map(|d| format!("data age {}", countdown_age(d.generated_at as i64)))
                    .unwrap_or_else(|| "no data".into())
            ),
        ];
        if let Some(e) = &self.usage.error {
            lines.push(format!("Error: {e}"));
        } else if self.usage.pending.is_some() {
            lines.push("Refreshing…".into());
        }
        if let Some(d) = &self.usage.data {
            match self.usage.tab {
                0 => account_lines(d, &mut lines),
                1 => token_lines(d, &self.usage, &mut lines),
                _ => history_lines(d, self.usage.selected, inner.width, &mut lines),
            }
        } else if self.usage.error.is_none() {
            lines.push("Loading usage…".into());
        }
        frame.render_widget(
            Paragraph::new(lines.join("\n"))
                .wrap(ratatui::widgets::Wrap { trim: false })
                .scroll((self.usage.scroll, 0)),
            inner,
        );
    }
}
fn countdown_age(t: i64) -> String {
    let s = (now() - t).max(0);
    if s >= 86400 {
        format!("{}d", s / 86400)
    } else if s >= 3600 {
        format!("{}h", s / 3600)
    } else if s >= 60 {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    }
}
fn account_lines(d: &Snapshot, out: &mut Vec<String>) {
    let mut accounts: Vec<_> = d.accounts.iter().collect();
    accounts.sort_by(|a, b| {
        a.active_on
            .is_empty()
            .cmp(&b.active_on.is_empty())
            .then_with(|| {
                let rank = |a: &Account| match room(a) {
                    Some(true) => 0,
                    None => 1,
                    Some(false) => 2,
                };
                rank(a).cmp(&rank(b))
            })
            .then_with(|| highest(a).total_cmp(&highest(b)))
            .then_with(|| a.email.cmp(&b.email))
    });
    out.push(format!(
        "Room: Claude {} / {} · Codex {} / {}",
        accounts
            .iter()
            .filter(|a| a.engine == "claude" && room(a) == Some(true))
            .count(),
        accounts.iter().filter(|a| a.engine == "claude").count(),
        accounts
            .iter()
            .filter(|a| a.engine == "codex" && room(a) == Some(true))
            .count(),
        accounts.iter().filter(|a| a.engine == "codex").count()
    ));
    for engine in ["claude", "codex"] {
        out.push(format!("\n{}", engine.to_uppercase()));
        for a in accounts.iter().filter(|a| a.engine == engine) {
            out.push(format!(
                "{}  {}  hosts:{}  reserved:{}{}  {}",
                label(a),
                match room(a) {
                    Some(true) => "ROOM",
                    Some(false) => "NO ROOM",
                    None => "room ?",
                },
                if a.active_on.is_empty() {
                    "-".into()
                } else {
                    a.active_on.join(",")
                },
                a.reserved_for
                    .as_ref()
                    .filter(|v| !v.is_empty())
                    .map(|v| v.join(","))
                    .unwrap_or_else(|| "-".into()),
                a.pool_host
                    .as_ref()
                    .map(|h| format!("  pool:{h}"))
                    .unwrap_or_default(),
                weekly_spark(d, a)
            ));
            for l in &a.limits {
                let filled = ((l.percent.clamp(0.0, 100.0) / 10.0).round() as usize).min(10);
                out.push(format!(
                    "  {} [{}{}] {:.0}% reset {}",
                    l.kind,
                    "█".repeat(filled),
                    "░".repeat(10 - filled),
                    l.percent,
                    countdown(l.resets_at)
                ));
                let _ = l.read_at;
            }
        }
    }
}
fn token_lines(d: &Snapshot, u: &Usage, out: &mut Vec<String>) {
    let filter = match u.engine {
        1 => Some("claude"),
        2 => Some("codex"),
        _ => None,
    };
    out.push(format!(
        "h/d: {} · e: {} · g: {} · j/k scroll",
        u.granularity,
        filter.unwrap_or("all"),
        if u.by_host { "host" } else { "account" }
    ));
    for window in ["24h", "7d", "30d", "all"] {
        out.push(format!("\n{window}"));
        for t in d
            .tokens
            .totals
            .iter()
            .filter(|t| t.window == window && filter.is_none_or(|f| t.engine == f))
        {
            let dimension = match (&t.host, &t.email) {
                (None, None) => "total".into(),
                (Some(h), None) => format!("host {h}"),
                (None, Some(a)) => format!("account {a}"),
                _ => continue,
            };
            out.push(format!(
                " {} {dimension}: in+out {}  cache {}",
                t.engine,
                compact(t.input.saturating_add(t.output)),
                compact(t.cache_read.saturating_add(t.cache_write))
            ));
        }
    }
    out.push("\nOver time: █ input+output  ░ cache".into());
    if d.tokens.series.is_empty() {
        out.push("No history yet".into());
    }
    let mut groups: BTreeMap<(i64, String), (u64, u64)> = BTreeMap::new();
    for b in &d.tokens.series {
        if filter.is_some_and(|f| b.engine != f) {
            continue;
        }
        let name = if u.by_host {
            b.host.as_deref()
        } else {
            b.email.as_deref()
        }
        .unwrap_or("unknown");
        let entry = groups
            .entry((b.bucket as i64, format!("{} {name}", b.engine)))
            .or_default();
        entry.0 = entry.0.saturating_add(b.input.saturating_add(b.output));
        entry.1 = entry
            .1
            .saturating_add(b.cache_read.saturating_add(b.cache_write));
    }
    let max = groups
        .values()
        .map(|(a, b)| a.saturating_add(*b))
        .max()
        .unwrap_or(1)
        .max(1);
    for ((t, name), (io, cache)) in groups {
        let timestamp = chrono::DateTime::from_timestamp(t, 0)
            .map(|date| {
                date.format(if u.granularity == "day" {
                    "%Y-%m-%d"
                } else {
                    "%m-%d %H:%M"
                })
                .to_string()
            })
            .unwrap_or_else(|| t.to_string());
        let a = ((io as f64 / max as f64) * 24.0).round() as usize;
        let b = ((cache as f64 / max as f64) * 24.0).round() as usize;
        out.push(format!(
            "{timestamp} {name}: {}{} {} + cache {}",
            "█".repeat(a),
            "░".repeat(b),
            compact(io),
            compact(cache)
        ));
    }
}
fn history_lines(d: &Snapshot, selected: usize, width: u16, out: &mut Vec<String>) {
    out.push("j/k select account · last 7 days · 98% switch line".into());
    let Some(a) = d
        .accounts
        .get(selected.min(d.accounts.len().saturating_sub(1)))
    else {
        out.push("No accounts · no history yet".into());
        return;
    };
    if !d
        .limit_history
        .iter()
        .any(|h| h.engine == a.engine && h.email == a.email && !h.points.is_empty())
    {
        out.push("No history yet".into());
    }
    out.push(format!(
        "{} {}/{}",
        label(a),
        selected + 1,
        d.accounts.len()
    ));
    let chart_width = (width as usize).saturating_sub(6).clamp(1, 80);
    let end = d.generated_at as i64;
    let start = end - 7 * 86400;
    let mut grid = vec![vec![' '; chart_width]; 11];
    for x in 0..chart_width {
        grid[0][x] = '─';
    }
    let mut kinds = Vec::new();
    for h in d
        .limit_history
        .iter()
        .filter(|h| h.engine == a.engine && h.email == a.email)
    {
        let mark = char::from_digit((kinds.len() % 9 + 1) as u32, 10).unwrap();
        kinds.push(format!("{mark} {}", h.kind));
        for &(t, p) in &h.points {
            let t = t as i64;
            if t < start || t > end {
                continue;
            }
            let x = (((t - start) as f64 / (7 * 86400) as f64)
                * (chart_width.saturating_sub(1) as f64)) as usize;
            let y = (10.0 - (p.clamp(0.0, 100.0) / 10.0)).round() as usize;
            grid[y.min(10)][x] = mark;
        }
    }
    for (i, row) in grid.into_iter().enumerate() {
        out.push(format!(
            "{:>3}% {}",
            if i == 0 { 98 } else { 100 - i * 10 },
            row.into_iter().collect::<String>()
        ));
    }
    out.push(kinds.join("  "));
    out.push("7 days ago → now".into());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    fn app() -> App {
        let tmp = tempfile::tempdir().unwrap();
        let _lock = crate::test_support::home_lock();
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
    fn key(c: KeyCode) -> CrosstermEvent {
        CrosstermEvent::Key(KeyEvent::new(c, KeyModifiers::NONE))
    }
    fn screen(app: &mut App, w: u16, h: u16) -> String {
        let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        let b = t.backend().buffer();
        (0..h)
            .map(|y| (0..w).map(|x| b[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }
    #[test]
    fn fixture_and_tabs_render() {
        let data: Snapshot =
            serde_json::from_str(include_str!("../../tests/fixtures/usage-snapshot.json")).unwrap();
        assert_eq!(data.schema, 1);
        assert_eq!(data.accounts.len(), 3);
        assert_eq!(data.tokens.series.len(), 2);
        let numeric_epochs = include_str!("../../tests/fixtures/usage-snapshot.json")
            .replace("1795400000", "1795400000.0")
            .replace("1795410000", "1795410000.0")
            .replace("1795300000", "1795300000.0")
            .replace("1795392000", "1795392000.0");
        assert!(serde_json::from_str::<Snapshot>(&numeric_epochs).is_ok());
        assert_eq!(weekly_spark(&data, &data.accounts[0]), "▁▂");
        let mut a = app();
        a.usage.data = Some(data);
        a.handle_event(&key(KeyCode::F(5)));
        let s = screen(&mut a, 120, 35);
        for x in [
            "Usage",
            "a@example.com",
            "weekly_fable",
            "802",
            "NO ROOM",
            "cm-sessions",
        ] {
            assert!(s.contains(x), "missing {x}: {s}");
        }
        a.handle_event(&key(KeyCode::Char('2')));
        let s = screen(&mut a, 120, 35);
        for x in ["24h", "7d", "30d", "all", "cache", "1.2M"] {
            assert!(s.contains(x), "missing {x}: {s}");
        }
        a.handle_event(&key(KeyCode::Char('d')));
        assert_eq!(a.usage.granularity, "day");
        a.handle_event(&key(KeyCode::Char('e')));
        a.handle_event(&key(KeyCode::Char('g')));
        assert_eq!(a.usage.engine, 1);
        assert!(a.usage.by_host);
        a.handle_event(&key(KeyCode::Char('3')));
        a.handle_event(&key(KeyCode::Char('j')));
        assert_eq!(a.usage.selected, 1);
        a.handle_event(&key(KeyCode::Char('k')));
        let s = screen(&mut a, 120, 35);
        assert!(s.contains("98%"));
        assert!(s.contains("weekly_all"));
        a.handle_event(&key(KeyCode::Char('q')));
        assert!(!a.usage.open);
    }
    #[test]
    fn sparkline_spans_the_entire_week() {
        let mut d = Snapshot {
            generated_at: 1_795_400_000.0,
            ..Snapshot::default()
        };
        let account = Account {
            engine: "claude".into(),
            email: "a@example.com".into(),
            ..Account::default()
        };
        d.limit_history.push(History {
            engine: account.engine.clone(),
            email: account.email.clone(),
            kind: "weekly_all".into(),
            points: (0..100)
                .map(|i| (d.generated_at - 7.0 * 86400.0 + i as f64 * 6000.0, i as f64))
                .collect(),
        });
        let line = weekly_spark(&d, &account);
        assert_eq!(line.chars().count(), 24);
        assert!(line.starts_with('▁') && line.ends_with('█'));
    }
    #[test]
    fn p1_fixture_uses_contract_fields() {
        let d: Snapshot =
            serde_json::from_str(include_str!("../../tests/fixtures/usage-p1.json")).unwrap();
        assert_eq!(d.schema, 1);
        assert_eq!(d.accounts.len(), 2);
        assert_eq!(
            d.tokens.series[0].email.as_deref(),
            Some("alice@example.com")
        );
    }
    #[test]
    fn empty_and_error_render_at_narrow_width() {
        let mut a = app();
        a.usage.open = true;
        a.usage.data=Some(serde_json::from_str(r#"{"schema":1,"generated_at":0,"accounts":[],"limit_history":[],"tokens":{"totals":[],"series":[]}}"#).unwrap());
        a.usage.error = Some("not_hub".into());
        let s = screen(&mut a, 35, 15);
        assert!(s.contains("not_hub"));
        a.usage.tab = 2;
        assert!(screen(&mut a, 35, 20).contains("No accounts"));
        let _ = screen(&mut a, 3, 2);
    }
}
