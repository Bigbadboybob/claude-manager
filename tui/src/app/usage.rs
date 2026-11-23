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
    outlook: Option<Vec<Outlook>>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Account {
    engine: String,
    email: String,
    alias: Option<String>,
    status: Option<String>,
    pool_host: Option<String>,
    pool_hosts: Option<Vec<String>>,
    pool_status: Option<BTreeMap<String, String>>,
    active_on: Vec<String>,
    reserved_for: Option<Vec<String>>,
    room: Option<bool>,
    limits: Vec<Limit>,
    resets: Option<Vec<ResetCredit>>,
    projection: Option<Projection>,
    plan_type: Option<String>,
    reservation_note: Option<String>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Limit {
    kind: String,
    percent: f64,
    resets_at: Option<f64>,
    read_at: Option<f64>,
    window_minutes: Option<u64>,
    switch_at: Option<f64>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ResetCredit {
    #[serde(rename = "type")]
    kind: String,
    count: u64,
    expires_at: Option<f64>,
    as_of: Option<f64>,
    usable_now: Option<bool>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Projection {
    kind: String,
    rate_per_hour: f64,
    basis_hours: f64,
    at_switch: Option<f64>,
    at_reset_percent: f64,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Outlook {
    engine: String,
    accounts: u64,
    accounts_with_room: u64,
    room_weeks: f64,
    burn_per_hour: f64,
    dry_at: Option<f64>,
    refills: Vec<Refill>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Refill {
    at: f64,
    email: String,
    frees: f64,
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
    requests: u64,
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
    expanded: bool,
    detail_open: bool,
    help: bool,
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
            expanded: false,
            detail_open: false,
            help: false,
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
fn since_for_granularity(epoch: i64, granularity: &str) -> i64 {
    let days = if granularity == "day" { 30 } else { 7 };
    (epoch.saturating_sub(days * 86_400) / 3_600) * 3_600
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
fn room(a: &Account) -> Option<bool> {
    if a.engine == "claude" {
        a.room
    } else if a.status.as_deref().is_some_and(|s| s != "active") {
        Some(false)
    } else if a.status.as_deref() == Some("active") && !a.limits.is_empty() {
        Some(
            a.limits
                .iter()
                .all(|l| l.percent < l.switch_at.unwrap_or(98.0)),
        )
    } else {
        None
    }
}
fn spark(points: &[f64]) -> String {
    const GLYPHS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    points
        .iter()
        .map(|v| GLYPHS[((v.clamp(0.0, 100.0) / 100.0 * 7.0).round() as usize).min(7)])
        .collect()
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
                                let previous = self.usage.data.as_ref().and_then(|old| {
                                    sorted_accounts(old).get(self.usage.selected).map(|a| {
                                        (a.engine.clone(), a.email.clone(), a.pool_host.clone())
                                    })
                                });
                                if let Some((engine, email, pool)) = previous {
                                    self.usage.selected = sorted_accounts(&d)
                                        .iter()
                                        .position(|a| {
                                            a.engine == engine
                                                && a.email == email
                                                && a.pool_host == pool
                                        })
                                        .unwrap_or(0);
                                } else {
                                    self.usage.selected =
                                        self.usage.selected.min(d.accounts.len().saturating_sub(1));
                                }
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
                let v = crate::client_session::rpc_messaging_board(
                    &socket,
                    &token,
                    "usage.snapshot",
                    serde_json::json!({"since":since_for_granularity(now(),g),"granularity":g}),
                )
                .map_err(|e| format!("{host}: {e:#}"))?;
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
                self.usage.expanded = false;
                self.usage.detail_open = false;
                self.usage.help = false;
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
            KeyCode::Char('?') => self.usage.help = !self.usage.help,
            KeyCode::Tab => {
                self.usage.tab = (self.usage.tab + 1) % 2;
                self.usage.scroll = 0;
                self.usage.help = false;
            }
            KeyCode::Char(c @ '1'..='2') => {
                self.usage.tab = c as usize - '1' as usize;
                self.usage.scroll = 0;
                self.usage.help = false;
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
            KeyCode::Char('x') if self.usage.tab == 0 => self.usage.expanded = !self.usage.expanded,
            KeyCode::Enter | KeyCode::Backspace if self.usage.tab == 0 => {
                self.usage.detail_open = !self.usage.detail_open;
                self.usage.scroll = 0;
            }
            KeyCode::Char('g') if self.usage.tab == 0 => {
                self.usage.selected = 0;
                self.usage.scroll = 0;
            }
            KeyCode::Char('G') if self.usage.tab == 0 => {
                self.usage.selected = self
                    .usage
                    .data
                    .as_ref()
                    .map_or(0, |d| d.accounts.len().saturating_sub(1));
                self.usage.scroll = 0;
            }
            KeyCode::Char('j') | KeyCode::Down if self.usage.tab == 0 => {
                let n = self.usage.data.as_ref().map_or(0, |d| d.accounts.len());
                self.usage.selected = (self.usage.selected + 1).min(n.saturating_sub(1));
                self.usage.scroll = 0;
            }
            KeyCode::Char('k') | KeyCode::Up if self.usage.tab == 0 => {
                self.usage.selected = self.usage.selected.saturating_sub(1);
                self.usage.scroll = 0;
            }
            KeyCode::PageDown => self.usage.scroll = self.usage.scroll.saturating_add(5),
            KeyCode::PageUp => self.usage.scroll = self.usage.scroll.saturating_sub(5),
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
        render_usage(frame, area, &self.usage, self.global_settings.usage_host());
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
// A missing Codex window duration is never guessed from primary/secondary.
fn window_label(a: &Account, l: &Limit) -> String {
    if a.engine == "claude" {
        return match l.kind.as_str() {
            "five_hour" => "5h".into(),
            "weekly_all" => "Week".into(),
            k if k.starts_with("weekly_") => format!("{} week", &k[7..]),
            _ => l.kind.clone(),
        };
    }
    match l.window_minutes {
        Some(300) => "5h".into(),
        Some(10080) => "Week".into(),
        Some(m) if m % 1440 == 0 => format!("{}d", m / 1440),
        Some(m) if m % 60 == 0 => format!("{}h", m / 60),
        Some(m) => format!("{m}m"),
        None => "Window ?".into(),
    }
}
fn is_week(a: &Account, l: &Limit) -> bool {
    if a.engine == "claude" {
        l.kind.starts_with("weekly_")
    } else {
        l.window_minutes.is_some_and(|m| m >= 10080)
    }
}
fn is_five(a: &Account, l: &Limit) -> bool {
    if a.engine == "claude" {
        l.kind == "five_hour"
    } else {
        l.window_minutes == Some(300)
    }
}
fn switch_at(l: &Limit) -> f64 {
    l.switch_at.unwrap_or(98.0)
}
fn weekly_limit(a: &Account) -> Option<&Limit> {
    if let Some(p) = &a.projection {
        if let Some(l) = a.limits.iter().find(|l| l.kind == p.kind && is_week(a, l)) {
            return Some(l);
        }
    }
    a.limits.iter().filter(|l| is_week(a, l)).max_by(|a, b| {
        (a.percent / switch_at(a).max(1.0)).total_cmp(&(b.percent / switch_at(b).max(1.0)))
    })
}
fn five_limit(a: &Account) -> Option<&Limit> {
    a.limits.iter().find(|l| is_five(a, l))
}
fn limit_stale(l: &Limit, at: i64) -> bool {
    l.read_at.is_none_or(|t| at.saturating_sub(t as i64) > 600)
        || l.resets_at.is_some_and(|t| (t as i64) <= at)
}
fn limit_style(l: Option<&Limit>, at: i64) -> Style {
    let Some(l) = l else {
        return Style::default().fg(theme::DIM);
    };
    if limit_stale(l, at) {
        return Style::default().fg(theme::DIM);
    }
    if l.percent >= switch_at(l) {
        Style::default()
            .fg(theme::ERROR)
            .add_modifier(Modifier::BOLD)
    } else if l.percent >= 80.0 {
        Style::default().fg(theme::OWNER_BLOCKED)
    } else if l.percent >= 50.0 {
        Style::default().fg(theme::ATTN)
    } else {
        Style::default().fg(theme::OK)
    }
}
fn short_name(a: &Account) -> &str {
    a.alias
        .as_deref()
        .filter(|x| !x.is_empty())
        .unwrap_or_else(|| a.email.split('@').next().unwrap_or(&a.email))
}
fn fit(s: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > width {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push_str(&" ".repeat(width.saturating_sub(used)));
    out
}
// Keep styled indicators inside their fixed-width column, including wide glyphs.
fn fit_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    use unicode_width::UnicodeWidthChar;
    let mut remaining = width;
    let mut out = Vec::new();
    for span in spans {
        let mut text = String::new();
        for c in span.content.chars() {
            let cells = c.width().unwrap_or(0);
            if cells > remaining {
                break;
            }
            text.push(c);
            remaining -= cells;
        }
        out.push(Span::styled(text, span.style));
        if remaining == 0 {
            break;
        }
    }
    out.push(Span::raw(" ".repeat(remaining)));
    out
}
fn bar(l: Option<&Limit>, width: usize, at: i64) -> Vec<Span<'static>> {
    let Some(l) = l else {
        return vec![Span::styled(
            "─".repeat(width),
            Style::default().fg(theme::DIM),
        )];
    };
    let n = (l.percent.clamp(0.0, 100.0) * width as f64 / 100.0).round() as usize;
    vec![
        Span::styled("█".repeat(n.min(width)), limit_style(Some(l), at)),
        Span::styled(
            "░".repeat(width.saturating_sub(n)),
            Style::default().fg(theme::DIM),
        ),
    ]
}
fn fmt_utc(t: Option<f64>, format: &str) -> String {
    t.and_then(|v| chrono::DateTime::from_timestamp(v as i64, 0))
        .map(|d| d.format(format).to_string())
        .unwrap_or_else(|| "?".into())
}
fn reset_icons(a: &Account, detail: bool) -> Vec<Span<'static>> {
    let Some(resets) = &a.resets else {
        return vec![Span::styled("?", Style::default().fg(theme::DIM))];
    };
    let kinds: &[(&str, &str, &str)] = if a.engine == "claude" {
        &[
            ("claude_fable", "⚡", "Fable"),
            ("claude_full", "⟲", "Full"),
        ]
    } else {
        &[("codex", "⟲", "Codex")]
    };
    let mut spans = Vec::new();
    for (kind, icon, name) in kinds {
        let r = resets.iter().find(|r| r.kind == *kind);
        let count = r.map_or(0, |r| r.count);
        if count == 0 && !detail {
            continue;
        }
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        let unavailable = r.and_then(|r| r.usable_now) == Some(false);
        let color = if count == 0 || unavailable {
            theme::DIM
        } else if *kind == "claude_fable" {
            theme::REMOTE
        } else {
            theme::HEADER
        };
        let text = if detail {
            format!("{icon} {name} ×{count}")
        } else {
            format!("{icon}{count}{}", if unavailable { "!" } else { "" })
        };
        spans.push(Span::styled(text, Style::default().fg(color)));
        if detail {
            if let Some(r) = r {
                if count > 0 && a.engine == "claude" {
                    let status = match r.usable_now {
                        Some(true) => " (some usable)",
                        Some(false) => " (not usable now)",
                        None => " (usability unknown)",
                    };
                    spans.push(Span::styled(
                        status,
                        Style::default().fg(if unavailable {
                            theme::DIM
                        } else {
                            theme::MUTED
                        }),
                    ));
                }
                if r.expires_at.is_some() {
                    spans.push(Span::styled(
                        format!(" until {}", fmt_utc(r.expires_at, "%Y-%m-%d")),
                        Style::default().fg(theme::MUTED),
                    ));
                }
                if let Some(t) = r.as_of {
                    spans.push(Span::styled(
                        format!(" (as of {})", fmt_utc(Some(t), "%m-%d %H:%MZ")),
                        Style::default().fg(theme::DIM),
                    ));
                }
            }
        }
    }
    spans
}
fn sorted_accounts(d: &Snapshot) -> Vec<&Account> {
    let mut v: Vec<_> = d.accounts.iter().collect();
    v.sort_by(|a, b| {
        a.engine
            .cmp(&b.engine)
            .then_with(|| a.active_on.is_empty().cmp(&b.active_on.is_empty()))
            .then_with(|| {
                let remaining = |a: &Account| {
                    weekly_limit(a)
                        .map(|l| switch_at(l) - l.percent)
                        .unwrap_or(f64::NEG_INFINITY)
                };
                remaining(b).total_cmp(&remaining(a))
            })
            .then_with(|| a.email.cmp(&b.email))
    });
    v
}
fn overview_lines(d: &Snapshot, at: i64, width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for o in d.outlook.as_deref().unwrap_or(&[]) {
        let style = match o.dry_at {
            Some(t) if t as i64 <= at + 86400 => Style::default()
                .fg(theme::ERROR)
                .add_modifier(Modifier::BOLD),
            Some(t) if t as i64 <= at + 3 * 86400 => Style::default().fg(theme::ATTN),
            _ => Style::default().fg(theme::OK),
        };
        let hosts: std::collections::BTreeSet<_> = d
            .accounts
            .iter()
            .filter(|a| a.engine == o.engine)
            .flat_map(|a| a.active_on.iter())
            .collect();
        let host_count = if hosts.is_empty() {
            String::new()
        } else {
            format!(
                " on {} host{}",
                hosts.len(),
                if hosts.len() == 1 { "" } else { "s" }
            )
        };
        let dry = o
            .dry_at
            .map(|t| {
                format!(
                    "dry {} (in {})",
                    fmt_utc(Some(t), "%a %H:%MZ"),
                    countdown(Some(t))
                )
            })
            .unwrap_or_else(|| "no exhaustion in 7d".into());
        let summary = if width < 100 {
            format!(
                "{:.2} weeks · burn {:.3}/h{} · {}",
                o.room_weeks, o.burn_per_hour, host_count, dry
            )
        } else {
            format!(
                "{:.2} account-weeks left · burn {:.3}/h{} · {} · {}/{} with room",
                o.room_weeks, o.burn_per_hour, host_count, dry, o.accounts_with_room, o.accounts
            )
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<7}", o.engine.to_uppercase()),
                Style::default()
                    .fg(theme::HEADER)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(summary, style),
        ]));
        let refs = o
            .refills
            .iter()
            .map(|r| {
                format!(
                    "{} {} +{:.0}%",
                    fmt_utc(Some(r.at), "%a %H:%MZ"),
                    r.email.split('@').next().unwrap_or(&r.email),
                    r.frees * 100.0
                )
            })
            .collect::<Vec<_>>();
        lines.push(Line::styled(
            if refs.is_empty() {
                "        refills: none in 7d".into()
            } else {
                format!("        refills: {}", refs.join(" · "))
            },
            Style::default().fg(theme::MUTED),
        ));
    }
    lines
}
fn account_list_lines(
    d: &Snapshot,
    u: &Usage,
    width: usize,
    height: usize,
    at: i64,
) -> Vec<Line<'static>> {
    let accounts = sorted_accounts(d);
    let selected = u.selected.min(accounts.len().saturating_sub(1));
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut focus_row = 0;
    let mut previous = "";
    let name_width = if width >= 65 { 13 } else { 9 }.min(width.saturating_sub(2));
    let prefix_width = 3 + name_width;
    // Percent, reset countdown, 5h indicator, reset credits, and status markers.
    let bar_width = width
        .saturating_sub(prefix_width + 5 + 8 + 7 + 9 + 3)
        .clamp(1, 70);
    for (i, a) in accounts.iter().enumerate() {
        if a.engine != previous {
            previous = &a.engine;
            lines.push(Line::styled(
                format!("{} · weekly used", a.engine.to_uppercase()),
                Style::default()
                    .fg(theme::HEADER)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        if i == selected {
            focus_row = lines.len();
        }
        let w = weekly_limit(a);
        let pool_suffix = a
            .pool_hosts
            .as_ref()
            .filter(|hosts| a.engine == "codex" && hosts.len() > 1)
            .map(|hosts| format!(" ×{}", hosts.len()))
            .unwrap_or_default();
        use unicode_width::UnicodeWidthStr;
        let short = fit(
            short_name(a),
            name_width.saturating_sub(pool_suffix.width()),
        );
        let name = fit(&format!("{}{pool_suffix}", short.trim_end()), name_width);
        let mut suffix = fit_spans(
            vec![Span::styled(
                w.map(|l| format!(" {:>3.0}%", l.percent))
                    .unwrap_or_else(|| "   ?%".into()),
                limit_style(w, at),
            )],
            5,
        );
        suffix.extend(fit_spans(
            w.filter(|l| l.resets_at.is_some())
                .map(|l| {
                    vec![Span::styled(
                        format!(" r:{}", countdown(l.resets_at)),
                        Style::default().fg(theme::MUTED),
                    )]
                })
                .unwrap_or_default(),
            8,
        ));
        suffix.extend(fit_spans(
            five_limit(a)
                .filter(|l| !limit_stale(l, at))
                .map(|l| {
                    vec![Span::styled(
                        format!("{}5h{:.0}%", spark(&[l.percent]), l.percent),
                        limit_style(Some(l), at),
                    )]
                })
                .unwrap_or_default(),
            7,
        ));
        let mut icons = vec![Span::raw(" ")];
        icons.extend(reset_icons(a, false));
        suffix.extend(fit_spans(icons, 9));
        suffix.push(Span::styled(
            if a.active_on.is_empty() { " " } else { "●" },
            Style::default().fg(theme::HEADER),
        ));
        suffix.push(Span::styled(
            if room(a) == Some(false) { "✕" } else { " " },
            Style::default().fg(theme::ERROR),
        ));
        suffix.push(Span::styled(
            if w.is_none_or(|l| limit_stale(l, at)) {
                "◌"
            } else {
                " "
            },
            Style::default().fg(theme::DIM),
        ));
        let mut spans = vec![
            Span::styled(
                if i == selected { "▸ " } else { "  " },
                Style::default().fg(theme::HEADER),
            ),
            Span::styled(
                name,
                if i == selected {
                    Style::default()
                        .fg(theme::TEXT)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme::TEXT)
                },
            ),
            Span::raw(" "),
        ];
        spans.extend(bar(w, bar_width, at));
        spans.extend(suffix);
        lines.push(Line::from(spans));
        if u.expanded {
            let f = five_limit(a);
            let mut extra = vec![Span::styled(
                fit("    5h", prefix_width),
                Style::default().fg(theme::MUTED),
            )];
            extra.extend(bar(f, bar_width, at));
            extra.push(Span::styled(
                f.map(|l| format!(" {:.0}% used", l.percent))
                    .unwrap_or_else(|| " unknown".into()),
                limit_style(f, at),
            ));
            lines.push(Line::from(extra));
        }
    }
    if accounts.is_empty() {
        lines.push(Line::styled(
            "No accounts yet",
            Style::default().fg(theme::DIM),
        ));
    }
    let start = focus_row
        .saturating_add(1)
        .saturating_sub(height)
        .min(lines.len());
    lines.into_iter().skip(start).take(height).collect()
}
fn limit_detail_line(prefix: &str, l: Option<&Limit>, width: usize, at: i64) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!("{} ", fit(prefix, 12)),
        Style::default().fg(theme::MUTED),
    )];
    spans.extend(bar(l, width.saturating_sub(29).clamp(1, 48), at));
    spans.push(Span::styled(
        l.map(|l| format!(" {:>3.0}% used", l.percent))
            .unwrap_or_else(|| " unknown".into()),
        limit_style(l, at),
    ));
    Line::from(spans)
}
fn chart_lines(d: &Snapshot, a: &Account, width: usize, at: i64) -> Vec<Line<'static>> {
    let week = weekly_limit(a);
    let five = five_limit(a);
    let chart_width = width.saturating_sub(6).clamp(1, 70);
    let start = d.generated_at as i64 - 7 * 86400;
    let end = d.generated_at as i64;
    let mut cells = vec![vec![(' ', Style::default().fg(theme::DIM)); chart_width]; 5];
    let threshold = week.map(switch_at).unwrap_or(98.0);
    let threshold_y = ((100.0 - threshold.clamp(0.0, 100.0)) / 25.0).round() as usize;
    for x in 0..chart_width {
        cells[threshold_y.min(4)][x] = ('─', Style::default().fg(theme::DIM));
    }
    for (l, mark, connector) in [(week, 'W', '━'), (five, '5', '·')] {
        if let Some(l) = l {
            let mut points: Vec<_> = d
                .limit_history
                .iter()
                .filter(|h| h.engine == a.engine && h.email == a.email && h.kind == l.kind)
                .flat_map(|h| h.points.iter().copied())
                .filter(|(t, _)| (*t as i64) >= start && (*t as i64) <= end)
                .collect();
            points.sort_by(|a, b| a.0.total_cmp(&b.0));
            let x_for = |t: f64| {
                (((t as i64 - start) as f64 / (7 * 86400) as f64)
                    * chart_width.saturating_sub(1) as f64) as usize
            };
            let mut place = |x: usize, percent: f64, glyph: char| {
                let y = ((100.0 - percent.clamp(0.0, 100.0)) / 25.0).round() as usize;
                let sample = Limit {
                    percent,
                    read_at: l.read_at,
                    resets_at: l.resets_at,
                    switch_at: l.switch_at,
                    ..Limit::default()
                };
                let cell = &mut cells[y.min(4)][x.min(chart_width - 1)];
                let other_series = match mark {
                    'W' => matches!(cell.0, '5' | '·' | '◆'),
                    _ => matches!(cell.0, 'W' | '━' | '◆'),
                };
                *cell = (
                    if other_series { '◆' } else { glyph },
                    limit_style(Some(&sample), at),
                );
            };
            for pair in points.windows(2) {
                let (x1, x2) = (x_for(pair[0].0), x_for(pair[1].0));
                if x2 > x1 {
                    for x in x1..=x2 {
                        let f = (x - x1) as f64 / (x2 - x1) as f64;
                        place(x, pair[0].1 + (pair[1].1 - pair[0].1) * f, connector);
                    }
                }
            }
            for &(t, p) in &points {
                place(x_for(t), p, mark);
            }
        }
    }
    let mut out = vec![Line::styled(
        format!(
            "7-day trend  W/━ week  5/· 5h  ◆ both  ─ switch {:.0}%",
            threshold
        ),
        Style::default().fg(theme::MUTED),
    )];
    for (y, row) in cells.into_iter().enumerate() {
        let mut spans = vec![Span::styled(
            format!("{:>3}% ", 100 - y * 25),
            Style::default().fg(theme::DIM),
        )];
        spans.extend(row.into_iter().map(|(c, s)| Span::styled(c.to_string(), s)));
        out.push(Line::from(spans));
    }
    out.push(Line::styled(
        "      7 days ago → now",
        Style::default().fg(theme::DIM),
    ));
    out
}
fn account_detail_lines(d: &Snapshot, a: &Account, width: usize, at: i64) -> Vec<Line<'static>> {
    let w = weekly_limit(a);
    let f = five_limit(a);
    let mut out = vec![Line::from(vec![
        Span::styled(
            a.email.clone(),
            Style::default()
                .fg(theme::TEXT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            match room(a) {
                Some(true) => "  ROOM",
                Some(false) => "  NO ROOM",
                None => "  room ?",
            },
            Style::default().fg(match room(a) {
                Some(true) => theme::OK,
                Some(false) => theme::ERROR,
                None => theme::DIM,
            }),
        ),
        Span::styled(
            if a.active_on.is_empty() {
                String::new()
            } else {
                format!("  ● {}", a.active_on.join(", "))
            },
            Style::default().fg(theme::HEADER),
        ),
    ])];
    out.push(limit_detail_line("Week", w, width, at));
    if let Some(l) = w {
        out.push(Line::styled(
            format!(
                "         resets {} (in {}) · switch at {:.0}%",
                fmt_utc(l.resets_at, "%a %H:%MZ"),
                countdown(l.resets_at),
                switch_at(l)
            ),
            Style::default().fg(theme::MUTED),
        ));
    }
    out.push(limit_detail_line("5h", f, width, at));
    if let Some(l) = f {
        out.push(Line::styled(
            format!(
                "         resets {} (in {})",
                fmt_utc(l.resets_at, "%a %H:%MZ"),
                countdown(l.resets_at)
            ),
            Style::default().fg(theme::MUTED),
        ));
    }
    for l in &a.limits {
        if w.is_some_and(|x| std::ptr::eq(x, l)) || f.is_some_and(|x| std::ptr::eq(x, l)) {
            continue;
        }
        out.push(limit_detail_line(&window_label(a, l), Some(l), width, at));
    }
    let mut resets = vec![Span::styled("Resets   ", Style::default().fg(theme::MUTED))];
    for span in reset_icons(a, true) {
        if span.content == "  " {
            out.push(Line::from(resets));
            resets = vec![Span::raw("         ")];
        } else {
            resets.push(span);
        }
    }
    out.push(Line::from(resets));
    if let Some(p) = &a.projection {
        let projected = p
            .at_switch
            .map(|t| format!("switch {} ⚠", fmt_utc(Some(t), "%a %H:%MZ")))
            .unwrap_or_else(|| "no switch before reset".into());
        out.push(Line::styled(
            format!(
                "Pace     {:.1}%/h · basis {:.1}h · {projected}",
                p.rate_per_hour, p.basis_hours
            ),
            if w.is_none_or(|l| limit_stale(l, at)) {
                Style::default().fg(theme::DIM)
            } else if p.at_switch.is_some() {
                Style::default().fg(theme::ATTN)
            } else {
                Style::default().fg(theme::TEXT)
            },
        ));
        out.push(Line::styled(
            format!(
                "         projected at reset: {:.0}% used",
                p.at_reset_percent
            ),
            Style::default().fg(theme::MUTED),
        ));
    } else {
        out.push(Line::styled(
            "Pace     unknown",
            Style::default().fg(theme::DIM),
        ));
    }
    out.push(Line::styled(
        format!(
            "Hosts    active: {} · reserved: {}{}",
            if a.active_on.is_empty() {
                "—".into()
            } else {
                a.active_on.join(", ")
            },
            a.reserved_for
                .as_ref()
                .map(|r| if r.is_empty() {
                    "none".into()
                } else {
                    r.join(", ")
                })
                .unwrap_or_else(|| "unknown".into()),
            a.reservation_note
                .as_ref()
                .map(|s| format!(" ({s})"))
                .unwrap_or_default()
        ),
        Style::default().fg(theme::MUTED),
    ));
    if a.engine == "codex" {
        out.push(Line::styled(
            format!(
                "Codex    status {} · plan {}",
                a.status.as_deref().unwrap_or("?"),
                a.plan_type.as_deref().unwrap_or("?")
            ),
            Style::default().fg(theme::MUTED),
        ));
        let pools = match a.pool_hosts.as_ref().filter(|hosts| !hosts.is_empty()) {
            Some(hosts) => hosts
                .iter()
                .map(|host| {
                    match a
                        .pool_status
                        .as_ref()
                        .and_then(|statuses| statuses.get(host))
                    {
                        Some(status) => format!("{host} ({status})"),
                        None => host.clone(),
                    }
                })
                .collect::<Vec<_>>()
                .join(", "),
            None => a.pool_host.as_deref().unwrap_or("?").to_string(),
        };
        out.push(Line::styled(
            format!("Pools: {pools}"),
            Style::default().fg(theme::MUTED),
        ));
    }
    out.extend(chart_lines(d, a, width, at));
    out.push(Line::styled(
        "Tokens by account",
        Style::default().fg(theme::HEADER),
    ));
    for window in ["24h", "7d", "30d", "all"] {
        let total = d.tokens.totals.iter().find(|t| {
            t.window == window
                && t.engine == a.engine
                && t.email.as_deref() == Some(&a.email)
                && t.host.is_none()
        });
        out.push(Line::styled(
            match total {
                Some(t) => format!(
                    "  {window:<4} {} in / {} out · {} cache · {} requests",
                    compact(t.input),
                    compact(t.output),
                    compact(t.cache_read.saturating_add(t.cache_write)),
                    compact(t.requests)
                ),
                None => format!("  {window:<4} —"),
            },
            Style::default().fg(theme::MUTED),
        ));
    }
    out
}
fn render_usage(frame: &mut Frame, area: Rect, u: &Usage, host: &str) {
    if !u.open {
        return;
    }
    frame.render_widget(Clear, area);
    if area.width == 0 || area.height == 0 {
        return;
    }
    let at = now();
    let accounts = u.data.as_ref().map(sorted_accounts).unwrap_or_default();
    let count = |engine: &str| {
        let list: Vec<_> = accounts.iter().filter(|a| a.engine == engine).collect();
        format!(
            "{} {}/{} with room",
            if engine == "claude" {
                "Claude"
            } else {
                "Codex"
            },
            list.iter().filter(|a| room(a) == Some(true)).count(),
            list.len()
        )
    };
    let age = u
        .data
        .as_ref()
        .map(|d| format!("updated {} ago", countdown_age(d.generated_at as i64)))
        .unwrap_or_else(|| "no data".into());
    let summary = Line::from(vec![
        Span::styled(
            " Usage ",
            Style::default()
                .fg(theme::HEADER)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("· {} · {}", count("claude"), count("codex"))),
    ]);
    let tail = if area.width >= 100 {
        format!("{age} · {host} · ? keys")
    } else {
        format!("{age} · ? keys")
    };
    let tail_width = (tail.chars().count() as u16).min(area.width);
    frame.render_widget(
        Paragraph::new(summary),
        Rect::new(area.x, area.y, area.width.saturating_sub(tail_width), 1),
    );
    frame.render_widget(
        Paragraph::new(tail)
            .style(Style::default().fg(theme::MUTED))
            .alignment(ratatui::layout::Alignment::Right),
        Rect::new(
            area.right().saturating_sub(tail_width),
            area.y,
            tail_width,
            1,
        ),
    );
    let mut top = area.y.saturating_add(1);
    let bottom = area.bottom().saturating_sub(1);
    if let Some(e) = &u.error {
        if top < bottom {
            frame.render_widget(
                Paragraph::new(format!("Error: {e}")).style(Style::default().fg(theme::ERROR)),
                Rect::new(area.x, top, area.width, 1),
            );
            top += 1;
        }
    }
    if u.help {
        let help = vec![
            "Usage · keys",
            "F5 / Esc / q   return",
            "Tab / 1 / 2   Accounts / Tokens",
            "j/k or arrows   select account / scroll Tokens",
            "g/G   first / last account",
            "x   expand/collapse 5h bars",
            "Enter / Backspace   open/close detail on narrow screens",
            "Page Up/Down   scroll detail or Tokens",
            "h/d   hourly / daily tokens",
            "e   cycle engine     g (Tokens)   group",
            "⚡ Fable reset · ⟲ full/Codex reset · ! not usable · ? unknown",
            "● active · ✕ no room · ◌ stale/unread",
            "Bars and percentages show used capacity.",
        ];
        if bottom > top {
            frame.render_widget(
                Paragraph::new(help.join("\n")),
                Rect::new(area.x, top, area.width, bottom - top),
            );
        }
    } else if let Some(d) = &u.data {
        if u.tab == 0 {
            let overview = overview_lines(d, at, area.width);
            if !overview.is_empty() && bottom > top {
                let h = (overview.len() as u16 + 2).min(bottom - top);
                let rect = Rect::new(area.x, top, area.width, h);
                let block = Block::default()
                    .borders(Borders::ALL)
                    .title(" Week outlook ")
                    .border_style(Style::default().fg(theme::MUTED));
                let inner = block.inner(rect);
                frame.render_widget(block, rect);
                frame.render_widget(Paragraph::new(overview), inner);
                top += h;
            }
            if bottom > top {
                let body = Rect::new(area.x, top, area.width, bottom - top);
                let wide = area.width >= 100;
                let show_detail = wide || u.detail_open;
                let list_width = if wide {
                    (body.width * 45 / 100).max(64).min(body.width)
                } else if show_detail {
                    0
                } else {
                    body.width
                };
                if list_width > 0 {
                    let list = Rect::new(body.x, body.y, list_width, body.height);
                    frame.render_widget(
                        Paragraph::new(account_list_lines(
                            d,
                            u,
                            list.width as usize,
                            list.height as usize,
                            at,
                        )),
                        list,
                    );
                }
                if show_detail {
                    if let Some(a) = accounts.get(u.selected.min(accounts.len().saturating_sub(1)))
                    {
                        let x = body.x + list_width;
                        let rect = Rect::new(
                            x,
                            body.y,
                            body.width.saturating_sub(list_width),
                            body.height,
                        );
                        let block = Block::default()
                            .borders(Borders::LEFT)
                            .title(" Account details ")
                            .border_style(Style::default().fg(theme::DIM));
                        let inner = block.inner(rect);
                        frame.render_widget(block, rect);
                        frame.render_widget(
                            Paragraph::new(account_detail_lines(d, a, inner.width as usize, at))
                                .wrap(ratatui::widgets::Wrap { trim: false })
                                .scroll((u.scroll, 0)),
                            inner,
                        );
                    }
                }
            }
        } else if bottom > top {
            let mut lines = vec!["Tokens".to_owned()];
            token_lines(d, u, &mut lines);
            frame.render_widget(
                Paragraph::new(lines.join("\n"))
                    .wrap(ratatui::widgets::Wrap { trim: false })
                    .scroll((u.scroll, 0)),
                Rect::new(area.x, top, area.width, bottom - top),
            );
        }
    } else if bottom > top {
        frame.render_widget(
            Paragraph::new(if u.error.is_some() {
                "No snapshot"
            } else {
                "Loading usage…"
            }),
            Rect::new(area.x, top, area.width, bottom - top),
        );
    }
    if area.height > 1 {
        let footer = if u.help {
            "? close help · F5/Esc/q return"
        } else if u.tab == 1 {
            "1 Accounts · h/d bucket · e engine · g group · j/k scroll · F5 return"
        } else if area.width < 100 {
            "1 Accounts · 2 Tokens · j/k select · x 5h · Enter details · ? keys · F5 return"
        } else {
            "1 Accounts · 2 Tokens · j/k select · g/G ends · x 5h · ⚡ Fable ⟲ Full/Codex · ? keys"
        };
        frame.render_widget(
            Paragraph::new(footer).style(Style::default().fg(theme::DIM)),
            Rect::new(area.x, bottom, area.width, 1),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    use ratatui::{backend::TestBackend, style::Color, Terminal};
    fn key(c: KeyCode) -> CrosstermEvent {
        CrosstermEvent::Key(KeyEvent::new(c, KeyModifiers::NONE))
    }
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
    fn fixture() -> Snapshot {
        let mut d: Snapshot =
            serde_json::from_str(include_str!("../../tests/fixtures/usage-snapshot.json")).unwrap();
        // Preserve the fixture's time intervals, while keeping its readings live.
        let shift = now() as f64 - d.generated_at;
        d.generated_at += shift;
        for a in &mut d.accounts {
            for l in &mut a.limits {
                if let Some(x) = &mut l.read_at {
                    *x += shift;
                }
                if let Some(x) = &mut l.resets_at {
                    *x += shift;
                }
            }
            if let Some(rs) = &mut a.resets {
                for r in rs {
                    if let Some(x) = &mut r.expires_at {
                        *x += shift;
                    }
                    if let Some(x) = &mut r.as_of {
                        *x += shift;
                    }
                }
            }
            if let Some(p) = &mut a.projection {
                if let Some(x) = &mut p.at_switch {
                    *x += shift;
                }
            }
        }
        for h in &mut d.limit_history {
            for p in &mut h.points {
                p.0 += shift;
            }
        }
        if let Some(os) = &mut d.outlook {
            for o in os {
                if let Some(t) = &mut o.dry_at {
                    *t += shift;
                }
                for r in &mut o.refills {
                    r.at += shift;
                }
            }
        }
        d
    }
    fn screen(a: &mut App, w: u16, h: u16) -> String {
        use unicode_width::UnicodeWidthStr;
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| a.draw(f)).unwrap();
        let b = t.backend().buffer();
        (0..h)
            .map(|y| {
                let mut line = String::new();
                let mut x = 0;
                while x < w {
                    let symbol = b[(x, y)].symbol();
                    line.push_str(symbol);
                    x += symbol.width().max(1) as u16;
                }
                line.trim_end().to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    #[test]
    fn fixture_renders_full_screen_and_details_at_all_widths() {
        let mut a = app();
        a.usage.data = Some(fixture());
        a.handle_event(&key(KeyCode::F(5)));
        for (w, h) in [(200, 50), (120, 40), (80, 24)] {
            let text = screen(&mut a, w, h);
            assert!(text.starts_with(" Usage"), "{text}");
            for item in [
                "Week outlook",
                "CLAUDE",
                "CODEX",
                "⚡1",
                "⟲1",
                "weekly used",
            ] {
                assert!(text.contains(item), "{item}:\n{text}");
            }
            if w >= 100 {
                for item in [
                    "Account details",
                    "a@example.com",
                    "Pace",
                    "7-day trend",
                    "98%",
                    "Tokens by account",
                ] {
                    assert!(text.contains(item), "{item}:\n{text}");
                }
            } else {
                assert!(!text.contains("Account details"));
            }
        }
        a.handle_event(&key(KeyCode::Enter));
        let detail = screen(&mut a, 80, 24);
        assert!(detail.contains("Account details") && detail.contains("a@example.com"));
        a.handle_event(&key(KeyCode::F(5)));
        assert!(!a.usage.open);
    }
    #[test]
    fn older_snapshot_shows_unknown_and_never_guesses_codex_windows() {
        let mut v: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/usage-snapshot.json")).unwrap();
        v.as_object_mut().unwrap().remove("outlook");
        for a in v["accounts"].as_array_mut().unwrap() {
            let a = a.as_object_mut().unwrap();
            a.remove("resets");
            a.remove("projection");
            for l in a["limits"].as_array_mut().unwrap() {
                l.as_object_mut().unwrap().remove("window_minutes");
                l.as_object_mut().unwrap().remove("switch_at");
            }
        }
        let d: Snapshot = serde_json::from_value(v).unwrap();
        let codex = d.accounts.iter().find(|a| a.engine == "codex").unwrap();
        assert!(weekly_limit(codex).is_none());
        assert!(five_limit(codex).is_none());
        assert_eq!(window_label(codex, &codex.limits[0]), "Window ?");
        assert_eq!(reset_icons(codex, false)[0].content, "?");
        let mut a = app();
        a.usage.open = true;
        a.usage.data = Some(d);
        let text = screen(&mut a, 120, 40);
        assert!(!text.contains("Week outlook"));
        assert!(text.contains("?"));
    }
    #[test]
    fn full_screen_takes_priority_and_returns_to_each_view() {
        let mut a = app();
        a.usage.data = Some(fixture());
        for view in 0..4 {
            a.orgchart.visible = view == 0;
            a.board.visible = view == 1;
            a.messages.visible = view == 2;
            a.roster.visible = view == 3;
            assert!(a.handle_event(&key(KeyCode::F(5))));
            assert!(screen(&mut a, 80, 24).starts_with(" Usage"));
            a.handle_event(&key(KeyCode::Char('q')));
            assert!(!a.usage.open);
            assert_eq!(
                [
                    a.orgchart.visible,
                    a.board.visible,
                    a.messages.visible,
                    a.roster.visible
                ][view],
                true
            );
        }
    }
    #[test]
    fn coloured_bars_and_reset_icons_reach_the_test_backend() {
        let mut a = app();
        let mut d = fixture();
        d.accounts[0].resets.as_mut().unwrap()[0].usable_now = Some(true);
        let l = d.accounts[0]
            .limits
            .iter_mut()
            .find(|l| l.kind == "weekly_all")
            .unwrap();
        l.percent = 80.0;
        a.usage.data = Some(d);
        a.usage.open = true;
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| a.draw(f)).unwrap();
        let cells = &terminal.backend().buffer().content;
        assert!(cells
            .iter()
            .any(|c| c.symbol() == "█" && c.fg == theme::OWNER_BLOCKED));
        assert!(cells
            .iter()
            .any(|c| c.symbol() == "⚡" && c.fg == theme::REMOTE));
        assert!(cells
            .iter()
            .any(|c| c.symbol() == "⟲" && c.fg == theme::HEADER));
    }
    #[test]
    fn unavailable_reset_credit_is_marked_and_dimmed() {
        let d = fixture();
        let a = &d.accounts[0];
        let icons = reset_icons(a, false);
        assert_eq!(icons[0].content, "⚡1!");
        assert_eq!(icons[0].style.fg, Some(theme::DIM));
        let details = reset_icons(a, true);
        assert!(details.iter().any(|s| s.content.contains("not usable now")));
        assert!(details.iter().any(|s| s.content.contains("some usable")));
    }
    #[test]
    fn outlook_colours_follow_dry_time() {
        let mut d = fixture();
        let t = now();
        let o = &mut d.outlook.as_mut().unwrap()[0];
        o.dry_at = Some((t + 3600) as f64);
        assert_eq!(
            overview_lines(&d, t, 120)[0].spans[1].style.fg,
            Some(theme::ERROR)
        );
        d.outlook.as_mut().unwrap()[0].dry_at = Some((t + 2 * 86400) as f64);
        assert_eq!(
            overview_lines(&d, t, 120)[0].spans[1].style.fg,
            Some(theme::ATTN)
        );
        d.outlook.as_mut().unwrap()[0].dry_at = None;
        assert_eq!(
            overview_lines(&d, t, 120)[0].spans[1].style.fg,
            Some(theme::OK)
        );
    }
    #[test]
    fn level_colours_obey_thresholds_and_staleness() {
        let t = now();
        let mut l = Limit {
            read_at: Some(t as f64),
            resets_at: Some((t + 3600) as f64),
            switch_at: Some(94.0),
            ..Limit::default()
        };
        for (p, c) in [
            (0.0, theme::OK),
            (49.9, theme::OK),
            (50.0, theme::ATTN),
            (79.9, theme::ATTN),
            (80.0, theme::OWNER_BLOCKED),
            (94.0, theme::ERROR),
        ] {
            l.percent = p;
            assert_eq!(limit_style(Some(&l), t).fg, Some(c));
        }
        assert!(limit_style(Some(&l), t)
            .add_modifier
            .contains(Modifier::BOLD));
        l.read_at = Some((t - 601) as f64);
        assert_eq!(limit_style(Some(&l), t).fg, Some(theme::DIM));
        assert_eq!(limit_style(None, t).fg, Some(Color::DarkGray));
        l.read_at = Some(t as f64);
        l.percent = 96.0;
        l.switch_at = Some(100.0);
        assert_eq!(limit_style(Some(&l), t).fg, Some(theme::OWNER_BLOCKED));
    }
    #[test]
    fn navigation_expansion_credits_and_tokens() {
        let mut a = app();
        a.usage.data = Some(fixture());
        a.handle_event(&key(KeyCode::F(5)));
        let initial = screen(&mut a, 160, 40);
        assert!(initial.contains("⚡1") && initial.contains("⟲1") && initial.contains("⟲2"));
        a.handle_event(&key(KeyCode::Char('j')));
        assert_eq!(a.usage.selected, 1);
        assert!(screen(&mut a, 160, 40).contains("c@example.com"));
        a.handle_event(&key(KeyCode::Char('G')));
        assert_eq!(a.usage.selected, 2);
        let codex = screen(&mut a, 160, 40);
        assert!(codex.contains("b@example.com") && codex.contains("Codex ×2"));
        assert!(codex.contains("1.2M in") && codex.contains("4 requests"));
        a.handle_event(&key(KeyCode::Char('g')));
        assert_eq!(a.usage.selected, 0);
        a.handle_event(&key(KeyCode::Char('x')));
        assert!(a.usage.expanded);
        assert!(screen(&mut a, 160, 40).contains("    5h"));
        a.handle_event(&key(KeyCode::Tab));
        assert_eq!(a.usage.tab, 1);
        a.handle_event(&key(KeyCode::Char('d')));
        assert_eq!(a.usage.granularity, "day");
        a.handle_event(&key(KeyCode::Char('e')));
        a.handle_event(&key(KeyCode::Char('g')));
        assert_eq!(a.usage.engine, 1);
        assert!(a.usage.by_host);
        a.handle_event(&key(KeyCode::Char('1')));
        assert_eq!(a.usage.tab, 0);
        a.handle_event(&key(KeyCode::Esc));
        assert!(!a.usage.open);
    }
    #[test]
    fn refresh_keeps_focused_account_when_sort_order_changes() {
        let mut a = app();
        let original = fixture();
        a.usage.data = Some(original);
        a.usage.open = true;
        a.usage.selected = 1;
        let mut next = fixture();
        next.accounts
            .iter_mut()
            .find(|a| a.email == "a@example.com")
            .unwrap()
            .active_on
            .clear();
        next.accounts
            .iter_mut()
            .find(|a| a.email == "c@example.com")
            .unwrap()
            .active_on
            .push("host".into());
        let (tx, rx) = mpsc::channel();
        a.usage.pending = Some(rx);
        a.usage.last_poll = Some(Instant::now());
        tx.send(("hour", Ok(next))).unwrap();
        a.usage_tick();
        let accounts = sorted_accounts(a.usage.data.as_ref().unwrap());
        assert_eq!(accounts[a.usage.selected].email, "c@example.com");
        assert_eq!(a.usage.selected, 0);
    }
    #[test]
    fn weekly_list_bar_remains_wide_at_120_columns() {
        let d = fixture();
        let lines = account_list_lines(&d, &Usage::default(), 64, 30, now());
        let row = &lines[1];
        let width: usize = row
            .spans
            .iter()
            .filter(|s| s.content.chars().all(|c| c == '█' || c == '░'))
            .map(|s| s.content.chars().count())
            .sum();
        assert!(width >= 20, "bar was {width} cells");
    }
    #[test]
    fn account_bars_align_in_test_backend() {
        let d = fixture();
        for width in [64, 78, 100] {
            let mut u = Usage::default();
            u.expanded = true;
            let lines = account_list_lines(&d, &u, width, 30, now());
            let mut terminal = Terminal::new(TestBackend::new(width as u16, 30)).unwrap();
            terminal
                .draw(|f| f.render_widget(Paragraph::new(lines), f.area()))
                .unwrap();
            let b = terminal.backend().buffer();
            let mut bars = Vec::new();
            for y in 0..30 {
                let is_bar = |x| matches!(b[(x, y)].symbol(), "█" | "░" | "─");
                if let Some(start) = (0..width as u16).find(|&x| is_bar(x)) {
                    let len = (start..width as u16).take_while(|&x| is_bar(x)).count();
                    bars.push((start, len));
                }
            }
            assert_eq!(bars.len(), d.accounts.len() * 2, "{bars:?}");
            assert!(bars.iter().all(|bar| *bar == bars[0]), "{bars:?}");
            assert!(bars[0].1 >= 20, "{bars:?}");

            let account = d
                .accounts
                .iter()
                .find(|a| a.email == "a@example.com")
                .unwrap();
            let lines = account_detail_lines(&d, account, width, now());
            let mut terminal = Terminal::new(TestBackend::new(width as u16, 30)).unwrap();
            terminal
                .draw(|f| f.render_widget(Paragraph::new(lines), f.area()))
                .unwrap();
            let b = terminal.backend().buffer();
            let mut bars = Vec::new();
            for y in 0..30 {
                let label: String = (0..12).map(|x| b[(x, y)].symbol()).collect();
                if !["Week", "5h", "fable week"].contains(&label.trim()) {
                    continue;
                }
                let is_bar = |x| matches!(b[(x, y)].symbol(), "█" | "░" | "─");
                if let Some(start) = (0..width as u16).find(|&x| is_bar(x)) {
                    let len = (start..width as u16).take_while(|&x| is_bar(x)).count();
                    bars.push((start, len));
                }
            }
            assert_eq!(bars.len(), 3, "{bars:?}");
            assert!(bars.iter().all(|bar| *bar == bars[0]), "{bars:?}");
        }
    }
    #[test]
    fn codex_multiple_pools_and_legacy_fallback_render() {
        let mut json: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/usage-snapshot.json")).unwrap();
        let account = json["accounts"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|a| a["engine"] == "codex")
            .unwrap();
        account["pool_hosts"] = serde_json::json!(["cm-manager", "cm-sessions"]);
        account["pool_status"] =
            serde_json::json!({"cm-manager": "active", "cm-sessions": "paused"});
        let d: Snapshot = serde_json::from_value(json).unwrap();
        let mut app = app();
        app.usage.data = Some(d);
        app.usage.open = true;
        app.usage.selected = 2;
        let text = screen(&mut app, 160, 40);
        assert!(text.contains("802 ×2"), "{text}");
        assert!(
            text.contains("Pools: cm-manager (active), cm-sessions (paused)"),
            "{text}"
        );
        app.usage.data = Some(fixture());
        let legacy = screen(&mut app, 160, 40);
        assert!(legacy.contains("Pools: cm-sessions"), "{legacy}");
        assert!(!legacy.contains("802 ×2"), "{legacy}");
    }
    #[test]
    fn codex_primary_can_be_weekly_and_zero_credits_are_distinct_from_unknown() {
        let mut d = fixture();
        let a = d.accounts.iter_mut().find(|a| a.engine == "codex").unwrap();
        a.limits[0].window_minutes = Some(10080);
        a.limits[1].window_minutes = Some(300);
        a.projection = None;
        assert_eq!(weekly_limit(a).unwrap().kind, "primary");
        assert_eq!(window_label(a, weekly_limit(a).unwrap()), "Week");
        assert_eq!(window_label(a, five_limit(a).unwrap()), "5h");
        a.resets = Some(vec![]);
        assert!(reset_icons(a, false).is_empty());
        assert!(reset_icons(a, true).iter().any(|s| s.content.contains('0')));
        a.resets = None;
        assert_eq!(reset_icons(a, true)[0].content, "?");
    }
    #[test]
    fn empty_error_and_tiny_screen_are_safe() {
        let mut a = app();
        a.usage.open = true;
        a.usage.data = Some(Snapshot {
            schema: 1,
            ..Snapshot::default()
        });
        a.usage.error = Some("not_hub".into());
        let s = screen(&mut a, 80, 24);
        assert!(s.contains("not_hub") && s.contains("No accounts yet"));
        let _ = screen(&mut a, 3, 2);
    }
    #[test]
    fn token_window_and_seven_day_chart() {
        let epoch = 1_795_400_123;
        assert_eq!(
            since_for_granularity(epoch, "hour"),
            (epoch - 7 * 86400) / 3600 * 3600
        );
        assert_eq!(
            since_for_granularity(epoch, "day"),
            (epoch - 30 * 86400) / 3600 * 3600
        );
        let mut d = fixture();
        let a = &d.accounts[0];
        let kind = weekly_limit(a).unwrap().kind.clone();
        d.limit_history = vec![History {
            engine: a.engine.clone(),
            email: a.email.clone(),
            kind,
            points: vec![
                (d.generated_at - 8.0 * 86400.0, 20.0),
                (d.generated_at - 2.0 * 86400.0, 70.0),
                (d.generated_at - 86400.0, 80.0),
            ],
        }];
        d.limit_history.push(History {
            engine: "claude".into(),
            email: "a@example.com".into(),
            kind: "five_hour".into(),
            points: vec![(d.generated_at - 3600.0, 25.0)],
        });
        let lines = chart_lines(&d, &d.accounts[0], 60, now());
        assert!(!lines[4].spans.iter().skip(1).any(|s| s.content == "W"));
        assert!(lines[2].spans.iter().skip(1).any(|s| s.content == "W"));
        assert!(lines[4].spans.iter().skip(1).any(|s| s.content == "5"));
        assert!(lines
            .iter()
            .any(|line| line.spans.iter().any(|s| s.content == "━")));
    }
    #[test]
    fn fixture_accounts_text_renders() {
        let mut a = app();
        a.usage.open = true;
        a.usage.data = Some(fixture());
        println!(
            "--- ACCOUNTS 80x24 ---\n{}\n--- END 80 ---",
            screen(&mut a, 80, 24)
        );
        println!(
            "--- ACCOUNTS 160x40 ---\n{}\n--- END 160 ---",
            screen(&mut a, 160, 40)
        );
    }
}
