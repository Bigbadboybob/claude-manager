//! Durable task lifecycle, deliberately independent of agent activity/idle time.
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContinuousStage {
    Queued,
    Investigating,
    Implementing,
    ReviewQueued,
    Reviewing,
    OwnerReview,
    Deploying,
    Monitoring,
    Waiting,
    Done,
    Unknown,
}

impl ContinuousStage {
    pub fn resolve(metadata: Option<&Value>, done: bool, blocked: bool) -> Self {
        if done {
            return Self::Done;
        }
        let Some(m) = metadata else {
            return if blocked {
                Self::OwnerReview
            } else {
                Self::Unknown
            };
        };
        // An explicit stage wins over the old overloaded planning `blocked` flag.
        if let Some(stage) = m.get("continuous_stage") {
            return stage
                .as_str()
                .and_then(Self::parse)
                .filter(|s| *s != Self::Done)
                .unwrap_or(Self::Unknown);
        }
        let owner_question = m
            .get("operator_question")
            .and_then(Value::as_str)
            .is_some_and(|q| !q.trim().is_empty());
        if owner_question {
            return Self::OwnerReview;
        }
        let legacy = m.get("stage").and_then(Value::as_str).unwrap_or("");
        // Older `review` rows used blocked to mean Owner review. New writers
        // must distinguish review_queued, reviewing and owner_review explicitly.
        if blocked && matches!(legacy, "review" | "fix_ready" | "") {
            return Self::OwnerReview;
        }
        Self::parse(legacy)
            .filter(|s| *s != Self::Done)
            .unwrap_or(if blocked {
                Self::OwnerReview
            } else {
                Self::Unknown
            })
    }

    fn parse(stage: &str) -> Option<Self> {
        Some(match stage.trim() {
            "queued" | "admitted" | "backlog" | "dispatch_pending" => Self::Queued,
            "investigating" | "investigate" | "propose" | "proposal" => Self::Investigating,
            "implementing" | "implement" | "correction" | "fixing" => Self::Implementing,
            "review_queued" | "orchestrator_review" | "review" => Self::ReviewQueued,
            "reviewing" => Self::Reviewing,
            "owner_review" | "needs_owner" | "fix_ready" | "deployment_gate" => Self::OwnerReview,
            "deploying" | "merge" | "merged" | "deploy" => Self::Deploying,
            "monitoring" | "monitor" | "verifying" | "verify" => Self::Monitoring,
            "waiting" | "blocked" | "waiting_external" => Self::Waiting,
            "done" | "resolved" | "completed" => Self::Done,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "Queue",
            Self::Investigating => "Investigate",
            Self::Implementing => "Build",
            Self::ReviewQueued => "Review Q",
            Self::Reviewing => "Reviewing",
            Self::OwnerReview => "Needs you",
            Self::Deploying => "Deploy",
            Self::Monitoring => "Verify",
            Self::Waiting => "Waiting",
            Self::Done => "Done",
            Self::Unknown => "Unstaged",
        }
    }

    pub fn color(self) -> Color {
        match self {
            Self::Queued => Color::Gray,
            Self::Investigating => Color::LightBlue,
            Self::Implementing => Color::Cyan,
            Self::ReviewQueued => Color::Yellow,
            Self::Reviewing => Color::Rgb(255, 165, 70),
            Self::OwnerReview => Color::LightMagenta,
            Self::Deploying => Color::Rgb(165, 140, 255),
            Self::Monitoring => Color::Rgb(80, 210, 170),
            Self::Waiting => Color::LightRed,
            Self::Done => Color::LightGreen,
            Self::Unknown => Color::DarkGray,
        }
    }

    pub fn badge(self) -> Span<'static> {
        Span::styled(
            format!("[{}] ", self.label()),
            Style::default().fg(self.color()),
        )
    }
}

pub fn legend() -> Vec<Line<'static>> {
    use ContinuousStage::*;
    let mut lines = vec![Line::from("Stages (separate from activity)")];
    for stages in [
        &[Queued, Investigating, Implementing][..],
        &[ReviewQueued, Reviewing],
        &[OwnerReview, Deploying, Monitoring],
        &[Waiting, Done, Unknown],
    ] {
        let mut spans = Vec::new();
        for stage in stages {
            spans.push(Span::styled(
                format!("{}  ", stage.label()),
                Style::default().fg(stage.color()),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from("Review Q / Reviewing: orchestrator"));
    lines.push(Line::from("Idle does not mean approved"));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn continuous_stage_distinguishes_review_ownership_from_activity() {
        for (value, expected) in [
            ("review_queued", ContinuousStage::ReviewQueued),
            ("reviewing", ContinuousStage::Reviewing),
            ("owner_review", ContinuousStage::OwnerReview),
        ] {
            let meta = json!({"continuous_stage":value, "stage":"review"});
            // A stale raw blocked flag cannot turn an internal review into an Owner request.
            assert_eq!(ContinuousStage::resolve(Some(&meta), false, true), expected);
            assert_eq!(
                ContinuousStage::resolve(Some(&meta), false, false),
                expected
            );
        }
    }

    #[test]
    fn continuous_stage_handles_legacy_missing_and_terminal_metadata() {
        assert_eq!(
            ContinuousStage::resolve(None, false, false),
            ContinuousStage::Unknown
        );
        assert_eq!(
            ContinuousStage::resolve(None, false, true),
            ContinuousStage::OwnerReview
        );
        assert_eq!(
            ContinuousStage::resolve(Some(&json!({"stage":"review"})), false, false),
            ContinuousStage::ReviewQueued
        );
        assert_eq!(
            ContinuousStage::resolve(Some(&json!({"stage":"review"})), false, true),
            ContinuousStage::OwnerReview
        );
        assert_eq!(
            ContinuousStage::resolve(Some(&json!({"continuous_stage":"reviewing"})), true, false),
            ContinuousStage::Done
        );
        assert_eq!(
            ContinuousStage::resolve(
                Some(&json!({"continuous_stage":"future-stage"})),
                false,
                true
            ),
            ContinuousStage::Unknown
        );
    }

    #[test]
    fn continuous_stage_never_calls_reopened_or_malformed_tasks_complete() {
        for meta in [
            json!({"continuous_stage":"done"}),
            json!({"stage":"completed"}),
            json!({"continuous_stage":42, "stage":"owner_review"}),
        ] {
            assert_eq!(
                ContinuousStage::resolve(Some(&meta), false, false),
                ContinuousStage::Unknown
            );
        }
    }

    #[test]
    fn continuous_stage_legend_fits_default_column_and_preserves_colors() {
        use ratatui::{backend::TestBackend, widgets::Paragraph, Terminal};
        let mut terminal = Terminal::new(TestBackend::new(34, 7)).unwrap();
        terminal
            .draw(|f| f.render_widget(Paragraph::new(legend()), f.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = buffer.content().iter().map(|c| c.symbol()).collect();
        assert!(screen.contains("Review Q  Reviewing"));
        assert!(screen.contains("Needs you  Deploy  Verify"));
        assert!(screen.contains("Idle does not mean approved"));
        assert_eq!(buffer[(0, 2)].fg, ContinuousStage::ReviewQueued.color());
        assert_eq!(buffer[(10, 2)].fg, ContinuousStage::Reviewing.color());
    }
}
