//! Type-to-filter task picker shared by the A-F fork form and the
//! "Reports to" field on the A-n / A-f launch forms.
//!
//! Prefix matches on the task name rank first, then substring matches (or a
//! task-id prefix), each in the caller's task order.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::theme;

/// A task the user can pick (from the viewer's task list).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TaskChoice {
    pub task_id: String,
    pub name: String,
}

/// Where a reporting line points: no parent, or a chosen task.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) enum ReportsTo {
    #[default]
    Standalone,
    Task(TaskChoice),
}

impl ReportsTo {
    pub(crate) fn parent_task_id(&self) -> Option<&str> {
        match self {
            ReportsTo::Standalone => None,
            ReportsTo::Task(t) => Some(&t.task_id),
        }
    }

    /// The form row value.
    pub(crate) fn label(&self) -> String {
        match self {
            ReportsTo::Standalone => "Standalone".to_string(),
            ReportsTo::Task(t) => format!("{}  {}", display(&t.name), short_id(&t.task_id)),
        }
    }
}

/// Indices of `tasks` matching `query`, ranked as the module doc says.
pub(crate) fn filter(tasks: &[TaskChoice], query: &str) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    let mut prefix = Vec::new();
    let mut substr = Vec::new();
    for (i, t) in tasks.iter().enumerate() {
        let name = t.name.to_lowercase();
        if q.is_empty() || name.starts_with(&q) {
            prefix.push(i);
        } else if name.contains(&q) || t.task_id.starts_with(&q) {
            substr.push(i);
        }
    }
    prefix.extend(substr);
    prefix
}

/// What a key did to an open picker.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PickerKey {
    /// Still open (query or selection changed, or the key was ignored).
    Open,
    Picked(TaskChoice),
    /// Esc: closed without a choice.
    Cancelled,
}

/// A picker that owns its candidate list (the launch forms' use).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TaskPicker {
    pub title: &'static str,
    pub tasks: Vec<TaskChoice>,
    pub query: String,
    pub selected: usize,
}

impl TaskPicker {
    pub(crate) fn new(title: &'static str, tasks: Vec<TaskChoice>, query: String) -> Self {
        Self { title, tasks, query, selected: 0 }
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> PickerKey {
        match step(&self.tasks, &mut self.query, &mut self.selected, key) {
            Some(Some(i)) => PickerKey::Picked(self.tasks[i].clone()),
            Some(None) => PickerKey::Cancelled,
            None => PickerKey::Open,
        }
    }

    pub(crate) fn draw(&self, frame: &mut Frame, over: Rect) {
        draw(frame, over, self.title, &self.tasks, &self.query, self.selected);
    }
}

/// Apply one key to a picker's query/selection. `Some(Some(i))` picked
/// `tasks[i]`, `Some(None)` closed it (Esc, or Enter with no match),
/// `None` keeps it open.
pub(crate) fn step(
    tasks: &[TaskChoice],
    query: &mut String,
    selected: &mut usize,
    key: KeyEvent,
) -> Option<Option<usize>> {
    let plain = !key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL);
    let matches = filter(tasks, query);
    match key.code {
        KeyCode::Esc => return Some(None),
        KeyCode::Up | KeyCode::BackTab => *selected = selected.saturating_sub(1),
        KeyCode::Down | KeyCode::Tab => {
            *selected = (*selected + 1).min(matches.len().saturating_sub(1));
        }
        KeyCode::Enter => return Some(matches.get(*selected).copied()),
        KeyCode::Backspace => {
            query.pop();
            *selected = 0;
        }
        KeyCode::Char(c) if plain => {
            query.push(c);
            *selected = 0;
        }
        _ => {}
    }
    None
}

/// Draw the picker overlay inside `over`.
pub(crate) fn draw(
    frame: &mut Frame,
    over: Rect,
    title: &str,
    tasks: &[TaskChoice],
    query: &str,
    selected: usize,
) {
    let matches = filter(tasks, query);
    let height = (matches.len() as u16 + 4).clamp(6, 16).min(over.height);
    let area = Rect::new(over.x + 2, over.y + 3, over.width.saturating_sub(4), height);
    frame.render_widget(Clear, area);
    let block = Block::default().borders(Borders::ALL).title(title.to_string());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let dim = Style::default().fg(theme::DIM);
    let mut lines = vec![Line::from(vec![
        Span::styled("/ ", dim),
        Span::styled(display(query), Style::default().fg(theme::TEXT)),
        Span::styled("\u{2588}", Style::default().fg(theme::TEXT)),
    ])];
    let rows = inner.height.saturating_sub(1) as usize;
    let start = selected.saturating_sub(rows.saturating_sub(1));
    if matches.is_empty() {
        lines.push(Line::from(Span::styled("no matching tasks", dim)));
    }
    for (pos, &i) in matches.iter().enumerate().skip(start).take(rows) {
        let style = if pos == selected {
            Style::default().fg(theme::TEXT).add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(theme::TEXT)
        };
        lines.push(Line::from(Span::styled(
            format!("{}  {}", display(&tasks[i].name), short_id(&tasks[i].task_id)),
            style,
        )));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Strip control characters (task names are agent-authored).
fn display(s: &str) -> String {
    s.chars().filter(|c| *c == '\t' || !c.is_control()).collect()
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: &str, name: &str) -> TaskChoice {
        TaskChoice { task_id: id.into(), name: name.into() }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn picker_filters_moves_picks_and_cancels() {
        let tasks = vec![t("aaa1", "Execution policy"), t("bbb2", "Trader exec"), t("ccc3", "Docs")];
        assert_eq!(filter(&tasks, "exec"), vec![0, 1]);
        assert_eq!(filter(&tasks, "trader"), vec![1]);
        assert_eq!(filter(&tasks, "ccc"), vec![2], "task-id prefix matches");
        let mut p = TaskPicker::new(" Reports to ", tasks, String::new());
        for c in "ex".chars() {
            assert_eq!(p.key(key(KeyCode::Char(c))), PickerKey::Open);
        }
        p.key(key(KeyCode::Down));
        assert_eq!(p.key(key(KeyCode::Enter)), PickerKey::Picked(t("bbb2", "Trader exec")));
        assert_eq!(p.key(key(KeyCode::Esc)), PickerKey::Cancelled);
        p.query = "zzz".into();
        assert_eq!(p.key(key(KeyCode::Enter)), PickerKey::Cancelled, "no match closes");
    }

    #[test]
    fn reports_to_labels_and_parent() {
        assert_eq!(ReportsTo::Standalone.parent_task_id(), None);
        assert_eq!(ReportsTo::Standalone.label(), "Standalone");
        let r = ReportsTo::Task(t("0123456789", "CTO"));
        assert_eq!(r.parent_task_id(), Some("0123456789"));
        assert_eq!(r.label(), "CTO  01234567");
    }
}
