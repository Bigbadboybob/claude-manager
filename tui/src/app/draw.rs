//! Rendering: the frame draw entrypoint, sidebar/status-bar/terminal/continuous-column painters, and every dialog overlay.

use super::*;

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

const SPINNER_INTERVAL_MS: u128 = 80;

/// Frame duration of the `notify_user` attention animation — the "rainbow
/// heartbeat". A session with a pending alert advances one frame every
/// `ALERT_FRAME_MS`, pulsing its glyph small→large while the color cycles
/// through the rainbow so it pops against the steady green-spinner / white-dot
/// indicators around it.
const ALERT_FRAME_MS: u128 = 120;

/// Glyph pulse — the bead grows then shrinks. 6 frames. The color it cycles
/// through per frame is `theme::ALERT_RAINBOW`.
const ALERT_PULSE: &[&str] = &["\u{00b7}", "\u{2022}", "\u{25cf}", "\u{25c9}", "\u{25cf}", "\u{2022}"];

/// Value spans for a color-picker field in the A-e settings dialogs: a
/// "none" slot plus one swatch per `theme::USER_COLORS` entry, the current
/// selection bracketed. Callers style their own label; a ←/→ hint is
/// appended while the field is focused.
fn color_picker_spans(current: Option<&str>, focused: bool) -> Vec<Span<'static>> {
    let dim = Style::default().fg(theme::DIM);
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(
        if current.is_none() { "[none]" } else { " none " },
        if current.is_none() {
            Style::default().fg(theme::TEXT)
        } else {
            dim
        },
    ));
    for (name, color) in theme::USER_COLORS {
        let marker = if current == Some(*name) {
            "[\u{25a0}]"
        } else {
            " \u{25a0} "
        };
        spans.push(Span::styled(marker, Style::default().fg(*color)));
    }
    if focused {
        spans.push(Span::styled("  \u{2190}/\u{2192}", dim));
    }
    spans
}

fn aggregate_indicator(
    run: &WorkflowRun,
    ws: &Workspace,
    spinner: &'static str,
) -> (&'static str, Style) {
    match run.status {
        workflow::RunStatus::Done => ("\u{2713}", Style::default().fg(theme::OK)),
        workflow::RunStatus::Paused => ("\u{25cf}", Style::default().fg(theme::ATTN)),
        _ => {
            // Match the per-session indicator logic: active iff any participant
            // session tagged with this run_id is Running and not exited.
            let any_running = ws.sessions.iter().any(|ts| {
                ts.workflow_run_id.as_ref() == Some(&run.run_id)
                    && ts.status == SessionStatus::Running
                    && !ts.session.exited
            });
            if any_running {
                (spinner, Style::default().fg(theme::OK))
            } else {
                ("\u{25cf}", Style::default().fg(theme::TEXT))
            }
        }
    }
}

/// Per-draw-section timing logger. Mirrors `main::log_slow_phase`
/// but with a tighter 50ms threshold so sub-components of a slow
/// `draw` phase get attributed individually (the main-loop
/// `phase=draw` log only fires on the aggregate, which can hide
/// a single hot widget). Same log file + format prefix so a
/// reader can tail one file for both granularities.
fn log_draw_section(section: &str, elapsed: std::time::Duration) {
    const THRESHOLD: std::time::Duration = std::time::Duration::from_millis(50);
    if elapsed < THRESHOLD {
        return;
    }
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let dir = std::path::PathBuf::from(home).join(".cm");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("slow-ticks.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(
            f,
            "{} phase=draw:{} elapsed_ms={}",
            now,
            section,
            elapsed.as_millis(),
        );
    }
}

impl App {
    fn spinner_frame(&self) -> &'static str {
        let elapsed = self.start_time.elapsed().as_millis();
        let idx = (elapsed / SPINNER_INTERVAL_MS) as usize % SPINNER_FRAMES.len();
        SPINNER_FRAMES[idx]
    }

    /// Current animation frame of the alert heartbeat — a monotonic counter
    /// off the same clock the spinner uses. Drives both the glyph/color pick
    /// and the redraw trigger in `tick_alerts`.
    pub(super) fn alert_frame(&self) -> u64 {
        (self.start_time.elapsed().as_millis() / ALERT_FRAME_MS) as u64
    }

    /// Glyph + style for an alerting session's indicator at the current frame:
    /// a bold bead that pulses small→large while its color cycles through the
    /// rainbow. Fully replaces the normal status indicator while the alert is
    /// pending — the animation *is* the signal.
    fn alert_indicator(&self) -> (&'static str, Style) {
        let f = self.alert_frame() as usize;
        let glyph = ALERT_PULSE[f % ALERT_PULSE.len()];
        let color = theme::ALERT_RAINBOW[f % theme::ALERT_RAINBOW.len()];
        (glyph, Style::default().fg(color).add_modifier(Modifier::BOLD))
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();

        // Phase 6: bottom layout — content / [activity strip] / status bar.
        // Activity strip renders only when toggled on (Alt-,) and we have
        // entries to show; fixed at 5 lines so it doesn't dominate the
        // screen but shows enough recent context to be useful.
        let activity_height: u16 = if self.activity_visible
            && !self.activity_log.is_empty()
            && area.height >= 8
        {
            5
        } else {
            0
        };
        let rows = if activity_height > 0 {
            Layout::vertical([
                Constraint::Min(1),
                Constraint::Length(activity_height),
                Constraint::Length(1),
            ])
            .split(area)
        } else {
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area)
        };

        let content_area = rows[0];
        let bar_area = if activity_height > 0 { rows[2] } else { rows[1] };
        let activity_area = if activity_height > 0 { Some(rows[1]) } else { None };

        // Wipe the content area first so stale cells from a previous frame's
        // wider/taller widgets don't bleed through when a new panel renders
        // less content. Ratatui only diffs touched cells; without this, the
        // user sees artifacts in the gaps after switching views/panels (the
        // status bar is fully painted by draw_status_bar so it doesn't need
        // clearing here).
        // Clear-on-transition: only wipe the content area when a
        // layout-changing event happened (view switch, input
        // dialog open/close). Steady-state draws skip Clear and
        // let ratatui's incremental diff handle redraw — which
        // was the dominant cost pre-fix (Clear forced every cell
        // changed → ratatui flushed the entire screen as ANSI
        // escapes on every frame, ~200-400ms per draw).
        let cur_input_disc = std::mem::discriminant(&self.input_mode);
        let layout_changed = self.last_drawn_view_mode.as_ref() != Some(&self.view_mode)
            || self.last_drawn_input_disc != Some(cur_input_disc);
        if layout_changed {
            let t = std::time::Instant::now();
            frame.render_widget(Clear, content_area);
            log_draw_section("clear", t.elapsed());
        }
        self.last_drawn_view_mode = Some(self.view_mode.clone());
        self.last_drawn_input_disc = Some(cur_input_disc);

        match self.view_mode {
            ViewMode::Sessions => {
                // Continuous panel: when `A-c` is on, split a third 36-col pane
                // off the right for the dedicated continuous column
                // (terminal | main | continuous). Off = terminal | main.
                let cols = if self.continuous_column_on {
                    Layout::horizontal([
                        Constraint::Min(40),
                        Constraint::Length(SIDEBAR_WIDTH),
                        Constraint::Length(SIDEBAR_WIDTH),
                    ])
                    .split(content_area)
                } else {
                    Layout::horizontal([
                        Constraint::Min(40),
                        Constraint::Length(SIDEBAR_WIDTH),
                    ])
                    .split(content_area)
                };

                let t = std::time::Instant::now();
                self.draw_terminal(frame, cols[0]);
                log_draw_section("sessions:terminal", t.elapsed());

                let t = std::time::Instant::now();
                self.draw_session_list(frame, cols[1]);
                log_draw_section("sessions:list", t.elapsed());

                if self.continuous_column_on {
                    let t = std::time::Instant::now();
                    self.draw_continuous_column(frame, cols[2]);
                    log_draw_section("sessions:continuous", t.elapsed());
                }
            }
            ViewMode::Planning => {
                let t = std::time::Instant::now();
                self.planning.draw(frame, content_area);
                log_draw_section("planning", t.elapsed());
            }
        }

        if let Some(act_area) = activity_area {
            let t = std::time::Instant::now();
            self.draw_activity_feed(frame, act_area);
            log_draw_section("activity", t.elapsed());
        }
        let t = std::time::Instant::now();
        self.draw_status_bar(frame, bar_area);
        log_draw_section("status", t.elapsed());

        // Draw input overlay if active (sessions mode only).
        if matches!(self.view_mode, ViewMode::Sessions) {
            // A-i peek: the draw computes the wrapped content height (the
            // only place the final wrap width is known) and reports the max
            // scroll offset; it's written back into the modal state after
            // the immutable borrow of `input_mode` ends.
            let mut peek_max: Option<u16> = None;
            match &self.input_mode {
                InputMode::NewSession {
                    label_text,
                    branch_text,
                    idle_timeout_text,
                    repo_url,
                    seed_from,
                    host_id,
                    active_field,
                } => {
                    self.draw_input_dialog(
                        frame,
                        area,
                        label_text,
                        branch_text,
                        idle_timeout_text,
                        repo_url,
                        seed_from.as_deref(),
                        host_id,
                        *active_field,
                    );
                }
                InputMode::NewTerminalSession {
                    workspace_id,
                    session_type,
                    seed_from,
                    active_field,
                    ..
                } => {
                    self.draw_new_terminal_dialog(
                        frame,
                        area,
                        workspace_id,
                        session_type,
                        seed_from.as_deref(),
                        *active_field,
                    );
                }
                InputMode::SessionSettings { name, idle_timeout, burst_threshold, hidden, notify_on_idle, global_perms, color, seeded_from_snapshot, active_field, .. } => {
                    self.draw_session_settings(
                        frame,
                        area,
                        name,
                        idle_timeout,
                        burst_threshold,
                        *hidden,
                        *notify_on_idle,
                        *global_perms,
                        color.as_deref(),
                        seeded_from_snapshot.as_deref(),
                        *active_field,
                    );
                }
                InputMode::WorkspaceSettings { name, color, pinned, active_field, .. } => {
                    self.draw_workspace_settings(
                        frame,
                        area,
                        name,
                        color.as_deref(),
                        *pinned,
                        *active_field,
                    );
                }
                InputMode::SaveSnapshot {
                    name_text,
                    description_text,
                    active_field,
                    error,
                    ..
                } => {
                    self.draw_save_snapshot(
                        frame,
                        area,
                        name_text,
                        description_text,
                        *active_field,
                        error.as_deref(),
                    );
                }
                InputMode::SnapshotCatalog {
                    snapshots,
                    selected,
                    mode,
                    picker_target,
                    status_msg,
                } => {
                    self.draw_snapshot_catalog(
                        frame,
                        area,
                        snapshots,
                        *selected,
                        mode,
                        picker_target.is_some(),
                        status_msg.as_deref(),
                    );
                }
                InputMode::WorkflowPicker { names, selected, .. } => {
                    self.draw_workflow_picker(frame, area, names, *selected);
                }
                InputMode::WorkflowLaunchConfirm { ws_id, workflow_name, slots, active_slot, goal, .. } => {
                    self.draw_workflow_launch(
                        frame,
                        area,
                        ws_id,
                        workflow_name,
                        slots,
                        *active_slot,
                        goal,
                    );
                }
                InputMode::TaskSettings { name, color, active_field, .. } => {
                    self.draw_task_settings(frame, area, name, color.as_deref(), *active_field);
                }
                InputMode::WorkflowHistory { run_id } => {
                    self.draw_workflow_history(frame, area, run_id);
                }
                InputMode::PastWorkspacePicker { candidates, selected } => {
                    self.draw_past_workspace_picker(frame, area, candidates, *selected);
                }
                InputMode::SessionPalette { candidates, query, selected } => {
                    self.draw_session_palette(frame, area, candidates, query, *selected);
                }
                InputMode::TaskPeek { lines, scroll, .. } => {
                    peek_max = Some(self.draw_task_peek(frame, area, lines, *scroll));
                }
                InputMode::Confirm { prompt, .. } => {
                    self.draw_confirm(frame, area, prompt);
                }
                InputMode::Normal => {}
            }
            if let Some(m) = peek_max {
                if let InputMode::TaskPeek { scroll, max_scroll, .. } = &mut self.input_mode {
                    *max_scroll = m;
                    *scroll = (*scroll).min(m);
                }
            }
        }
    }

    /// Minimal dialog for renaming a task from the sidebar.
    fn draw_task_settings(
        &self,
        frame: &mut Frame,
        area: Rect,
        name: &str,
        color: Option<&str>,
        active_field: u8,
    ) {
        let width = 60u16.min(area.width.saturating_sub(4));
        let height = 7u16;
        let x = (area.width.saturating_sub(width)) / 2;
        let y = (area.height.saturating_sub(height)) / 2;
        let dialog_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::TEXT))
            .title(" Task Settings ");
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let dim = Style::default().fg(theme::DIM);
        let white = Style::default().fg(theme::TEXT);
        let name_cursor = if active_field == 0 { "\u{2588}" } else { "" };
        let lines = vec![
            Line::from(vec![
                Span::styled("   Name: ", dim),
                Span::styled(name, white),
                Span::styled(name_cursor, white),
            ]),
            Line::from(""),
            {
                let mut spans = vec![Span::styled(
                    "  Color: ",
                    if active_field == 1 { white } else { dim },
                )];
                spans.extend(color_picker_spans(color, active_field == 1));
                Line::from(spans)
            },
            Line::from(""),
            Line::from(Span::styled(
                "Tab next \u{00b7} Enter save \u{00b7} Esc cancel",
                dim,
            )),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_confirm(&self, frame: &mut Frame, area: Rect, prompt: &str) {
        let width = 70u16.min(area.width.saturating_sub(4));
        let height = 5u16;
        let x = (area.width.saturating_sub(width)) / 2;
        let y = (area.height.saturating_sub(height)) / 2;
        let dialog_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::ATTN))
            .title(" Confirm ");
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let dim = Style::default().fg(theme::DIM);
        let white = Style::default().fg(theme::TEXT);
        let lines = vec![
            Line::from(Span::styled(prompt.to_string(), white)),
            Line::from(""),
            Line::from(Span::styled("y/Enter confirm \u{00b7} n/Esc cancel", dim)),
        ];
        frame.render_widget(Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }), inner);
    }

    fn draw_input_dialog(
        &self,
        frame: &mut Frame,
        area: Rect,
        label_text: &str,
        branch_text: &str,
        idle_timeout_text: &str,
        repo_url: &str,
        seed_from: Option<&str>,
        host_id: &cm_daemon::host_id::HostId,
        active_field: u8,
    ) {
        let width = 60u16.min(area.width.saturating_sub(4));
        // +1 row over the pre-host-picker layout for the host line.
        let height = 14u16;
        let x = (area.width.saturating_sub(width)) / 2;
        let y = (area.height.saturating_sub(height)) / 2;
        let dialog_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::TEXT))
            .title(Span::styled(
                " New Workspace ",
                Style::default()
                    .fg(theme::TEXT)
                    .add_modifier(Modifier::BOLD),
            ));

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let repo_name = repo_url
            .trim_end_matches('/')
            .trim_end_matches(".git")
            .rsplit('/')
            .next()
            .unwrap_or(repo_url);

        let cursor = "\u{2588}";
        let dim = Style::default().fg(theme::DIM);
        let white = Style::default().fg(theme::TEXT);
        let highlight = Style::default()
            .fg(theme::TEXT)
            .add_modifier(Modifier::BOLD);

        let repo_style = if active_field == 0 { highlight } else { white };
        let repo_hint = if active_field == 0 && self.config.repos.len() > 1 {
            "  \u{2190}/\u{2192} change"
        } else {
            ""
        };
        let name_cursor = if active_field == 1 { cursor } else { "" };
        let branch_cursor = if active_field == 2 { cursor } else { "" };
        let timeout_cursor = if active_field == 3 { cursor } else { "" };

        let branch_hint = if branch_text.trim() == "." {
            "  in-place (main repo, no worktree)"
        } else if branch_text.is_empty() && active_field != 2 {
            "main"
        } else {
            ""
        };

        let seed_label = sanitize_for_display(seed_from.unwrap_or("[none]"));
        let seed_style = if active_field == 4 { highlight } else { white };
        let seed_hint = match (active_field == 4, seed_from.is_some()) {
            (true, true) => "  Esc clear",
            (true, false) => "  Enter pick",
            _ => "",
        };

        let host_label = sanitize_for_display(host_id.as_str());
        let host_style = if active_field == 5 { highlight } else { white };
        // ←/→ only does something with more than one configured host.
        let host_hint = if active_field == 5 && self.hosts.hosts.len() > 1 {
            "  \u{2190}/\u{2192} change"
        } else {
            ""
        };

        let lines = vec![
            Line::from(vec![
                Span::styled("    Repo: ", dim),
                Span::styled(repo_name, repo_style),
                Span::styled(repo_hint, dim),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("    Name: ", dim),
                Span::styled(label_text, white),
                Span::styled(name_cursor, white),
            ]),
            Line::from(vec![
                Span::styled("  Branch: ", dim),
                Span::styled(branch_text, white),
                Span::styled(branch_cursor, white),
                Span::styled(branch_hint, dim),
            ]),
            Line::from(vec![
                Span::styled("Idle (s): ", dim),
                Span::styled(idle_timeout_text, white),
                Span::styled(timeout_cursor, white),
            ]),
            Line::from(vec![
                Span::styled("    Seed: ", dim),
                Span::styled(seed_label, seed_style),
                Span::styled(seed_hint, dim),
            ]),
            Line::from(vec![
                Span::styled("    Host: ", dim),
                Span::styled(host_label, host_style),
                Span::styled(host_hint, dim),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "Tab switch field \u{00b7} Enter start \u{00b7} Esc cancel",
                dim,
            )),
        ];

        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_new_terminal_dialog(
        &self,
        frame: &mut Frame,
        area: Rect,
        workspace_id: &str,
        session_type: &str,
        seed_from: Option<&str>,
        active_field: u8,
    ) {
        let width = 50u16.min(area.width.saturating_sub(4));
        // +2 rows for the seed-from line.
        let height = 11u16;
        let x = (area.width.saturating_sub(width)) / 2;
        let y = (area.height.saturating_sub(height)) / 2;
        let dialog_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, dialog_area);

        // Resolve workspace name from the stable id at render time —
        // tolerates a reorder while the form is open.
        let ws_name = self
            .workspaces
            .iter()
            .find(|w| w.id == workspace_id)
            .map(|w| w.name.as_str())
            .unwrap_or("?");

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::TEXT))
            .title(Span::styled(
                " Add Session ",
                Style::default()
                    .fg(theme::TEXT)
                    .add_modifier(Modifier::BOLD),
            ));

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let options = ["claude", "codex", "bash"];
        let max_name = (width as usize).saturating_sub(8);
        let display_name: String = ws_name.chars().take(max_name).collect();

        let dim = Style::default().fg(theme::DIM);
        let white = Style::default().fg(theme::TEXT);
        let highlight = Style::default()
            .fg(theme::TEXT)
            .add_modifier(Modifier::BOLD);

        let mut lines = vec![
            Line::from(vec![
                Span::styled("  Task: ", dim),
                Span::styled(display_name, white),
            ]),
            Line::from(""),
        ];
        // The session-type rows are field 0 and j/k cycle them in place.
        // A `▸` marker on the active row group makes it easy to see which
        // field has focus once the seed-from line is in play below.
        let type_marker = if active_field == 0 { "▸ " } else { "  " };
        for opt in &options {
            let ind = if session_type == *opt { ">" } else { " " };
            let st = if session_type == *opt {
                if active_field == 0 { highlight } else { white }
            } else {
                Style::default().fg(theme::MUTED)
            };
            lines.push(Line::from(Span::styled(
                format!("{}{} {}", type_marker, ind, opt),
                st,
            )));
        }
        lines.push(Line::from(""));
        let seed_label = sanitize_for_display(seed_from.unwrap_or(
            if session_type == "bash" { "[N/A]" } else { "[none]" },
        ));
        let seed_style = if active_field == 1 { highlight } else { white };
        let seed_hint = match (active_field == 1, seed_from.is_some(), session_type) {
            (true, _, "bash") => "  not pickable",
            (true, true, _) => "  Esc clear",
            (true, false, _) => "  Enter pick",
            _ => "",
        };
        lines.push(Line::from(vec![
            Span::styled("  Seed: ", dim),
            Span::styled(seed_label, seed_style),
            Span::styled(seed_hint, dim),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Tab field \u{00b7} j/k type \u{00b7} Enter start \u{00b7} Esc cancel",
            dim,
        )));

        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_session_settings(
        &self,
        frame: &mut Frame,
        area: Rect,
        name: &str,
        idle_timeout: &str,
        burst_threshold: &str,
        hidden: bool,
        notify_on_idle: bool,
        global_perms: bool,
        color: Option<&str>,
        seeded_from_snapshot: Option<&str>,
        active_field: u8,
    ) {
        let width = 55u16.min(area.width.saturating_sub(4));
        // Seeded-from line is a 2-line block (blank + "Seeded from: <name>")
        // only when the field is set; otherwise the dialog keeps its old
        // size so the unrelated common case doesn't grow. +2 rows for the
        // global-perms field (blank + line), +2 for the color picker.
        let height = if seeded_from_snapshot.is_some() { 21u16 } else { 19u16 };
        let x = (area.width.saturating_sub(width)) / 2;
        let y = (area.height.saturating_sub(height)) / 2;
        let dialog_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::TEXT))
            .title(Span::styled(
                " Session Settings ",
                Style::default()
                    .fg(theme::TEXT)
                    .add_modifier(Modifier::BOLD),
            ));

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let cursor = "\u{2588}";
        let dim = Style::default().fg(theme::DIM);
        let white = Style::default().fg(theme::TEXT);

        let name_cursor = if active_field == 0 { cursor } else { "" };
        let timeout_cursor = if active_field == 1 { cursor } else { "" };
        let burst_cursor = if active_field == 2 { cursor } else { "" };
        let hidden_marker = if hidden { "[x]" } else { "[ ]" };
        let hidden_style = if active_field == 3 { white } else { dim };
        let notify_marker = if notify_on_idle { "[x]" } else { "[ ]" };
        let notify_style = if active_field == 4 { white } else { dim };
        let perms_marker = if global_perms { "[x]" } else { "[ ]" };
        // Highlight the grant in yellow when on — it's a privileged,
        // cross-session capability, not a routine toggle.
        let perms_style = if active_field == 5 {
            Style::default().fg(theme::ATTN)
        } else if global_perms {
            Style::default().fg(theme::ATTN).add_modifier(Modifier::DIM)
        } else {
            dim
        };

        let mut lines = vec![
            Line::from(vec![
                Span::styled("           Name: ", dim),
                Span::styled(name, white),
                Span::styled(name_cursor, white),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("       Idle (s): ", dim),
                Span::styled(idle_timeout, white),
                Span::styled(timeout_cursor, white),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("Burst (wakeups): ", dim),
                Span::styled(burst_threshold, white),
                Span::styled(burst_cursor, white),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("         Hidden: ", dim),
                Span::styled(hidden_marker, hidden_style),
                Span::styled(if active_field == 3 { "  Space to toggle" } else { "" }, dim),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled(" Notify on idle: ", dim),
                Span::styled(notify_marker, notify_style),
                Span::styled(if active_field == 4 { "  Space to toggle" } else { "" }, dim),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("   Global perms: ", dim),
                Span::styled(perms_marker, perms_style),
                Span::styled(
                    if active_field == 5 {
                        "  Space — control ANY session"
                    } else {
                        ""
                    },
                    dim,
                ),
            ]),
            Line::from(""),
            {
                let mut spans = vec![Span::styled(
                    "          Color: ",
                    if active_field == 6 { white } else { dim },
                )];
                spans.extend(color_picker_spans(color, active_field == 6));
                Line::from(spans)
            },
        ];

        if let Some(snap) = seeded_from_snapshot {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("   Seeded from: ", dim),
                Span::styled(snap, white),
            ]));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Tab next field \u{00b7} Enter save \u{00b7} Esc cancel",
            dim,
        )));

        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_save_snapshot(
        &self,
        frame: &mut Frame,
        area: Rect,
        name_text: &str,
        description_text: &str,
        active_field: u8,
        error: Option<&str>,
    ) {
        let width = 60u16.min(area.width.saturating_sub(4));
        // Base = 11 rows (title, name, blank, description, blank, blank,
        // hint). +2 when an error is being shown.
        let height = if error.is_some() { 13u16 } else { 11u16 };
        let x = (area.width.saturating_sub(width)) / 2;
        let y = (area.height.saturating_sub(height)) / 2;
        let dialog_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::TEXT))
            .title(Span::styled(
                " Save Snapshot ",
                Style::default()
                    .fg(theme::TEXT)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let cursor = "\u{2588}";
        let dim = Style::default().fg(theme::DIM);
        let white = Style::default().fg(theme::TEXT);
        let red = Style::default().fg(theme::ERROR);

        let name_cursor = if active_field == 0 { cursor } else { "" };
        let desc_cursor = if active_field == 1 { cursor } else { "" };

        let mut lines = vec![
            Line::from(vec![
                Span::styled("        Name: ", dim),
                Span::styled(name_text, white),
                Span::styled(name_cursor, white),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled(" Description: ", dim),
                Span::styled(description_text, white),
                Span::styled(desc_cursor, white),
            ]),
            Line::from(""),
        ];

        if let Some(msg) = error {
            // Validation errors that quote an offending character can
            // include ESC / control bytes — sanitize on render so they
            // don't drive the terminal.
            lines.push(Line::from(Span::styled(sanitize_for_display(msg), red)));
            lines.push(Line::from(""));
        }

        lines.push(Line::from(Span::styled(
            "Tab switch field \u{00b7} Enter save \u{00b7} Esc cancel",
            dim,
        )));

        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_workspace_settings(
        &self,
        frame: &mut Frame,
        area: Rect,
        name: &str,
        color: Option<&str>,
        pinned: bool,
        active_field: u8,
    ) {
        let width = 55u16.min(area.width.saturating_sub(4));
        let height = 9u16;
        let x = (area.width.saturating_sub(width)) / 2;
        let y = (area.height.saturating_sub(height)) / 2;
        let dialog_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::TEXT))
            .title(Span::styled(
                " Workspace Settings ",
                Style::default()
                    .fg(theme::TEXT)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let dim = Style::default().fg(theme::DIM);
        let white = Style::default().fg(theme::TEXT);
        let name_cursor = if active_field == 0 { "\u{2588}" } else { "" };
        let pinned_marker = if pinned { "[x]" } else { "[ ]" };
        let pinned_style = if active_field == 2 { white } else { dim };
        let lines = vec![
            Line::from(vec![
                Span::styled("    Name: ", dim),
                Span::styled(name, white),
                Span::styled(name_cursor, white),
            ]),
            Line::from(""),
            {
                let mut spans = vec![Span::styled(
                    "   Color: ",
                    if active_field == 1 { white } else { dim },
                )];
                spans.extend(color_picker_spans(color, active_field == 1));
                Line::from(spans)
            },
            Line::from(""),
            Line::from(vec![
                Span::styled("  Pinned: ", dim),
                Span::styled(pinned_marker, pinned_style),
                Span::styled(
                    if active_field == 2 { "  Space to toggle" } else { "" },
                    dim,
                ),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "Tab next \u{00b7} Enter save \u{00b7} Esc cancel  (branch unchanged)",
                dim,
            )),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_terminal(&self, frame: &mut Frame, area: Rect) {
        let has_session = self.active_session().is_some();

        let title_style = if has_session {
            Style::default().fg(theme::TEXT)
        } else {
            Style::default().fg(theme::DIM)
        };

        // Border stays calm DIM chrome except for a genuine alarm: a
        // reconnecting `⟳` session tints it ATTN. Running/afterglow state
        // deliberately does NOT tint the window border (owner call,
        // 2026-07-18 — the persistent green frame around the whole session
        // window was distracting); the sidebar indicator remains the
        // per-session state signal.
        let border_color = match self.active_session() {
            Some((_, ts)) if self.reconnecting_sessions.contains(&ts.uid) => theme::ATTN,
            _ => theme::DIM,
        };

        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .title(Span::styled(self.active_title(), title_style));

        // Scrollback cue: when the active session's viewport is scrolled up
        // (not showing the live tail), surface a right-aligned ATTN tag in
        // the top border. Reading the offset per-frame means it vanishes the
        // moment the view returns to the tail (any input / Scroll::Bottom).
        let scrolled_back = self
            .active_session()
            .map(|(_, ts)| {
                crate::terminal_widget::scrollback_offset(&ts.session.term) > 0
            })
            .unwrap_or(false);
        if scrolled_back {
            block = block.title_top(
                Line::from(Span::styled(
                    " \u{25b2} scrollback ",
                    Style::default().fg(theme::ATTN),
                ))
                .right_aligned(),
            );
        }

        let inner = block.inner(area);
        frame.render_widget(block, area);

        if let Some((_, ts)) = self.active_session() {
            let widget = TerminalWidget::new(&ts.session.term, true);
            frame.render_widget(widget, inner);
        } else if let Some(wi) = self.active_workspace_index() {
            let ws = &self.workspaces[wi];
            let mut lines = vec![];
            // Show prompt + repo from first bound task, if any.
            if let Some(task) = self.first_task_for_ws(&ws.id) {
                if let Some(ref prompt) = task.prompt {
                    lines.push(Line::from(Span::styled(
                        prompt.as_str(),
                        Style::default().fg(theme::TEXT),
                    )));
                    lines.push(Line::from(""));
                }
            }
            if let Some(ref repo) = ws.repo_url {
                lines.push(Line::from(Span::styled(
                    format!("Repo: {}", repo),
                    Style::default().fg(theme::DIM),
                )));
            }
            if let Some(ref vm) = ws.worker_vm {
                lines.push(Line::from(Span::styled(
                    format!("VM: {}", vm),
                    Style::default().fg(theme::DIM),
                )));
            }
            if !lines.is_empty() {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(Span::styled(
                if ws.worker_vm.is_some() {
                    "Press Alt+A to SSH into this session"
                } else {
                    "Press Alt+A to attach"
                },
                Style::default().fg(theme::DIM),
            )));

            frame.render_widget(Paragraph::new(lines), inner);
        } else {
            let msg = if self.connected {
                Paragraph::new(
                    "No tasks \u{2014} press Alt+n to start a local session",
                )
                .style(Style::default().fg(theme::DIM))
            } else {
                Paragraph::new("Connecting to API...")
                    .style(Style::default().fg(theme::DIM))
            };
            frame.render_widget(msg, inner);
        }
    }

    /// Render the dedicated continuous column (S3) — orchestrators (depth 0)
    /// with their spawned subtasks nested (depth 1, `├`-prefixed) from
    /// `visual_items_continuous()`. Reuses `draw_session_list`'s indicator
    /// glyphs (reconnecting `⟳` / hidden / Running spinner / Idle `●` / alert).
    /// The cursor highlight + the `├`/`└` corner polish land in S4/S5.
    /// A continuous-column row "needs the operator" when its planning task is
    /// raw-`blocked`. The orchestrators set `blocked` ONLY for a fix-ready
    /// subtask awaiting review or an explicit human decision
    /// (needs_human_decision / long_review / source_down); everything they
    /// advance themselves stays `running`. We read the RAW `api_status` here
    /// (not `task_status()`, which derives `Blocked` from any idle session and
    /// would flag every idle row as needing a human).
    fn session_needs_human(&self, ts: &TerminalSession) -> bool {
        let Some(tid) = ts.task_id.as_deref() else {
            return false;
        };
        self.tasks
            .iter()
            .any(|t| t.task_id.as_deref() == Some(tid) && matches!(t.api_status, TaskStatus::Blocked))
    }

    /// P3 (Feature 1): the operator-facing question an orchestrator has parked on
    /// its planning task's metadata (`metadata.operator_question`), if any. A
    /// headless orchestrator can't call the TUI-only `notify_user`, so it sets
    /// this via `update_task(metadata=…)` (daemon-routed, works headless) and it
    /// rides the existing task-metadata sync (`reconcile_tasks`) to here —
    /// rendered as a distinct `◉` glyph + an inline text line in the continuous
    /// column so a pending decision (e.g. a `needs_human_decision`) is visible
    /// without attaching. The orchestrator clears it by writing the key back to
    /// null once resolved (the question is pending until IT resolves, not until
    /// the operator glances). Empty/whitespace is treated as absent.
    fn session_question(&self, ts: &TerminalSession) -> Option<String> {
        let tid = ts.task_id.as_deref()?;
        let task = self.tasks.iter().find(|t| t.task_id.as_deref() == Some(tid))?;
        Self::extract_operator_question(task.metadata.as_ref())
    }

    /// Pull `operator_question` out of a task's metadata bag, treating an
    /// empty/whitespace value (or a missing key / non-string) as absent — so a
    /// blank string never lights a content-less `◉`. Pure, so it's unit-testable
    /// without an App or a live session.
    fn extract_operator_question(metadata: Option<&serde_json::Value>) -> Option<String> {
        let q = metadata?.get("operator_question")?.as_str()?.trim();
        if q.is_empty() {
            None
        } else {
            Some(q.to_string())
        }
    }

    /// Dispatch-pending issues to render under an orchestrator row (gap found
    /// 2026-07-18): index issues an operator UNBLOCKED (cleared
    /// `blocked_reason` + dated `OPERATOR` directive, per the daemon's
    /// `continuous.dispatch_pending` scan) that the orchestrator has neither
    /// acknowledged (`operator_ack`) nor dispatched. The daemon can't see
    /// planning rows, so the liveness half of "not dispatched" is applied
    /// HERE: an issue whose `subtask_task_id` maps to a live planning task is
    /// dropped — the orchestrator already acted on it.
    pub(super) fn session_dispatch_pending(
        &self,
        ts: &TerminalSession,
    ) -> Vec<&cm_daemon::continuous::dispatch_pending::PendingIssue> {
        let Some(ct) = ts.continuous_task_id.as_deref() else {
            return Vec::new();
        };
        let Some(issues) = self
            .continuous_dispatch_pending
            .get(&ts.host_id)
            .and_then(|m| m.get(ct))
        else {
            return Vec::new();
        };
        issues
            .iter()
            .filter(|i| Self::issue_awaits_dispatch(i, &self.tasks))
            .collect()
    }

    /// Planning-liveness half of the dispatch-pending predicate. Pure, so
    /// it's unit-testable without an App: an issue still awaits dispatch
    /// unless its `subtask_task_id` maps to a non-done planning row (Done —
    /// or a task_id the board doesn't know, e.g. long-archived — is NOT
    /// live: a finished subtask doesn't clear a directive, only the
    /// orchestrator's ack or a fresh dispatch does).
    fn issue_awaits_dispatch(
        issue: &cm_daemon::continuous::dispatch_pending::PendingIssue,
        tasks: &[TaskEntry],
    ) -> bool {
        match issue.subtask_task_id.as_deref() {
            None => true,
            Some(tid) => !tasks.iter().any(|t| {
                t.task_id.as_deref() == Some(tid)
                    && !matches!(t.api_status, TaskStatus::Done)
            }),
        }
    }

    fn draw_continuous_column(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::DIM))
            .title(Span::styled(
                " Continuous ",
                Style::default().fg(theme::TEXT),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height < 2 || inner.width < 4 {
            return;
        }

        let spinner = self.spinner_frame();
        // One clock sample for every row's idle-age bucket this frame.
        let now = Instant::now();
        let rows = self.visual_items_continuous();
        let mut items: Vec<ListItem> = Vec::new();
        for (i, r) in rows.iter().take(inner.height as usize).enumerate() {
            let ts = &self.workspaces[r.ws_idx].sessions[r.sess_idx];
            // P3 (Feature 1): a parked operator-question wins the idle glyph and
            // adds a dim inline text line below the row (see the second Line push).
            let question = self.session_question(ts);
            // Dispatch-pending sub-lines (hollow yellow ○): operator-unblocked
            // index issues the orchestrator hasn't acted on yet. Orchestrator
            // rows only — a subtask row can't own index-level state.
            let pending = if r.depth == 0 {
                self.session_dispatch_pending(ts)
            } else {
                Vec::new()
            };
            // Idle-age bucket — mirrors the main sidebar: tints the
            // needs-human ● and, when stale, dims the row's label.
            let idle_bucket = (ts.status == SessionStatus::Idle
                && !ts.session.exited)
                .then(|| idle_age_bucket_at(ts.idle_since, now));
            let (indicator, indicator_style) = if self.reconnecting_sessions.contains(&ts.uid) {
                ("\u{27f3}", Style::default().fg(theme::ATTN))
            } else if ts.hidden {
                (" ", Style::default())
            } else {
                match ts.status {
                    SessionStatus::Running => (spinner, Style::default().fg(theme::OK)),
                    // Idle splits three ways: ◉ (cyan) = a pending operator
                    // QUESTION the orchestrator parked (metadata.operator_question);
                    // ● = the operator must act (fix-ready to review, or an
                    // explicit human decision — raw planning status `blocked`),
                    // age-tinted like the main sidebar's idle dot (afterglow /
                    // white / dim); ◇ (dim) = the orchestrator will advance it
                    // on its next fire. (A fourth state — ○ hollow yellow,
                    // dispatch pending — is a per-ISSUE sub-line below the row,
                    // not a row glyph: the operator unblocked an index issue and
                    // the orchestrator hasn't acked/dispatched it yet.)
                    SessionStatus::Idle => {
                        if question.is_some() {
                            ("\u{25c9}", Style::default().fg(theme::HEADER))
                        } else if self.session_needs_human(ts) {
                            let color = match idle_bucket {
                                Some(IdleAgeBucket::Afterglow) => theme::AFTERGLOW,
                                Some(IdleAgeBucket::Stale) => theme::DIM,
                                _ => theme::TEXT,
                            };
                            ("\u{25cf}", Style::default().fg(color))
                        } else {
                            ("\u{25c7}", Style::default().fg(theme::DIM))
                        }
                    }
                }
            };
            let (indicator, indicator_style) = if self.session_has_alert(&ts.uid) {
                self.alert_indicator()
            } else {
                (indicator, indicator_style)
            };

            // depth 0 → 4 cells, depth 1 → 6, depth 2 (session nested under a
            // subtask) → 8 — each level indents 2 more before the label.
            let prefix_cells = 4 + (r.depth as usize) * 2;
            let max_name = (inner.width as usize).saturating_sub(prefix_cells);
            let label = crate::planning::truncate_with_ellipsis(&ts.label, max_name);

            let mut spans = vec![Span::styled(
                format!(" {} ", indicator),
                indicator_style,
            )];
            if r.depth >= 1 {
                // Tree glyph for a nested row (subtask under orchestrator, or a
                // session under a subtask). Extra 2-space indent per level below
                // depth 1 so a depth-2 session sits under its subtask's agent.
                if r.depth >= 2 {
                    spans.push(Span::raw("  ".repeat((r.depth - 1) as usize)));
                }
                // LAST sibling at THIS depth gets the corner `└`, earlier ones
                // the tee `├`. Look past any DEEPER-nested rows to the next row
                // at depth <= mine: if it's shallower (or none) I'm the last
                // sibling; if it's the same depth another sibling follows.
                let is_last_child = rows[i + 1..]
                    .iter()
                    .find(|nx| nx.depth <= r.depth)
                    .map_or(true, |nx| nx.depth < r.depth);
                let glyph = if is_last_child {
                    "\u{2514} "
                } else {
                    "\u{251c} "
                };
                spans.push(Span::styled(
                    glyph,
                    Style::default().fg(theme::DIM),
                ));
            }
            // Focus highlight: only when the cursor is actually IN this column
            // (S4). Bold-white, matching the main sidebar's selected style.
            let is_selected = self.cursor_column == SidebarColumn::Continuous
                && matches!(
                    &self.cursor,
                    Cursor::Session(cwi, csi) if *cwi == r.ws_idx && *csi == r.sess_idx
                );
            let label_style = if is_selected {
                Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)
            } else if matches!(idle_bucket, Some(IdleAgeBucket::Stale))
                && !self.session_has_alert(&ts.uid)
                && !self.reconnecting_sessions.contains(&ts.uid)
            {
                // Stale-idle rows fade like the main sidebar's — see the
                // matching branch in `draw_session_list`.
                Style::default().fg(theme::DIM)
            } else if r.depth == 0 {
                // Orchestrators (depth 0) get a slightly brighter label so the
                // parent/child split reads at a glance.
                Style::default().fg(theme::MUTED).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::MUTED)
            };
            spans.push(Span::styled(label, label_style));
            let item_style = if is_selected {
                Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            // P3 (Feature 1): render the parked question as a dim second line
            // under the orchestrator row so it's readable without attaching.
            let mut lines = vec![Line::from(spans)];
            if let Some(q) = &question {
                let qmax = (inner.width as usize).saturating_sub(prefix_cells + 2);
                let qtext = crate::planning::truncate_with_ellipsis(q, qmax);
                lines.push(Line::from(vec![
                    Span::raw(" ".repeat(prefix_cells)),
                    Span::styled(
                        format!("\u{21b3} {}", qtext),
                        Style::default().fg(theme::HEADER).add_modifier(Modifier::DIM),
                    ),
                ]));
            }
            // Dispatch-pending issues: one hollow-yellow ○ line each, under
            // the orchestrator row (same visual slot as the question line) —
            // approved-awaiting-dispatch is visible without attaching, and
            // distinct from ● (operator must act) / ◇ (orchestrator has it).
            for issue in &pending {
                let imax = (inner.width as usize).saturating_sub(prefix_cells + 2);
                let itext = crate::planning::truncate_with_ellipsis(
                    &format!(
                        "{} · dispatch pending ({})",
                        issue.issue_id, issue.directive_date
                    ),
                    imax,
                );
                lines.push(Line::from(vec![
                    Span::raw(" ".repeat(prefix_cells)),
                    Span::styled(
                        format!("\u{25cb} {}", itext),
                        Style::default().fg(theme::ATTN),
                    ),
                ]));
            }
            items.push(ListItem::new(lines).style(item_style));
        }
        if items.is_empty() {
            items.push(ListItem::new(Line::from(Span::styled(
                "  (no continuous tasks)",
                Style::default().fg(theme::DIM),
            ))));
        }
        frame.render_widget(List::new(items), inner);
    }

    fn draw_session_list(&self, frame: &mut Frame, area: Rect) {
        let view_label = match self.sidebar_view {
            SidebarView::Status => " Sessions ",
            SidebarView::Task => " Tasks ",
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::DIM))
            .title(Span::styled(
                view_label,
                Style::default().fg(theme::TEXT),
            ));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.height < 2 || inner.width < 4 {
            return;
        }

        let spinner = self.spinner_frame();
        // One clock sample for every row's idle-age bucket this frame.
        let now = Instant::now();
        let dim = Style::default().fg(theme::DIM);

        // Help text — two columns. Defined up here so `list_height` can size
        // itself around the help footer (otherwise the help overdraws the
        // bottom rows of the list and indicators vanish).
        let help_entries: Vec<(&str, &str)> = vec![
            ("A-j/k  nav", "A-d  done"),
            ("A-h/l  column", "A-x  delete"),
            ("A-a    attach", "A-v  view"),
            ("A-n    new ws", "A-r  refresh"),
            ("A-s    +session", "A-q  quit"),
            ("A-w    close sess", "A-y  history"),
            ("A-W    close ws", "A-u  resume"),
            ("A-e    settings", "A-,  activity"),
            ("A-H    hide", "A-z  catalog"),
            ("A-f    workflow", "A-t  planning"),
            ("A-o    stop wf", "A-c  cont-col"),
            ("A-b    snapshot", "A-g  attention"),
            ("A-O    reopen ws", "A-9  push"),
            ("PgUp/Dn scroll", "A-0  pull"),
            ("A-Ent  newline", "A-;  recent"),
            ("A-p    find", "A-i  info"),
            ("A-'    yank", "A-m  mouse"),
        ];
        let help_rows = help_entries.len() as u16;
        let list_height = inner.height.saturating_sub(help_rows + 1);

        let visual = self.visual_items();
        let mut items: Vec<ListItem> = Vec::new();
        let max = list_height as usize;

        for vi in &visual {
            if items.len() >= max {
                break;
            }
            match vi {
                VisualItem::WorkspaceHeader(wi) => {
                    let ws = &self.workspaces[*wi];
                    let is_selected = match &self.cursor {
                        Cursor::Workspace(cwi) => cwi == wi,
                        _ => false,
                    };

                    // Remote-host indicator. The host is workspace-scoped (all
                    // sessions in a workspace share one host, by invariant) but
                    // it lives per-session as `ts.host_id`, so derive it from the
                    // first non-local session. An all-local or session-less
                    // workspace shows nothing. Status view surfaces remoteness via
                    // `HostHeader` grouping instead; this tag is what makes the
                    // Task sub-view (which has no host headers) legible.
                    let remote_host = ws
                        .sessions
                        .iter()
                        .map(|s| &s.host_id)
                        .find(|h| **h != cm_daemon::host_id::HostId::local());
                    let host_tag = remote_host
                        .map(|h| format!(" @{}", h.as_str()))
                        .unwrap_or_default();

                    // Reserve room for the tags so the name truncates before
                    // them (the pin glyph 📌 is double-width + a space).
                    let max_name = (inner.width as usize)
                        .saturating_sub(2)
                        .saturating_sub(host_tag.chars().count())
                        .saturating_sub(if ws.pinned { 3 } else { 0 });
                    // Char-boundary-safe truncation — a raw `&ws.name[..n]` byte
                    // slice panics when the cut lands inside a multibyte char
                    // (e.g. '≤' in a workspace name). Matches the session/task
                    // name truncation sites below.
                    let name = crate::planning::truncate_with_ellipsis(&ws.name, max_name);

                    let mut header_spans = vec![Span::raw(" ")];
                    if ws.pinned {
                        header_spans.push(Span::raw("\u{1f4cc} "));
                    }
                    header_spans.push(Span::raw(name));
                    if !host_tag.is_empty() {
                        // Magenta keeps it distinct from the Yellow/DarkGray host
                        // headers and Cyan task/workflow headers. The span keeps
                        // its color through selection (the name still highlights).
                        header_spans.push(Span::styled(
                            host_tag,
                            Style::default().fg(theme::REMOTE),
                        ));
                    }
                    let header_line = Line::from(header_spans);

                    // The user accent tints unselected rows; selection keeps
                    // the White+BOLD highlight so the cursor stays obvious.
                    let ws_accent = ws.color.as_deref().and_then(theme::user_color);
                    let base_style = if is_selected {
                        Style::default()
                            .fg(theme::TEXT)
                            .add_modifier(Modifier::BOLD)
                    } else if let Some(c) = ws_accent {
                        Style::default().fg(c)
                    } else {
                        Style::default().fg(theme::MUTED)
                    };

                    items.push(ListItem::new(header_line).style(base_style));
                }
                VisualItem::Session(wi, si) => {
                    let ws = &self.workspaces[*wi];
                    let ts = &ws.sessions[*si];
                    let is_selected = match &self.cursor {
                        Cursor::Session(cwi, csi) => cwi == wi && csi == si,
                        _ => false,
                    };

                    // Find enclosing workflow run, if any — controls vertical-line
                    // prefix for visual grouping in task view.
                    let in_active_workflow = ts
                        .workflow_run_id
                        .as_deref()
                        .is_some_and(|id| self.workflow_runs.iter().any(|r| r.run_id == id));

                    // Idle-age bucket — colors the idle dot (afterglow /
                    // settled / stale) and, when stale, dims the whole row.
                    let idle_bucket = (ts.status == SessionStatus::Idle
                        && !ts.session.exited)
                        .then(|| idle_age_bucket_at(ts.idle_since, now));

                    let (indicator, indicator_style) = if self
                        .reconnecting_sessions
                        .contains(&ts.uid)
                    {
                        // Remote auto-reconnect: this session's attach
                        // I/O stream died (tunnel dropped) but its
                        // daemon-side PTY keeps running. The `⟳` marks
                        // it reconnecting; the slot stays put and
                        // rebinds when connectivity returns. Shown even
                        // for hidden sessions — a stuck stream is worth
                        // surfacing.
                        ("\u{27f3}", Style::default().fg(theme::ATTN))
                    } else if ts.hidden {
                        (" ", Style::default())
                    } else {
                        match ts.status {
                            SessionStatus::Running => {
                                (spinner, Style::default().fg(theme::OK))
                            }
                            SessionStatus::Idle => {
                                // Dot color by idle age: warm afterglow for
                                // "just went idle — probably wants you",
                                // plain white while settled, dim once stale.
                                let color = match idle_bucket {
                                    Some(IdleAgeBucket::Afterglow) => theme::AFTERGLOW,
                                    Some(IdleAgeBucket::Stale) => theme::DIM,
                                    _ => theme::TEXT,
                                };
                                ("\u{25cf}", Style::default().fg(color))
                            }
                        }
                    };
                    // notify_user attention animation: while an alert is
                    // pending, the rainbow-heartbeat bead takes over the icon
                    // cell entirely (overriding even a hidden session's blank)
                    // — the whole point is to grab the eye regardless of status.
                    let (indicator, indicator_style) = if self.session_has_alert(&ts.uid) {
                        self.alert_indicator()
                    } else {
                        (indicator, indicator_style)
                    };

                    // Role badge for workflow-participant sessions, e.g.
                    // "[worker] " / "[reviewer] " / "[manager] ". Phase 6
                    // widened the sidebar so the full role name fits;
                    // single-char tags like "[W]" were too cryptic at a
                    // glance once feedback workflows became routine.
                    let wf_badge: Option<(String, Style)> =
                        if let (Some(run_id), Some(role)) =
                            (ts.workflow_run_id.as_deref(), ts.workflow_role.as_deref())
                        {
                            let active = self
                                .workflow_runs
                                .iter()
                                .any(|r| r.run_id == run_id && r.active_role.as_deref() == Some(role));
                            let style = if active {
                                Style::default().fg(theme::ATTN)
                            } else {
                                Style::default().fg(theme::HEADER)
                            };
                            Some((format!("[{}] ", role), style))
                        } else {
                            None
                        };

                    let display = match self.sidebar_view {
                        SidebarView::Status => {
                            let max_name =
                                (inner.width as usize).saturating_sub(8);
                            let full = format!("{} / {}", ws.name, ts.label);
                            crate::planning::truncate_with_ellipsis(&full, max_name)
                        }
                        SidebarView::Task => {
                            // Indent levels (Phase 6 deepened by 2 cells per tier
                            // so workflow-participant nesting reads cleanly):
                            //   - Workspace-level (no task): 2 spaces.
                            //   - Task-scoped, no workflow:  4 spaces.
                            //   - Workflow participant:      6 spaces, putting
                            //     them visually inside the task they belong to.
                            let in_active_wf = ts
                                .workflow_run_id
                                .as_deref()
                                .is_some_and(|id| {
                                    self.workflow_runs.iter().any(|r| r.run_id == id)
                                });
                            if in_active_wf {
                                format!("      {}", ts.label)
                            } else if ts.task_id.is_some() {
                                format!("    {}", ts.label)
                            } else {
                                format!("  {}", ts.label)
                            }
                        }
                    };

                    let mut spans = vec![Span::styled(
                        format!(" {} ", indicator),
                        indicator_style,
                    )];
                    // Vertical line prefix for sessions inside a workflow group
                    // (only in task view where grouping makes sense visually).
                    if in_active_workflow && self.sidebar_view == SidebarView::Task {
                        spans.push(Span::styled(
                            "\u{2502} ",
                            Style::default().fg(theme::DIM),
                        ));
                    }
                    if let Some((badge, style)) = wf_badge {
                        spans.push(Span::styled(badge, style));
                    }
                    spans.push(Span::raw(display));
                    let line = Line::from(spans);

                    // Session accent falls back to the workspace accent so a
                    // colored workspace tints its whole group; selection keeps
                    // the White+BOLD highlight.
                    let accent = ts
                        .color
                        .as_deref()
                        .or(ws.color.as_deref())
                        .and_then(theme::user_color);
                    let base_style = if is_selected {
                        Style::default()
                            .fg(theme::TEXT)
                            .add_modifier(Modifier::BOLD)
                    } else if matches!(idle_bucket, Some(IdleAgeBucket::Stale))
                        && !self.session_has_alert(&ts.uid)
                        && !self.reconnecting_sessions.contains(&ts.uid)
                    {
                        // Stale-idle rows (idle > 30 min) fade out label and
                        // all — they're background noise; a pending alert or
                        // an in-flight reconnect still gets full styling, and
                        // the selection branch above keeps the cursor legible.
                        Style::default().fg(theme::DIM)
                    } else if let Some(c) = accent {
                        Style::default().fg(c)
                    } else {
                        Style::default().fg(theme::MUTED)
                    };
                    items.push(ListItem::new(line).style(base_style));
                }
                VisualItem::Separator => {
                    let sep_line = Line::from(Span::styled(
                        format!(
                            " {}",
                            "\u{2500}"
                                .repeat(
                                    inner.width.saturating_sub(2) as usize
                                )
                        ),
                        dim,
                    ));
                    items.push(ListItem::new(sep_line));
                }
                VisualItem::WorkflowHeader { ws_idx, run_id } => {
                    let ws = &self.workspaces[*ws_idx];
                    let run = self.workflow_runs.iter().find(|r| &r.run_id == run_id);
                    let (agg_indicator, agg_style) = match run {
                        Some(r) => aggregate_indicator(r, ws, spinner),
                        None => ("\u{25cf}", Style::default().fg(theme::DIM)),
                    };
                    // If any participant of this workflow has a pending alert,
                    // blink the group header too (the participant rows blink
                    // individually, but the header keeps the signal visible
                    // when the group reads as one unit).
                    let agg_alerting = ws.sessions.iter().any(|ts| {
                        ts.workflow_run_id.as_deref() == Some(run_id.as_str())
                            && self.session_has_alert(&ts.uid)
                    });
                    let (agg_indicator, agg_style) = if agg_alerting {
                        self.alert_indicator()
                    } else {
                        (agg_indicator, agg_style)
                    };
                    let name = run
                        .map(|r| r.workflow_name.clone())
                        .unwrap_or_else(|| "workflow".into());
                    let paused_suffix = run
                        .map(|r| match r.status {
                            workflow::RunStatus::Paused => " (paused)",
                            workflow::RunStatus::Done => " (done)",
                            _ => "",
                        })
                        .unwrap_or("");
                    let line = Line::from(vec![
                        Span::styled(format!(" {} ", agg_indicator), agg_style),
                        Span::styled(
                            format!("\u{256d}\u{2500} {}{}", name, paused_suffix),
                            Style::default().fg(theme::HEADER),
                        ),
                    ]);
                    items.push(ListItem::new(line));
                }
                VisualItem::TaskHeader { ws_idx, task_id } => {
                    let is_selected = match &self.cursor {
                        Cursor::Task { ws_idx: cwi, task_id: ctid } => {
                            cwi == ws_idx && ctid == task_id
                        }
                        _ => false,
                    };
                    let name = self
                        .tasks
                        .iter()
                        .find(|t| t.task_id.as_deref() == Some(task_id.as_str()))
                        .map(|t| t.name.clone())
                        .unwrap_or_else(|| "task".into());
                    let max_name = (inner.width as usize).saturating_sub(4);
                    let name = crate::planning::truncate_with_ellipsis(&name, max_name);
                    // Style lives on the ListItem so selection highlight can
                    // override. Using Span::styled with a fixed color here
                    // would mask the base_style on selection.
                    let task_accent = self
                        .task_colors
                        .get(task_id.as_str())
                        .map(String::as_str)
                        .and_then(theme::user_color);
                    let base_style = if is_selected {
                        Style::default()
                            .fg(theme::TEXT)
                            .add_modifier(Modifier::BOLD)
                    } else if let Some(c) = task_accent {
                        Style::default().fg(c)
                    } else {
                        Style::default().fg(theme::HEADER)
                    };
                    let line = Line::from(vec![
                        Span::raw("  "),
                        Span::raw(name),
                    ]);
                    items.push(ListItem::new(line).style(base_style));
                }
                // 12e: host header rendered as a non-selectable
                // bold-ish row, one per configured host.
                VisualItem::HostHeader(host_id) => {
                    // Global-host removal: there is no "active" host anymore —
                    // every host header renders identically. The sidebar shows
                    // all configured hosts; "which host" is a per-workspace
                    // attribute, not a global mode.
                    let line = Line::from(vec![Span::styled(
                        format!("  {}", host_id.as_str()),
                        Style::default()
                            .fg(theme::DIM)
                            .add_modifier(Modifier::BOLD),
                    )]);
                    items.push(ListItem::new(line));
                }
                // Continuous-tasks section header, styled like the
                // (inactive) `HostHeader` arm. The count badge reflects
                // the continuous sessions across all visible workspaces.
                VisualItem::ContinuousHeader => {
                    let count = self
                        .workspaces
                        .iter()
                        .enumerate()
                        .filter(|(wi, ws)| !ws.is_closed && !self.is_past_workspace(*wi))
                        .flat_map(|(_, ws)| ws.sessions.iter())
                        .filter(|ts| ts.continuous_task_id.is_some())
                        .count();
                    let line = Line::from(vec![Span::styled(
                        format!("  continuous ({})", count),
                        Style::default()
                            .fg(theme::DIM)
                            .add_modifier(Modifier::BOLD),
                    )]);
                    items.push(ListItem::new(line));
                }
            }
        }

        let list = List::new(items);
        frame.render_widget(
            list,
            Rect {
                x: inner.x,
                y: inner.y,
                width: inner.width,
                height: list_height,
            },
        );

        let help_y = inner.y + inner.height.saturating_sub(help_rows + 1);
        let help_area = Rect {
            x: inner.x,
            y: help_y,
            width: inner.width,
            height: help_rows + 1,
        };

        let sep = Line::from(Span::styled(
            "\u{2500}".repeat(inner.width as usize),
            dim,
        ));
        let col = inner.width / 2;

        let mut lines = vec![sep];
        for (left, right) in &help_entries {
            let left_padded = format!("{:<w$}", left, w = col as usize);
            let line = Line::from(vec![
                Span::styled(left_padded, dim),
                Span::styled(*right, dim),
            ]);
            lines.push(line);
        }
        frame.render_widget(Paragraph::new(lines), help_area);
    }

    /// Phase 6: render the activity-feed strip (Alt-, toggle). Shows the
    /// last few `ActivityEntry`s from `self.activity_log` formatted as
    /// `HH:MM:SS  caller → summary`. The strip is bordered so it's
    /// visually distinct from the main content and the status bar.
    fn draw_activity_feed(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(theme::DIM))
            .title(Span::styled(
                " Activity (Alt-, to hide) ",
                Style::default().fg(theme::TEXT),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height == 0 || inner.width < 8 {
            return;
        }

        // Render the most recent N entries (where N is the inner height),
        // bottom-anchored so the newest entry is closest to the status bar
        // and older entries scroll upward as new ones arrive.
        let take = inner.height as usize;
        let start = self.activity_log.len().saturating_sub(take);
        let mut lines: Vec<Line> = Vec::with_capacity(take);
        for entry in self.activity_log.iter().skip(start) {
            let ts_str = format_utc_hms(entry.ts);
            // Caller column padded to a stable width so summary text
            // aligns across entries even when caller names differ.
            let caller_col_width = 10;
            let caller_padded = if entry.caller_label.chars().count() >= caller_col_width {
                entry.caller_label.chars().take(caller_col_width).collect::<String>()
            } else {
                let pad = caller_col_width - entry.caller_label.chars().count();
                format!("{}{}", entry.caller_label, " ".repeat(pad))
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{} ", ts_str),
                    Style::default().fg(theme::DIM),
                ),
                Span::styled(caller_padded, Style::default().fg(theme::HEADER)),
                Span::raw(" → "),
                Span::raw(entry.summary.clone()),
            ]));
        }
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_status_bar(&self, frame: &mut Frame, area: Rect) {
        // Degraded-mode banner. When this TUI doesn't own the control
        // socket it has NO MCP/agent control plane — `start_session`,
        // `send_input`, `start_workflow` etc. all route to whichever
        // instance does own it (or fail). That used to surface only as a
        // line in cm-tui.log; make it impossible to miss by taking over the
        // status bar with a red banner naming the PID to kill. Self-clears
        // once `maybe_rebind_control_socket` reclaims the socket.
        if !self.control_bound {
            let msg = match self.control_conflict_pid {
                Some(pid) => format!(
                    " \u{26a0} NO CONTROL PLANE — tui.sock held by another TUI (PID {pid}); \
                     MCP/agent control won't reach here. Run `kill {pid}`, then restart this TUI. ",
                ),
                None => " \u{26a0} NO CONTROL PLANE — tui.sock held by another instance; \
                     MCP/agent control won't reach here. Quit the other TUI, then restart this one. "
                    .to_string(),
            };
            // Truncate to the bar width, then pad so the red field spans it.
            let mut text: String = msg.chars().take(area.width as usize).collect();
            let used = text.chars().count() as u16;
            if used < area.width {
                text.push_str(&" ".repeat((area.width - used) as usize));
            }
            let line = Line::from(Span::styled(
                text,
                Style::default()
                    .fg(theme::TEXT)
                    .bg(theme::ERROR)
                    .add_modifier(Modifier::BOLD),
            ));
            frame.render_widget(Paragraph::new(line), area);
            return;
        }

        let running = self
            .tasks
            .iter()
            .filter(|t| matches!(self.task_status(t), TaskStatus::Running))
            .count();
        let blocked = self
            .tasks
            .iter()
            .filter(|t| matches!(self.task_status(t), TaskStatus::Blocked))
            .count();
        let backlog = self
            .tasks
            .iter()
            .filter(|t| matches!(self.task_status(t), TaskStatus::Backlog))
            .count();

        let conn_indicator = if self.connected { "\u{25cf}" } else { "\u{25cb}" };
        let conn_color = if self.connected {
            theme::OK
        } else {
            theme::ERROR
        };

        let center = if let Some((ref msg, when)) = self.status_msg {
            if when.elapsed().as_secs() < 3 {
                msg.clone()
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        let mouse_off = !self.mouse_capture_enabled;
        let mouse_indicator = if mouse_off { " [mouse off] " } else { "" };

        // Session-state rollup, e.g. `⠹2 ●1 ⚑1`: running / idle-awaiting /
        // pending-alert counts across every open workspace (continuous
        // sessions included, exited sessions excluded). Zero-count clusters
        // are omitted. The idle dot borrows the afterglow tint when any
        // session just went idle; the alert flag is steady ATTN (the
        // sidebar bead does the animating).
        let mut n_running = 0usize;
        let mut n_idle = 0usize;
        let mut n_alert = 0usize;
        let mut any_afterglow = false;
        let now = Instant::now();
        for ws in self.workspaces.iter().filter(|w| !w.is_closed) {
            for ts in &ws.sessions {
                if self.session_has_alert(&ts.uid) {
                    // An alerting session counts ONLY as an alert — it's
                    // already asking for a human, whatever its status.
                    n_alert += 1;
                    continue;
                }
                if ts.session.exited {
                    continue;
                }
                match ts.status {
                    SessionStatus::Running => n_running += 1,
                    SessionStatus::Idle => {
                        n_idle += 1;
                        if idle_age_bucket_at(ts.idle_since, now)
                            == IdleAgeBucket::Afterglow
                        {
                            any_afterglow = true;
                        }
                    }
                }
            }
        }
        let mut rollup: Vec<Span> = Vec::new();
        if n_running > 0 {
            rollup.push(Span::styled(
                format!("{}{} ", self.spinner_frame(), n_running),
                Style::default().fg(theme::OK),
            ));
        }
        if n_idle > 0 {
            let idle_color = if any_afterglow {
                theme::AFTERGLOW
            } else {
                theme::TEXT
            };
            rollup.push(Span::styled(
                format!("\u{25cf}{} ", n_idle),
                Style::default().fg(idle_color),
            ));
        }
        if n_alert > 0 {
            rollup.push(Span::styled(
                format!("\u{2691}{} ", n_alert),
                Style::default().fg(theme::ATTN),
            ));
        }

        let left1 = format!(" {} ", conn_indicator);
        let left2 = "claude-manager ";
        let right = format!(" {}r {}b {}q ", running, blocked, backlog);

        // Width math from the ACTUAL span contents (every glyph used here is
        // single-cell, so char count == display width) — no hardcoded
        // left-side constant to drift out of sync.
        let left_used = (left1.chars().count() + left2.chars().count()) as u16;
        let right_width = right.chars().count() as u16;
        let center_width = center.chars().count() as u16;
        let mouse_width = mouse_indicator.chars().count() as u16;
        let rollup_width: u16 = rollup
            .iter()
            .map(|s| s.content.chars().count() as u16)
            .sum();
        let fixed = left_used + right_width + center_width + mouse_width;
        // Degrade on narrow terminals: the rollup is the first thing to go
        // (the rest keeps the pre-rollup behavior of clipping at the edge).
        let (rollup, rollup_width) = if fixed + rollup_width > area.width {
            (Vec::new(), 0u16)
        } else {
            (rollup, rollup_width)
        };
        let pad = area.width.saturating_sub(fixed + rollup_width);
        let pad_left = pad / 2;
        let pad_right = pad - pad_left;

        let mut spans = vec![
            Span::styled(left1, Style::default().fg(conn_color)),
            Span::styled(left2, Style::default().fg(theme::DIM)),
            Span::styled(
                mouse_indicator,
                Style::default().fg(theme::BADGE_FG).bg(theme::ATTN).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " ".repeat(pad_left as usize),
                Style::default(),
            ),
            Span::styled(center, Style::default().fg(theme::ATTN)),
            Span::styled(
                " ".repeat(pad_right as usize),
                Style::default(),
            ),
        ];
        spans.extend(rollup);
        spans.push(Span::styled(right, Style::default().fg(theme::DIM)));

        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn active_title(&self) -> String {
        if let Some((ws, ts)) = self.active_session() {
            format!(" {} / {} ", ws.name, ts.label)
        } else if let Some(wi) = self.active_workspace_index() {
            format!(" {} ", self.workspaces[wi].name)
        } else {
            " Terminal ".to_string()
        }
    }

    pub fn draw_workflow_picker(
        &self,
        frame: &mut Frame,
        area: Rect,
        names: &[String],
        selected: usize,
    ) {
        let err_rows = self.workflow_load_errors.len() as u16;
        let err_pad = if err_rows > 0 { 1 } else { 0 };
        let width = area.width.min(60).max(36);
        let height = (names.len() as u16 + 5 + err_rows + err_pad)
            .min(area.height.saturating_sub(2));
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let dialog = Rect { x, y, width, height };

        frame.render_widget(Clear, dialog);

        let mut lines: Vec<Line> = Vec::new();
        for (path, err) in &self.workflow_load_errors {
            lines.push(Line::from(Span::styled(
                format_workflow_load_error(path, err),
                Style::default().fg(theme::DIM),
            )));
        }
        if !self.workflow_load_errors.is_empty() {
            lines.push(Line::from(""));
        }
        for (idx, name) in names.iter().enumerate() {
            let is_active = idx == selected;
            let cursor = if is_active { "▸ " } else { "  " };
            let desc = self
                .workflows
                .get(name)
                .map(|w| w.description.clone())
                .unwrap_or_default();
            let name_style = if is_active {
                Style::default().fg(theme::ATTN).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::HEADER)
            };
            let desc_style = if is_active {
                Style::default().fg(theme::TEXT)
            } else {
                Style::default().fg(theme::DIM)
            };
            let mut spans = vec![
                Span::raw(cursor),
                Span::styled(format!("{:<12}", name), name_style),
            ];
            if !desc.is_empty() {
                spans.push(Span::styled(desc, desc_style));
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "\u{2191}\u{2193} select   Enter: choose   Esc: cancel",
            Style::default().fg(theme::DIM),
        )));

        let block = Block::default()
            .borders(Borders::ALL)
            .title(workflow_picker_title(self.workflow_load_errors.len()))
            .style(Style::default().fg(theme::TEXT));
        let paragraph = Paragraph::new(lines).block(block);
        frame.render_widget(paragraph, dialog);
    }

    pub fn draw_past_workspace_picker(
        &self,
        frame: &mut Frame,
        area: Rect,
        candidates: &[PastCandidate],
        selected: usize,
    ) {
        let total = candidates.len();
        let width = area.width.min(80).max(40);
        // Dialog chrome: 2 border rows + 1 blank + 1 footer line.
        // Below that, each candidate (or scroll-indicator) takes one row.
        let max_dialog_height = area.height.saturating_sub(2).max(7);
        let desired_height = (total as u16).saturating_add(4).max(7);
        let height = desired_height.min(max_dialog_height);
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let dialog = Rect { x, y, width, height };

        frame.render_widget(Clear, dialog);

        let inner_height = height.saturating_sub(2) as usize;
        let footer_rows = 2; // blank + key-hint line
        let list_budget = inner_height.saturating_sub(footer_rows).max(1);
        let needs_scroll = total > list_budget;
        // Reserve one line each for "↑ N more" / "↓ N more" when scrolling.
        let body_rows = if needs_scroll {
            list_budget.saturating_sub(2).max(1)
        } else {
            list_budget
        };

        // Stable scroll: keep selected at the bottom edge when it would
        // otherwise scroll off, capped by the final page so we don't show
        // empty rows below the last candidate.
        let offset = if !needs_scroll || selected < body_rows {
            0
        } else {
            selected
                .saturating_sub(body_rows)
                .saturating_add(1)
                .min(total.saturating_sub(body_rows))
        };
        let above = offset;
        let end = (offset + body_rows).min(total);
        let below = total.saturating_sub(end);

        let dim = Style::default().fg(theme::DIM);
        let mut lines: Vec<Line> = Vec::new();

        if candidates.is_empty() {
            lines.push(Line::from(Span::styled("No past workspaces.", dim)));
        } else {
            if needs_scroll {
                if above > 0 {
                    lines.push(Line::from(Span::styled(
                        format!("  \u{2191} {} more", above),
                        dim,
                    )));
                } else {
                    lines.push(Line::from(""));
                }
            }
            for idx in offset..end {
                let cand = &candidates[idx];
                let is_active = idx == selected;
                let cursor = if is_active { "\u{25b8} " } else { "  " };
                let path_repr = cand
                    .worktree_path
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                let name_style = if !cand.worktree_exists {
                    Style::default()
                        .fg(theme::DIM)
                        .add_modifier(Modifier::CROSSED_OUT)
                } else if is_active {
                    Style::default()
                        .fg(theme::ATTN)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme::HEADER)
                };
                let path_style = if !cand.worktree_exists {
                    Style::default()
                        .fg(theme::ERROR)
                        .add_modifier(Modifier::CROSSED_OUT)
                } else if is_active {
                    Style::default().fg(theme::TEXT)
                } else {
                    Style::default().fg(theme::DIM)
                };
                let suffix = if cand.worktree_exists {
                    String::new()
                } else {
                    "  (worktree gone)".to_string()
                };
                let mut spans = vec![
                    Span::raw(cursor),
                    Span::styled(cand.display.clone(), name_style),
                ];
                if !path_repr.is_empty() {
                    spans.push(Span::raw("  "));
                    spans.push(Span::styled(path_repr.to_string(), path_style));
                }
                if !suffix.is_empty() {
                    spans.push(Span::styled(
                        suffix,
                        Style::default().fg(theme::ERROR),
                    ));
                }
                lines.push(Line::from(spans));
            }
            if needs_scroll {
                if below > 0 {
                    lines.push(Line::from(Span::styled(
                        format!("  \u{2193} {} more", below),
                        dim,
                    )));
                } else {
                    lines.push(Line::from(""));
                }
            }
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "\u{2191}\u{2193} select   Enter: reopen   Esc: cancel",
            dim,
        )));

        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Reopen past workspace ")
            .style(Style::default().fg(theme::TEXT));
        let paragraph = Paragraph::new(lines).block(block);
        frame.render_widget(paragraph, dialog);
    }

    /// A-p fuzzy-find palette. Chrome mirrors the past-workspace picker:
    /// centered dialog, one row per match, dim key-hint footer. The query
    /// line renders on top; matches are the same filtered+ranked view the
    /// handler indexes, capped at [`PALETTE_MAX_RESULTS`].
    pub fn draw_session_palette(
        &self,
        frame: &mut Frame,
        area: Rect,
        candidates: &[PaletteCandidate],
        query: &str,
        selected: usize,
    ) {
        let displays: Vec<&str> = candidates.iter().map(|c| c.display.as_str()).collect();
        let mut filtered = palette_match_indices(query, &displays);
        filtered.truncate(PALETTE_MAX_RESULTS);

        let width = area.width.min(80).max(40);
        // Chrome: 2 border rows + query + blank + rows + blank + footer.
        let rows = filtered.len().max(1) as u16;
        let height = (rows + 6).min(area.height.saturating_sub(2).max(8));
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let dialog = Rect { x, y, width, height };

        frame.render_widget(Clear, dialog);

        let dim = Style::default().fg(theme::DIM);
        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(vec![
            Span::styled("> ", dim),
            Span::styled(query.to_string(), Style::default().fg(theme::TEXT)),
            Span::styled("\u{2588}", Style::default().fg(theme::TEXT)),
        ]));
        lines.push(Line::from(""));
        if filtered.is_empty() {
            lines.push(Line::from(Span::styled("(no matches)", dim)));
        } else {
            let sel = selected.min(filtered.len() - 1);
            for (i, &ci) in filtered.iter().enumerate() {
                let is_active = i == sel;
                let marker = if is_active { "\u{25b8} " } else { "  " };
                let style = if is_active {
                    Style::default()
                        .fg(theme::TEXT)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme::MUTED)
                };
                lines.push(Line::from(vec![
                    Span::styled(marker.to_string(), style),
                    Span::styled(candidates[ci].display.clone(), style),
                ]));
            }
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "type to filter   \u{2191}\u{2193}/Tab select   Enter: jump   Esc: close",
            dim,
        )));

        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Find session ")
            .style(Style::default().fg(theme::TEXT));
        frame.render_widget(Paragraph::new(lines).block(block), dialog);
    }

    /// A-i read-only info overlay — draw_confirm's chrome scaled up
    /// (~70% width, ~60% height, centered, cleared underneath). Body
    /// wraps and scrolls; the key-hint footer stays pinned below it.
    /// Returns the max scroll offset (wrapped height minus the body
    /// height) for the caller to write back into `TaskPeek::max_scroll`.
    pub fn draw_task_peek(
        &self,
        frame: &mut Frame,
        area: Rect,
        lines: &[PeekLine],
        scroll: u16,
    ) -> u16 {
        let width = (((area.width as u32) * 7 / 10) as u16)
            .max(40)
            .min(area.width.saturating_sub(2).max(1));
        let height = (((area.height as u32) * 6 / 10) as u16)
            .max(8)
            .min(area.height.saturating_sub(2).max(1));
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let dialog = Rect { x, y, width, height };

        frame.render_widget(Clear, dialog);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::TEXT))
            .title(" Info ");
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);

        let dim = Style::default().fg(theme::DIM);
        let text = Style::default().fg(theme::TEXT);
        let rendered: Vec<Line> = lines
            .iter()
            .map(|pl| match pl {
                PeekLine::Title(t) => Line::from(Span::styled(
                    t.clone(),
                    Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD),
                )),
                PeekLine::Field { label, value } => Line::from(vec![
                    Span::styled(format!("{}: ", label), dim),
                    Span::styled(value.clone(), text),
                ]),
                PeekLine::Status { value, status } => {
                    let color = match status {
                        TaskStatus::Running => theme::OK,
                        TaskStatus::Blocked => theme::ATTN,
                        TaskStatus::Done => theme::DIM,
                        TaskStatus::Backlog => theme::MUTED,
                    };
                    Line::from(vec![
                        Span::styled("Status: ", dim),
                        Span::styled(value.clone(), Style::default().fg(color)),
                    ])
                }
                PeekLine::Text(t) => Line::from(Span::styled(t.clone(), text)),
                PeekLine::Blank => Line::from(""),
            })
            .collect();

        // Body above, pinned footer hint below.
        let body = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: inner.height.saturating_sub(2),
        };

        // Estimated wrapped height (ceil of char width over body width per
        // line) → max scroll. Word wrap can occasionally break a line
        // earlier than the character count predicts, so this is a close
        // lower bound — good enough for a scroll clamp.
        let bw = body.width.max(1) as usize;
        let wrapped: usize = rendered
            .iter()
            .map(|l| {
                let w: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
                w.max(1).div_ceil(bw)
            })
            .sum();
        let max_scroll = wrapped
            .saturating_sub(body.height as usize)
            .min(u16::MAX as usize) as u16;

        frame.render_widget(
            Paragraph::new(rendered)
                .wrap(ratatui::widgets::Wrap { trim: false })
                .scroll((scroll.min(max_scroll), 0)),
            body,
        );
        if inner.height >= 2 {
            let footer = Rect {
                x: inner.x,
                y: inner.y + inner.height - 1,
                width: inner.width,
                height: 1,
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "j/k PgUp/PgDn scroll \u{00b7} Esc close",
                    dim,
                ))),
                footer,
            );
        }
        max_scroll
    }

    pub fn draw_snapshot_catalog(
        &self,
        frame: &mut Frame,
        area: Rect,
        snapshots: &[agent_memory::Snapshot],
        selected: usize,
        mode: &CatalogMode,
        is_picker: bool,
        status_msg: Option<&str>,
    ) {
        // Each sub-mode reuses the same outer dialog so transitions feel
        // in-place. Browse renders the list; Detail overlays the manifest
        // and head/tail; Rename overlays an inline editor; ConfirmDelete
        // overlays a y/n prompt.
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let width = area.width.min(78).max(40);
        let height = area.height.min(28).max(8);
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let dialog = Rect { x, y, width, height };

        frame.render_widget(Clear, dialog);

        let title = match (is_picker, mode) {
            (true, _) => " Pick Snapshot ",
            (false, CatalogMode::Detail { .. }) => " Snapshot Detail ",
            (false, CatalogMode::Rename { .. }) => " Rename Snapshot ",
            (false, CatalogMode::ConfirmDelete) => " Delete Snapshot ",
            _ => " Snapshots ",
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(title, Style::default().fg(theme::TEXT)));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);

        let dim = Style::default().fg(theme::DIM);
        let white = Style::default().fg(theme::TEXT);
        let cyan = Style::default().fg(theme::HEADER);
        let yellow = Style::default()
            .fg(theme::ATTN)
            .add_modifier(Modifier::BOLD);
        let red = Style::default().fg(theme::ERROR);

        if snapshots.is_empty() {
            let mut lines = vec![
                Line::from(""),
                Line::from(Span::styled(
                    "No snapshots saved yet.",
                    Style::default().fg(theme::MUTED),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Focus a claude / codex session and press A-b to save one.",
                    dim,
                )),
                Line::from(""),
            ];
            if let Some(msg) = status_msg {
                lines.push(Line::from(Span::styled(
                    sanitize_for_display(msg),
                    Style::default().fg(theme::ERROR),
                )));
                lines.push(Line::from(""));
            }
            lines.push(Line::from(Span::styled("Esc / A-z close", dim)));
            frame.render_widget(Paragraph::new(lines), inner);
            return;
        }

        // Reserve rows for the footer (blank + optional status + hint).
        // Cap the visible row count by the inner area so the selection
        // never scrolls off-screen and the footer is always visible —
        // a long list without windowing would silently let the user
        // rename / delete an invisible row.
        let footer_rows: u16 = 1 // blank separator
            + if status_msg.is_some() { 1 } else { 0 }
            + 1; // hint
        let visible_rows = inner
            .height
            .saturating_sub(footer_rows) as usize;
        let (row_start, row_end) =
            visible_range(selected, snapshots.len(), visible_rows);

        let mut lines: Vec<Line> = Vec::new();
        for (idx, snap) in snapshots[row_start..row_end].iter().enumerate() {
            let global_idx = row_start + idx;
            let is_active = global_idx == selected;
            let cursor = if is_active { "▸ " } else { "  " };
            let engine = match snap.manifest.engine {
                Engine::ClaudeCode => "claude-code",
                Engine::Codex => "codex",
            };
            let when =
                format_relative_time(snap.manifest.created_at_unix, now_secs);
            // Sanitize every snapshot-sourced string — manifests come from
            // disk and could include ANSI/OSC bytes that would otherwise
            // execute against the user's terminal on render.
            let safe_name = sanitize_for_display(&snap.name);
            let desc_first = sanitize_for_display(
                snap.manifest.description.lines().next().unwrap_or(""),
            );

            let name_style = if is_active { yellow } else { cyan };
            let meta_style = if is_active { white } else { dim };
            let mut spans = vec![
                Span::raw(cursor),
                Span::styled(format!("{safe_name:<24}"), name_style),
                Span::styled(format!("{engine:<13}"), meta_style),
                Span::styled(format!("{when:<10}"), meta_style),
            ];
            if !desc_first.is_empty() {
                spans.push(Span::styled(desc_first, meta_style));
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::from(""));
        if let Some(msg) = status_msg {
            lines.push(Line::from(Span::styled(
                sanitize_for_display(msg),
                Style::default().fg(theme::ERROR),
            )));
        }
        let total = snapshots.len();
        let hint = if is_picker {
            "j/k select \u{00b7} Enter pick \u{00b7} Esc / A-z cancel"
        } else {
            "j/k select \u{00b7} Enter detail \u{00b7} r rename \u{00b7} d delete \u{00b7} Esc / A-z close"
        };
        // Indicator like " 41/53 " when only a window is visible, so the
        // user can tell their selection is part of a longer list.
        let footer = if row_end - row_start < total {
            format!(
                "{hint}    [{}/{}]",
                selected.saturating_add(1),
                total,
            )
        } else {
            hint.to_string()
        };
        lines.push(Line::from(Span::styled(footer, dim)));

        frame.render_widget(Paragraph::new(lines), inner);

        // Sub-mode overlays.
        match mode {
            CatalogMode::Browse => {}
            CatalogMode::Detail { head, tail } => {
                if let Some(snap) = snapshots.get(selected) {
                    self.draw_snapshot_detail(frame, inner, snap, head, tail);
                }
            }
            CatalogMode::Rename { text, error } => {
                self.draw_snapshot_rename_overlay(
                    frame,
                    inner,
                    text,
                    error.as_deref(),
                );
            }
            CatalogMode::ConfirmDelete => {
                if let Some(snap) = snapshots.get(selected) {
                    self.draw_snapshot_delete_overlay(frame, inner, &snap.name);
                }
            }
        }
        let _ = (white, red);
    }

    fn draw_snapshot_detail(
        &self,
        frame: &mut Frame,
        area: Rect,
        snap: &agent_memory::Snapshot,
        head: &[String],
        tail: &[String],
    ) {
        let width = area.width.saturating_sub(2).min(74).max(30);
        let height = area.height.saturating_sub(2).min(22).max(6);
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let dialog = Rect { x, y, width, height };
        frame.render_widget(Clear, dialog);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(
                format!(" {} ", sanitize_for_display(&snap.name)),
                Style::default()
                    .fg(theme::TEXT)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);

        let dim = Style::default().fg(theme::DIM);
        let white = Style::default().fg(theme::TEXT);

        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let engine = match snap.manifest.engine {
            Engine::ClaudeCode => "claude-code",
            Engine::Codex => "codex",
        };

        let mut lines: Vec<Line> = Vec::new();
        // Every snapshot-sourced string is sanitized — manifests live on
        // disk and could carry ANSI/OSC byte sequences that would
        // otherwise be replayed into the user's terminal here.
        let kv = |k: &str, v: &str| {
            Line::from(vec![
                Span::styled(format!("{k:<16}"), dim),
                Span::styled(sanitize_for_display(v), white),
            ])
        };
        lines.push(kv("Engine:", engine));
        lines.push(kv(
            "Created:",
            &format_relative_time(snap.manifest.created_at_unix, now_secs),
        ));
        lines.push(kv(
            "Transcript:",
            &format!("{} bytes", snap.manifest.transcript_bytes),
        ));
        lines.push(kv(
            "Memory files:",
            &snap.manifest.memory_files.to_string(),
        ));
        lines.push(kv("Source UID:", &snap.manifest.source_session_uid));
        lines.push(kv(
            "Source transcript:",
            &snap.manifest.source_transcript_id,
        ));
        lines.push(kv(
            "Source cwd:",
            &snap.manifest.source_cwd.display().to_string(),
        ));
        if !snap.manifest.description.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("Description", dim)));
            for ln in snap.manifest.description.lines() {
                lines.push(Line::from(Span::styled(
                    sanitize_for_display(ln),
                    white,
                )));
            }
        }
        if !head.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("Transcript head", dim)));
            for ln in head {
                lines.push(Line::from(Span::styled(
                    truncate(&sanitize_for_display(ln), 72),
                    white,
                )));
            }
        }
        if !tail.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("Transcript tail", dim)));
            for ln in tail {
                lines.push(Line::from(Span::styled(
                    truncate(&sanitize_for_display(ln), 72),
                    white,
                )));
            }
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("Esc / Enter back", dim)));

        frame.render_widget(
            Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }),
            inner,
        );
    }

    fn draw_snapshot_rename_overlay(
        &self,
        frame: &mut Frame,
        area: Rect,
        text: &str,
        error: Option<&str>,
    ) {
        let width = area.width.min(60).max(30);
        let height = if error.is_some() { 9u16 } else { 7u16 };
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let dialog = Rect { x, y, width, height };
        frame.render_widget(Clear, dialog);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(" Rename ", Style::default().fg(theme::TEXT)));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);

        let lines = rename_overlay_lines(text, error);
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_snapshot_delete_overlay(
        &self,
        frame: &mut Frame,
        area: Rect,
        name: &str,
    ) {
        let width = area.width.min(60).max(30);
        let height = 5u16;
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let dialog = Rect { x, y, width, height };
        frame.render_widget(Clear, dialog);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::ATTN))
            .title(Span::styled(" Confirm ", Style::default().fg(theme::TEXT)));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);

        let dim = Style::default().fg(theme::DIM);
        let white = Style::default().fg(theme::TEXT);

        let lines = vec![
            Line::from(Span::styled(
                format!("Delete snapshot `{}`?", sanitize_for_display(name)),
                white,
            )),
            Line::from(""),
            Line::from(Span::styled("y / Enter confirm \u{00b7} n / Esc cancel", dim)),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
    }

    pub fn draw_workflow_launch(
        &self,
        frame: &mut Frame,
        area: Rect,
        ws_id: &str,
        workflow_name: &str,
        slots: &[WorkflowSlotChoice],
        active_slot: usize,
        goal: &str,
    ) {
        // Re-resolve the stable id each draw — workspaces may have reordered
        // since the modal opened (a frozen index would show the wrong name).
        let ws_index = resolve_workspace_by_id(&self.workspaces, ws_id)
            .unwrap_or(usize::MAX);
        let width = area.width.min(72).max(44);
        // +10 leaves room for the goal field row and the hint footer.
        let height = (slots.len() as u16 + 10).min(area.height.saturating_sub(2));
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let dialog = Rect { x, y, width, height };

        frame.render_widget(Clear, dialog);

        let title = format!(" Launch workflow: {} ", workflow_name);
        let ws_name = self
            .workspaces
            .get(ws_index)
            .map(|w| w.name.clone())
            .unwrap_or_default();

        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(Span::styled(
            format!("Workspace: {}", ws_name),
            Style::default().fg(theme::TEXT),
        )));
        lines.push(Line::from(""));
        for (idx, slot) in slots.iter().enumerate() {
            let is_active = idx == active_slot;
            let src_label = match slot.source() {
                WorkflowSlotSource::Existing(si) => {
                    let label = self
                        .workspaces
                        .get(ws_index)
                        .and_then(|w| w.sessions.get(*si))
                        .map(|s| s.label.clone())
                        .unwrap_or_else(|| "?".into());
                    format!("existing ({})", label)
                }
                WorkflowSlotSource::New(Engine::ClaudeCode) => "new claude".into(),
                WorkflowSlotSource::New(Engine::Codex) => "new codex".into(),
            };
            let cursor = if is_active { "▸ " } else { "  " };
            let role_style = if is_active {
                Style::default().fg(theme::ATTN).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::HEADER)
            };
            let value_style = if is_active {
                Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::MUTED)
            };
            let decorator = if is_active && slot.options.len() > 1 {
                format!("◂ {} ▸", src_label)
            } else {
                src_label.clone()
            };
            lines.push(Line::from(vec![
                Span::raw(cursor),
                Span::styled(format!("{:<10}", slot.role), role_style),
                Span::styled(decorator, value_style),
            ]));
        }
        lines.push(Line::from(""));
        // Goal field (optional). Focused when `active_slot == slots.len()`.
        let goal_focused = active_slot == slots.len();
        let goal_cursor = if goal_focused { "▸ " } else { "  " };
        let goal_label_style = if goal_focused {
            Style::default().fg(theme::ATTN).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::HEADER)
        };
        let goal_value_style = if goal_focused {
            Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::MUTED)
        };
        let goal_display: String = if goal.is_empty() {
            "(optional — overrides {{ goal }})".into()
        } else if goal_focused {
            format!("{}\u{258f}", goal)
        } else {
            goal.to_string()
        };
        lines.push(Line::from(vec![
            Span::raw(goal_cursor),
            Span::styled(format!("{:<10}", "goal"), goal_label_style),
            Span::styled(goal_display, goal_value_style),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "\u{2191}\u{2193} field   \u{2190}\u{2192} choice   Enter: launch   Esc: cancel",
            Style::default().fg(theme::DIM),
        )));

        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .style(Style::default().fg(theme::TEXT));
        let paragraph = Paragraph::new(lines).block(block);
        frame.render_widget(paragraph, dialog);
    }

    pub fn draw_workflow_history(&self, frame: &mut Frame, area: Rect, run_id: &str) {
        let width = area.width.saturating_sub(4).min(90);
        let height = area.height.saturating_sub(4);
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let dialog = Rect { x, y, width, height };
        frame.render_widget(Clear, dialog);

        let run = self.workflow_runs.iter().find(|r| r.run_id == run_id);
        let mut lines: Vec<Line> = Vec::new();
        if let Some(run) = run {
            lines.push(Line::from(Span::styled(
                format!(
                    "{} • iter {} • status: {:?}",
                    run.workflow_name, run.iteration, run.status
                ),
                Style::default().fg(theme::TEXT),
            )));
            lines.push(Line::from(""));
            for h in &run.history {
                let msg = h
                    .last_message
                    .as_deref()
                    .map(|s| {
                        let first = s.lines().next().unwrap_or("");
                        let trimmed: String = first.chars().take(80).collect();
                        trimmed
                    })
                    .unwrap_or("(active)".into());
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("[{:>3}] {:<10}", h.iteration, h.role),
                        Style::default().fg(theme::HEADER),
                    ),
                    Span::raw("  "),
                    Span::styled(msg, Style::default().fg(theme::MUTED)),
                ]));
            }
            if let Some(reason) = &run.done_reason {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    format!("done: {}", reason),
                    Style::default().fg(theme::OK),
                )));
            }
        } else {
            lines.push(Line::from("(run not found)"));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Esc / Enter: close",
            Style::default().fg(theme::DIM),
        )));
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" Workflow history • {} ", run_id))
            .style(Style::default().fg(theme::TEXT));
        let paragraph = Paragraph::new(lines).block(block);
        frame.render_widget(paragraph, dialog);
    }
}

#[cfg(test)]
mod operator_question_tests {
    use super::*;

    #[test]
    fn extract_operator_question_reads_present_trims_and_ignores_blanks() {
        // present → Some, trimmed.
        assert_eq!(
            App::extract_operator_question(Some(&serde_json::json!({
                "operator_question": "  refill OpenAI billing  "
            }))),
            Some("refill OpenAI billing".to_string())
        );
        // empty / whitespace-only → None (a blank must never light a
        // content-less ◉).
        assert_eq!(
            App::extract_operator_question(Some(&serde_json::json!({"operator_question": "   "}))),
            None
        );
        assert_eq!(
            App::extract_operator_question(Some(&serde_json::json!({"operator_question": ""}))),
            None
        );
        // missing key / non-string value / absent metadata → None.
        assert_eq!(
            App::extract_operator_question(Some(&serde_json::json!({"other": "x"}))),
            None
        );
        assert_eq!(
            App::extract_operator_question(Some(&serde_json::json!({"operator_question": 42}))),
            None
        );
        assert_eq!(App::extract_operator_question(None), None);
    }
}

#[cfg(test)]
mod dispatch_pending_filter_tests {
    //! Planning-liveness half of the Continuous panel's dispatch-pending
    //! (○) indicator: `issue_awaits_dispatch`. The parse half (cleared
    //! blocked_reason + dated OPERATOR directive + no ack) is tested
    //! daemon-side in `cm_daemon::continuous::dispatch_pending`.
    use super::*;
    use cm_daemon::continuous::dispatch_pending::PendingIssue;

    fn issue(subtask: Option<&str>) -> PendingIssue {
        PendingIssue {
            issue_id: "PERF-083".into(),
            title: None,
            directive_date: "2026-07-18".into(),
            subtask_task_id: subtask.map(String::from),
        }
    }

    fn task(task_id: &str, status: TaskStatus) -> TaskEntry {
        TaskEntry {
            task_id: Some(task_id.into()),
            name: task_id.into(),
            api_status: status,
            repo_url: None,
            prompt: None,
            wip_branch: None,
            session_id: None,
            blocked_at: None,
            is_cloud: false,
            is_continuous: false,
            workspace_id: None,
            project: None,
            parent_task_id: None,
            worktree_mode: WorktreeMode::Inherit,
            metadata: None,
        }
    }

    #[test]
    fn no_subtask_awaits_dispatch() {
        // The PERF-083 exemplar: directive written, nothing spawned yet.
        assert!(App::issue_awaits_dispatch(&issue(None), &[]));
    }

    #[test]
    fn live_subtask_means_already_dispatched() {
        // The PERF-088 exemplar: OPERATOR comment present, but its subtask
        // maps to a running planning task → the orchestrator already acted.
        for status in [TaskStatus::Running, TaskStatus::Blocked, TaskStatus::Backlog] {
            let tasks = vec![task("94e3b1aa", status.clone())];
            assert!(
                !App::issue_awaits_dispatch(&issue(Some("94e3b1aa")), &tasks),
                "{:?} subtask must suppress the indicator",
                status
            );
        }
    }

    #[test]
    fn done_or_unknown_subtask_still_awaits() {
        // A finished (or archived-away) subtask doesn't clear a directive —
        // only the orchestrator's ack or a fresh dispatch does.
        let done = vec![task("94e3b1aa", TaskStatus::Done)];
        assert!(App::issue_awaits_dispatch(&issue(Some("94e3b1aa")), &done));
        let unrelated = vec![task("other-task", TaskStatus::Running)];
        assert!(App::issue_awaits_dispatch(&issue(Some("94e3b1aa")), &unrelated));
        assert!(App::issue_awaits_dispatch(&issue(Some("94e3b1aa")), &[]));
    }
}
