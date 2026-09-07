//! Composer mentions retain stable identities only while their visible text survives.
use super::*;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Mention {
    start: usize,
    end: usize,
    text: String,
    // None is the explicit channel broadcast, never an inferred body token.
    participant: Option<String>,
}
impl Mention {
    fn matches(&self, body: &str) -> bool {
        body.get(self.start..self.end) == Some(self.text.as_str())
            && body.get(..self.start).is_some_and(|s| {
                s.chars()
                    .next_back()
                    .is_none_or(|c| c.is_whitespace() || "([{,:".contains(c))
            })
            && body.get(self.end..).is_some_and(|s| {
                s.chars()
                    .next()
                    .is_none_or(|c| c.is_whitespace() || ".,:;!?()[]{}*/\"'".contains(c))
            })
    }
}
impl Draft {
    pub(super) fn replace_text(&mut self, range: std::ops::Range<usize>, text: &str) {
        let delta = text.len() as isize - (range.end - range.start) as isize;
        self.entities.retain_mut(|m| {
            if range.end <= m.start {
                m.start = m.start.saturating_add_signed(delta);
                m.end = m.end.saturating_add_signed(delta);
                true
            } else {
                range.start >= m.end
            }
        });
        self.body.replace_range(range.clone(), text);
        self.cursor = range.start + text.len();
        self.entities.retain(|m| m.matches(&self.body));
    }
    fn valid_mentions(&self) -> impl Iterator<Item = &Mention> {
        self.entities.iter().filter(|m| m.matches(&self.body))
    }
    pub(super) fn mention_payload(&self) -> (Vec<String>, bool) {
        let mut ids = self.mentions.clone();
        let mut here = false;
        for m in self.valid_mentions() {
            if let Some(id) = &m.participant {
                if !ids.contains(id) {
                    ids.push(id.clone());
                }
            } else {
                here = true;
            }
        }
        (ids, here)
    }
}
impl Messages {
    fn mention_query(&self) -> Option<(usize, String)> {
        if self.mode != "compose" || self.mention_dismissed {
            return None;
        }
        let draft = self.draft();
        if !draft.request_id.is_empty() {
            return None;
        }
        let cursor = draft.cursor.min(draft.body.len());
        let before = draft.body.get(..cursor)?;
        let start = before.rfind('@')?;
        if before[..start]
            .chars()
            .next_back()
            .is_some_and(|c| !c.is_whitespace() && !"([{,:".contains(c))
            || draft
                .valid_mentions()
                .any(|m| start >= m.start && start < m.end)
        {
            return None;
        }
        let query = &before[start + 1..];
        if query.contains('\n') || query.chars().count() > 40 {
            return None;
        }
        Some((start, query.to_lowercase()))
    }
    pub(super) fn mention_options(&self) -> Vec<(Option<String>, String)> {
        let Some((_, query)) = self.mention_query() else {
            return vec![];
        };
        let channel = self.current_channel().is_some()
            || self.target["channel"].as_str().is_some_and(|p| p != "*");
        let dm = self
            .dms
            .iter()
            .find(|d| d["id"] == self.target["conversation"]);
        let allowed = |id: &str| {
            channel
                || id == "owner"
                || self.target["dm"] == id
                || self.target["dm"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|p| p == id))
                || dm.is_some_and(|d| {
                    d["members"]
                        .as_array()
                        .is_some_and(|a| a.iter().any(|p| p == id))
                })
        };
        let mut candidates: Vec<_> = self
            .people
            .iter()
            .filter_map(|p| {
                let id = p["id"].as_str()?;
                let name = p["name"].as_str().filter(|n| !n.is_empty()).unwrap_or(id);
                (allowed(id)
                    && (name.to_lowercase().contains(&query) || id.to_lowercase().contains(&query)))
                .then(|| (Some(id.to_owned()), name.to_owned()))
            })
            .collect();
        if allowed("owner")
            && "owner".contains(&query)
            && !candidates
                .iter()
                .any(|(id, _)| id.as_deref() == Some("owner"))
        {
            candidates.push((Some("owner".into()), "Owner".into()));
        }
        candidates.sort_by(|a, b| {
            a.1.to_lowercase()
                .cmp(&b.1.to_lowercase())
                .then_with(|| a.0.cmp(&b.0))
        });
        if channel && "here".starts_with(&query) {
            candidates.insert(0, (None, "here".into()));
        }
        candidates
    }
    pub(super) fn mention_key(&mut self, key: &crossterm::event::KeyEvent) -> bool {
        let options = self.mention_options();
        if options.is_empty() {
            return false;
        }
        match key.code {
            KeyCode::Up => self.mention_selected = self.mention_selected.saturating_sub(1),
            KeyCode::Down => {
                self.mention_selected = (self.mention_selected + 1).min(options.len() - 1)
            }
            KeyCode::Esc => self.mention_dismissed = true,
            KeyCode::Enter | KeyCode::Tab => {
                let (participant, label) =
                    options[self.mention_selected.min(options.len() - 1)].clone();
                let (start, _) = self.mention_query().unwrap();
                let mut draft = self.draft();
                let text = format!("@{label}");
                draft.replace_text(start..draft.cursor, &format!("{text} "));
                draft.entities.push(Mention {
                    start,
                    end: start + text.len(),
                    text,
                    participant,
                });
                self.set_draft(draft);
                self.mention_selected = 0;
            }
            _ => return false,
        }
        true
    }
}
impl App {
    pub(super) fn draw_mention_options(&self, frame: &mut Frame, area: Rect) {
        let options = self.messages.mention_options();
        if options.is_empty() || area.height == 0 {
            return;
        }
        let visible = area.height.saturating_sub(2) as usize;
        let selected = self.messages.mention_selected.min(options.len() - 1);
        let start = selected.saturating_sub(visible.saturating_sub(1));
        let lines: Vec<_> = options
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
            .map(|(i, (id, label))| {
                let detail = if id.is_none() {
                    "all joined channel members".into()
                } else if id.as_deref() == Some("owner") {
                    "you".into()
                } else {
                    let person = self
                        .messages
                        .people
                        .iter()
                        .find(|p| p["id"].as_str() == id.as_deref());
                    let presence = if person.is_some_and(|p| p["present"] == true) {
                        "active"
                    } else {
                        "not running"
                    };
                    if options.iter().filter(|(_, name)| name == label).count() > 1 {
                        let uid = person.and_then(|p| p["session_uid"].as_str()).unwrap_or("");
                        format!(
                            "{presence} · session …{}",
                            uid.chars()
                                .rev()
                                .take(8)
                                .collect::<String>()
                                .chars()
                                .rev()
                                .collect::<String>()
                        )
                    } else {
                        presence.into()
                    }
                };
                Line::styled(
                    format!(
                        "{} @{label} · {detail}",
                        if i == selected { "›" } else { " " }
                    ),
                    Style::default().fg(theme::CHAT_TAG).bg(if i == selected {
                        theme::CHAT_SELECTION
                    } else {
                        theme::CHAT_PANEL
                    }),
                )
            })
            .collect();
        frame.render_widget(
            Paragraph::new(lines)
                .style(Style::default().bg(theme::CHAT_PANEL))
                .block(chat_block("Mention · ↑/↓ choose · Enter selects", true)),
            area,
        );
        if visible > 0 {
            frame.buffer_mut().set_style(
                Rect::new(
                    area.x + 1,
                    area.y + 1 + (selected - start) as u16,
                    area.width.saturating_sub(2),
                    1,
                ),
                Style::default().bg(theme::CHAT_SELECTION),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn visible_mentions_follow_unicode_edits_and_disappear_when_changed() {
        let mut d = Draft {
            body: "Hi @Émile ok".into(),
            entities: vec![Mention {
                start: 3,
                end: 10,
                text: "@Émile".into(),
                participant: Some("stable-id".into()),
            }],
            ..Draft::default()
        };
        assert_eq!(d.mention_payload().0, ["stable-id"]);
        d.replace_text(0..0, "👋 ");
        assert_eq!(d.mention_payload().0, ["stable-id"]);
        let end = d.body.len();
        d.replace_text(end..end, "!");
        assert_eq!(d.mention_payload().0, ["stable-id"]);
        d.replace_text(9..11, "E");
        assert!(d.mention_payload().0.is_empty());
        let restored: Draft = serde_json::from_value(serde_json::to_value(&d).unwrap()).unwrap();
        assert!(restored.mention_payload().0.is_empty());
    }
    #[test]
    fn adjoining_text_cannot_keep_or_resurrect_a_mention() {
        let original = Draft {
            body: "@Alpha ok".into(),
            entities: vec![Mention {
                start: 0,
                end: 6,
                text: "@Alpha".into(),
                participant: Some("alpha".into()),
            }],
            ..Draft::default()
        };
        for (range, text) in [(6..6, "Z"), (0..0, "X"), (6..7, "")] {
            let mut d = original.clone();
            d.replace_text(range, text);
            assert!(d.mention_payload().0.is_empty());
            assert!(d.entities.is_empty());
        }
        let mut d = original;
        d.replace_text(6..6, "!");
        assert_eq!(d.mention_payload().0, ["alpha"]);
    }
    #[test]
    fn completions_filter_dm_members_and_freeze_selected_ids() {
        let _lock = crate::test_support::home_lock();
        let _home = super::super::tests::Home::new();
        let mut m = Messages {
            mode: "compose".into(),
            target: json!({"channel":"general"}),
            people: vec![
                json!({"id":"alpha","name":"Alpha Builder"}),
                json!({"id":"beta","name":"Beta Reader"}),
            ],
            ..Messages::default()
        };
        m.set_draft(Draft {
            body: "@Al".into(),
            cursor: 3,
            ..Draft::default()
        });
        assert_eq!(
            m.mention_options(),
            vec![(Some("alpha".into()), "Alpha Builder".into())]
        );
        assert!(m.mention_key(&crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE
        )));
        assert_eq!(m.draft().body, "@Alpha Builder ");
        assert_eq!(m.draft().mention_payload().0, ["alpha"]);
        m.people[0]["name"] = json!("Renamed");
        assert_eq!(m.draft().mention_payload().0, ["alpha"]);
        m.set_draft(Draft {
            body: "@here".into(),
            cursor: 5,
            ..Draft::default()
        });
        assert!(!m.draft().mention_payload().1); // typing alone is not a broadcast
        m.mention_key(&crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ));
        assert!(m.draft().mention_payload().1);
        m.target = json!({"dm":"beta"});
        m.set_draft(Draft {
            body: "@".into(),
            cursor: 1,
            ..Draft::default()
        });
        assert!(m
            .mention_options()
            .iter()
            .all(|(id, _)| matches!(id.as_deref(), Some("beta" | "owner"))));
    }
}
