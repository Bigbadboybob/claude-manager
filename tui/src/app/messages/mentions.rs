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
/// What a send notifies: picked mentions, plus what the body types itself
/// (Owner 2026-10-22: a typed `@here` or exact `@Name` used to go out as
/// plain text).
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Audience {
    pub ids: Vec<String>,
    pub here: bool,
    /// Typed names that match more than one person: not notified.
    pub ambiguous: Vec<String>,
}

/// Byte ranges of the body inside backtick code spans (inline or fenced).
fn code_ranges(body: &str) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut open: Option<(usize, usize)> = None; // (start, run length)
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let run = bytes[i..].iter().take_while(|b| **b == b'`').count();
            match open {
                Some((start, len)) if len == run => {
                    out.push(start..i + run);
                    open = None;
                }
                None => open = Some((i, run)),
                _ => {}
            }
            i += run;
        } else {
            i += 1;
        }
    }
    if let Some((start, _)) = open {
        out.push(start..body.len()); // an unclosed span runs to the end
    }
    out
}

/// Mentions the body types itself: `@here` (channels only) and `@Name` for a
/// name or id in `pool` (the people this conversation's popup offers),
/// matched case-insensitively, longest first, so names with spaces work. A
/// token counts only where a picked mention could stand: at the start or
/// after whitespace or an opening bracket, ending at whitespace or
/// punctuation, outside code spans. URLs and emails (`a@b`, `x/@y`) don't
/// qualify. A name more than one person shares is ambiguous, not notified.
pub(super) fn typed_mentions(body: &str, pool: &[(String, String)], channel: bool) -> Audience {
    let code = code_ranges(body);
    let starts_ok = |at: usize| {
        body[..at].chars().next_back().is_none_or(|c| c.is_whitespace() || "([{,:".contains(c))
    };
    let ends_ok = |at: usize| {
        body[at..].chars().next().is_none_or(|c| c.is_whitespace() || ".,:;!?()[]{}*/\"'".contains(c))
    };
    // Candidate labels: "here", then every name and id, longest first.
    let mut labels: Vec<(&str, Option<&str>)> = Vec::new();
    if channel {
        labels.push(("here", None));
    }
    for (id, name) in pool {
        labels.push((name.as_str(), Some(id.as_str())));
        labels.push((id.as_str(), Some(id.as_str())));
    }
    labels.retain(|(l, _)| !l.is_empty());
    labels.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
    let same = |a: &str, b: &str| a.to_lowercase() == b.to_lowercase();
    let mut out = Audience::default();
    for (at, _) in body.match_indices('@') {
        if !starts_ok(at) || code.iter().any(|r| r.contains(&at)) {
            continue;
        }
        let from = at + 1;
        // The longest label the body spells here, ending at a boundary.
        let Some(word) = labels.iter().find_map(|(l, _)| {
            let typed = body.get(from..from + l.len())?;
            (same(typed, l) && ends_ok(from + l.len())).then_some(typed)
        }) else {
            continue;
        };
        let mut who: Vec<Option<&str>> =
            labels.iter().filter(|(l, _)| same(l, word)).map(|(_, id)| *id).collect();
        who.sort();
        who.dedup();
        match who.as_slice() {
            [None] => out.here = true,
            [Some(id)] => {
                if !out.ids.iter().any(|x| x == id) {
                    out.ids.push((*id).to_owned());
                }
            }
            _ => {
                let typed = format!("@{word}");
                if !out.ambiguous.contains(&typed) {
                    out.ambiguous.push(typed);
                }
            }
        }
    }
    out
}

impl Messages {
    /// Whether the composer targets a channel (where `@here` exists).
    fn composing_in_channel(&self) -> bool {
        self.current_channel().is_some() || self.target["channel"].as_str().is_some_and(|p| p != "*")
    }

    /// The people a mention in this conversation may name: `(id, name)`,
    /// the same pool the popup offers.
    fn mention_pool(&self) -> Vec<(String, String)> {
        let channel = self.composing_in_channel();
        let dm = self.dms.iter().find(|d| d["id"] == self.target["conversation"]);
        let allowed = |id: &str| {
            channel
                || id == "owner"
                || self.target["dm"] == id
                || self.target["dm"].as_array().is_some_and(|a| a.iter().any(|p| p == id))
                || dm.is_some_and(|d| d["members"].as_array().is_some_and(|a| a.iter().any(|p| p == id)))
        };
        let mut pool: Vec<(String, String)> = self
            .people
            .iter()
            .filter_map(|p| {
                let id = p["id"].as_str()?;
                let name = p["name"].as_str().filter(|n| !n.is_empty()).unwrap_or(id);
                allowed(id).then(|| (id.to_owned(), name.to_owned()))
            })
            .collect();
        if !pool.iter().any(|(id, _)| id == "owner") {
            pool.push(("owner".into(), "Owner".into()));
        }
        pool
    }

    /// Everything the current draft will notify when sent: mentions picked
    /// from the popup plus those typed in the body.
    pub(super) fn send_audience(&self) -> Audience {
        let draft = self.draft();
        let (mut ids, mut here) = draft.mention_payload();
        let typed = typed_mentions(&draft.body, &self.mention_pool(), self.composing_in_channel());
        here |= typed.here;
        for id in typed.ids {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        Audience { ids, here, ambiguous: typed.ambiguous }
    }

    /// The composer's one-line preview of who a send notifies:
    /// `→ @here (8), EP · @Builder ambiguous`. Empty when nobody is.
    pub(super) fn audience_preview(&self) -> String {
        let a = self.send_audience();
        let mut who: Vec<String> = Vec::new();
        if a.here {
            who.push(match self.current_channel().and_then(|c| c["member_count"].as_u64()) {
                Some(n) => format!("@here ({n})"),
                None => "@here".into(),
            });
        }
        for id in &a.ids {
            let name = self
                .people
                .iter()
                .find(|p| p["id"] == id.as_str())
                .and_then(|p| p["name"].as_str())
                .filter(|n| !n.is_empty())
                .unwrap_or(if id == "owner" { "Owner" } else { id });
            who.push(name.to_owned());
        }
        let mut out = if who.is_empty() { String::new() } else { format!("\u{2192} {}", who.join(", ")) };
        if !a.ambiguous.is_empty() {
            if !out.is_empty() {
                out.push_str(" \u{00b7} ");
            }
            out.push_str(&format!("{} ambiguous, not notified", a.ambiguous.join(" ")));
        }
        out
    }

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
        // The picked-entity payload alone holds no broadcast, but a send
        // counts the typed @here too (see typed_mentions_tests).
        assert!(!m.draft().mention_payload().1);
        assert!(m.send_audience().here);
        m.mention_key(&crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ));
        assert!(m.draft().mention_payload().1);
        assert!(m.send_audience().here);
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

    /// Owner 2026-10-22: a typed `@here` or exact `@Name` notifies like a
    /// picked one; code spans, URLs and ambiguous names don't.
    fn composer(body: &str) -> Messages {
        let mut m = Messages {
            mode: "compose".into(),
            target: json!({"channel": "org-leaders"}),
            channels: vec![json!({"path": "org-leaders", "id": "c1", "member_count": 8})],
            people: vec![
                json!({"id": "ep-1", "name": "EP"}),
                json!({"id": "pp-1", "name": "PP-Lead"}),
                json!({"id": "alpha", "name": "Alpha Builder"}),
                json!({"id": "b1", "name": "Builder"}),
                json!({"id": "b2", "name": "Builder"}),
            ],
            ..Messages::default()
        };
        m.set_draft(Draft { body: body.into(), cursor: body.len(), ..Draft::default() });
        m
    }

    #[test]
    fn a_typed_here_broadcasts_and_previews_the_member_count() {
        let _lock = crate::test_support::home_lock();
        let _home = super::super::tests::Home::new();
        for body in ["@here standup in 5", "ping @here.", "(@HERE) now", "ok\n@here"] {
            let m = composer(body);
            assert!(m.send_audience().here, "{body:?}");
            assert_eq!(m.audience_preview(), "\u{2192} @here (8)", "{body:?}");
        }
        // In a DM there is no @here.
        let mut m = composer("@here");
        m.target = json!({"dm": "ep-1"});
        m.channels.clear();
        assert!(!m.send_audience().here);
    }

    #[test]
    fn a_picked_here_still_broadcasts() {
        let _lock = crate::test_support::home_lock();
        let _home = super::super::tests::Home::new();
        let mut m = composer("@he");
        assert!(m.mention_key(&crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        assert_eq!(m.draft().body, "@here ");
        assert!(m.draft().mention_payload().1);
        assert_eq!(m.send_audience(), Audience { ids: vec![], here: true, ambiguous: vec![] });
    }

    #[test]
    fn here_in_code_or_glued_to_text_is_not_a_mention() {
        let _lock = crate::test_support::home_lock();
        let _home = super::super::tests::Home::new();
        for body in [
            "use `@here` to ping",
            "```\nchat_send(mention_here) # @here\n```",
            "see https://x.test/@here",
            "mail team@here.example",
            "``a @here b``",
            "`unclosed @here",
            "@hereafter",
        ] {
            let m = composer(body);
            assert!(!m.send_audience().here, "{body:?}");
            assert_eq!(m.audience_preview(), "", "{body:?}");
        }
        // Outside the span it still counts.
        assert!(composer("`code` then @here").send_audience().here);
    }

    #[test]
    fn a_typed_exact_name_mentions_that_member() {
        let _lock = crate::test_support::home_lock();
        let _home = super::super::tests::Home::new();
        let m = composer("@EP and @pp-lead, also @Alpha Builder: review");
        let a = m.send_audience();
        assert_eq!(a.ids, ["ep-1", "pp-1", "alpha"]);
        assert!(!a.here && a.ambiguous.is_empty());
        assert_eq!(m.audience_preview(), "\u{2192} EP, PP-Lead, Alpha Builder");
        // An id works too; a partial name doesn't.
        assert_eq!(composer("@ep-1 hi").send_audience().ids, ["ep-1"]);
        assert!(composer("@Alph hi").send_audience().ids.is_empty());
        // Picked and typed together, no duplicates.
        let mut m = composer("@EP");
        assert_eq!(m.mention_options(), vec![(Some("ep-1".into()), "EP".into())]);
        m.mention_key(&crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let mut d = m.draft();
        d.replace_text(d.body.len()..d.body.len(), "and @EP again @here");
        m.set_draft(d);
        let a = m.send_audience();
        assert_eq!((a.ids.clone(), a.here), (vec!["ep-1".to_string()], true));
        assert_eq!(m.audience_preview(), "\u{2192} @here (8), EP");
    }

    #[test]
    fn an_ambiguous_name_is_not_notified_and_the_preview_says_so() {
        let _lock = crate::test_support::home_lock();
        let _home = super::super::tests::Home::new();
        let m = composer("@Builder can you look");
        let a = m.send_audience();
        assert!(a.ids.is_empty());
        assert_eq!(a.ambiguous, ["@Builder"]);
        assert_eq!(m.audience_preview(), "@Builder ambiguous, not notified");
        // "Alpha Builder" is unambiguous: the longest name wins.
        assert_eq!(composer("@Alpha Builder").send_audience().ids, ["alpha"]);
        let m = composer("@EP and @Builder");
        assert_eq!(m.audience_preview(), "\u{2192} EP \u{00b7} @Builder ambiguous, not notified");
    }
}
