//! Full conversation timeline and recipient search, shared by DMs and groups.
use super::*;

impl Messages {
    pub(super) fn person_name(&self, id: &str) -> String {
        self.people
            .iter()
            .find(|p| p["id"] == id)
            .and_then(|p| p["name"].as_str())
            .unwrap_or(id)
            .to_owned()
    }
    pub(super) fn dm_label(&self, dm: &Value) -> String {
        let peers: Vec<_> = dm["peers"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_else(|| dm["peer"].as_str().into_iter().collect());
        peers
            .iter()
            .map(|id| self.person_name(id))
            .collect::<Vec<_>>()
            .join(", ")
    }
    fn picker_people(&self) -> Vec<&Value> {
        let query = self.text.trim().to_lowercase();
        let mut people: Vec<_> = self
            .people
            .iter()
            .filter(|p| {
                p["id"] != "owner"
                    && (query.is_empty()
                        || ["name", "id", "session_uid", "task"].iter().any(|k| {
                            p[*k]
                                .as_str()
                                .is_some_and(|s| s.to_lowercase().contains(&query))
                        })
                        || p["aliases"].as_array().is_some_and(|a| {
                            a.iter().any(|v| {
                                v.as_str()
                                    .is_some_and(|s| s.to_lowercase().contains(&query))
                            })
                        }))
            })
            .collect();
        people.sort_by(|a, b| {
            (b["present"] == true)
                .cmp(&(a["present"] == true))
                .then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
        });
        people
    }
    pub(super) fn live_conversation(&self) -> bool {
        self.filter.as_object().is_some_and(|f| f.is_empty())
            && (self.target["channel"].as_str().is_some_and(|p| p != "*")
                || self.target.get("dm").is_some() || self.target.get("conversation").is_some())
    }
    pub(super) fn accept_messages(&mut self, value: &Value) {
        for (key, dst) in [
            ("_dms", &mut self.dms),
            ("_people", &mut self.people),
            ("_channels", &mut self.channels),
        ] {
            if let Some(items) = value[key].as_array() {
                *dst = items.clone();
            }
        }
        let query = self.query();
        let same = self.loaded_target == query;
        let old = same
            .then(|| self.items.get(self.selected))
            .flatten()
            .map(|m| m["id"].clone());
        let at_bottom = same && self.selected + 1 >= self.items.len();
        let mut incoming = value["items"].as_array().cloned().unwrap_or_default();
        // The service pages newest first. The screen always reads top to bottom.
        incoming.reverse();
        if same && (self.append_older || !self.page_cursor.is_null() || self.live_conversation()) {
            // Keep loaded history and update read flags on overlapping pages.
            for old in &mut self.items {
                if let Some(updated) = incoming.iter().find(|m| m["id"] == old["id"]) {
                    *old = updated.clone();
                }
            }
            incoming.retain(|m| !self.items.iter().any(|old| old["id"] == m["id"]));
            if self.append_older {
                incoming.append(&mut self.items);
            } else {
                self.items.append(&mut incoming);
                incoming = std::mem::take(&mut self.items);
            }
        }
        self.items = incoming;
        self.selected = if at_bottom && !self.append_older {
            self.items.len().saturating_sub(1)
        } else {
            old.as_ref()
                .and_then(|id| self.items.iter().position(|m| m["id"] == *id))
                .unwrap_or(self.items.len().saturating_sub(1))
        };
        self.reveal_selection |= !same
            || self.append_older
            || old != self.items.get(self.selected).map(|m| m["id"].clone());
        if !same {
            self.timeline_scroll = 0;
        }
        self.loaded_target = query;
        if self.select_older {
            self.selected = self.selected.saturating_sub(1);
            self.select_older = false;
        }
        if !same || self.append_older || !self.live_conversation() {
            self.next = value["next_cursor"].clone();
        }
        self.append_older = false;
        if let Some(ids) = self.management.request["ack_receipt"]["ids"].as_array() {
            for item in &mut self.items {
                if ids.contains(&item["id"]) { item["read"] = json!(true); }
            }
        }
        self.receipt = value["receipt"].clone();
        self.management.last_position = value["position"].clone();
    }
    fn unread_marker(&self, m: &Value) -> (&'static str, Style) {
        let muted = Style::default().fg(theme::CHAT_MUTED);
        if m["read"] == true || m["actor"]["id"] == "owner" {
            return ("", muted);
        }
        let mentioned = m["data"]["mentions"]
            .as_array()
            .is_some_and(|a| a.iter().any(|id| id == "owner"));
        let dm = m["conversation_kind"] == "dm"
            || self.dms.iter().any(|d| d["id"] == m["conversation_id"]);
        if mentioned {
            (
                "● @you",
                Style::default()
                    .fg(theme::CHAT_TAG)
                    .add_modifier(Modifier::BOLD),
            )
        } else if dm {
            (
                "● DM",
                Style::default()
                    .fg(theme::CHAT_TAG)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            ("·", muted)
        }
    }
}
impl App {
    pub(super) fn messaging_picker_key(&mut self, key: &crossterm::event::KeyEvent) -> bool {
        if self.messages.mode != "dm_picker" {
            return false;
        }
        match key.code {
            KeyCode::Esc => self.messages.mode.clear(),
            KeyCode::Down => {
                self.messages.picker_selected = (self.messages.picker_selected + 1)
                    .min(self.messages.picker_people().len().saturating_sub(1))
            }
            KeyCode::Up => {
                self.messages.picker_selected = self.messages.picker_selected.saturating_sub(1)
            }
            KeyCode::Char('j' | 'n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.messages.picker_selected = (self.messages.picker_selected + 1)
                    .min(self.messages.picker_people().len().saturating_sub(1));
            }
            KeyCode::Char('k' | 'p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.messages.picker_selected = self.messages.picker_selected.saturating_sub(1);
            }
            KeyCode::Tab | KeyCode::BackTab => {
                let id = self
                    .messages
                    .picker_people()
                    .get(self.messages.picker_selected)
                    .and_then(|p| p["id"].as_str())
                    .map(str::to_owned);
                if let Some(id) = id {
                    if self.messages.picker_members.contains(&id) {
                        self.messages.picker_members.retain(|p| p != &id);
                    } else if self.messages.picker_members.len() < 31 {
                        self.messages.picker_members.push(id);
                    } else {
                        self.messages.error = "A group can include you and up to 31 others".into();
                    }
                }
            }
            KeyCode::Enter => {
                let mut members = self.messages.picker_members.clone();
                if members.is_empty() {
                    if let Some(id) = self
                        .messages
                        .picker_people()
                        .get(self.messages.picker_selected)
                        .and_then(|p| p["id"].as_str())
                    {
                        members.push(id.to_owned());
                    }
                }
                if members.is_empty() {
                    self.messages.error = "Choose at least one recipient".into();
                    return true;
                }
                members.sort();
                let mut all = members.clone();
                all.push("owner".into());
                all.sort();
                let existing = self.messages.dms.iter().find(|d| {
                    d["members"] == json!(all) || members.len() == 1 && d["peer"] == members[0]
                });
                self.messages.target = existing
                    .map(|d| json!({"conversation":d["id"]}))
                    .unwrap_or_else(|| {
                        if members.len() == 1 {
                            json!({"dm":members[0]})
                        } else {
                            json!({"dm":members})
                        }
                    });
                self.messages.filter = json!({});
                self.messages.page_cursor = Value::Null;
                self.messages.loaded_target = Value::Null;
                self.messages.pane = 1;
                self.messages.mode = "compose".into();
                self.messages.saved.dms_collapsed = false;
                self.messages.persist();
                self.messaging_request("messaging.read", self.messages.query());
            }
            KeyCode::Backspace => {
                self.messages.text.pop();
                self.messages.picker_selected = 0;
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.messages.text.push(c);
                self.messages.picker_selected = 0;
            }
            _ => {}
        }
        true
    }
    pub(super) fn draw_messaging_picker(&self, frame: &mut Frame, area: Rect) {
        let inner = area.inner(ratatui::layout::Margin::new(1, 1));
        let width = inner.width.max(1) as usize;
        let muted = Style::default().fg(theme::CHAT_MUTED);
        let chosen = self
            .messages
            .picker_members
            .iter()
            .map(|id| self.messages.person_name(id))
            .collect::<Vec<_>>()
            .join(", ");
        let mut lines = vec![Line::styled(
            format!("Search: {}▏", self.messages.text),
            Style::default().fg(theme::CHAT_TEXT),
        )];
        lines.extend(
            manage::wrap_readable(
                &format!(
                    "Recipients: {}",
                    if chosen.is_empty() {
                        "select a person; Tab adds to a group"
                    } else {
                        &chosen
                    }
                ),
                width,
            )
            .into_iter()
            .map(|s| Line::styled(s, Style::default().fg(theme::CHAT_AGENT))),
        );
        lines.extend(
            manage::wrap_readable(
                "Type to search · ↑/↓ move · Tab toggle · Enter compose · Esc cancel",
                width,
            )
            .into_iter()
            .map(|s| Line::styled(s, muted)),
        );
        lines.push(Line::from(""));
        let height = (inner.height as usize).saturating_sub(lines.len());
        let people = self.messages.picker_people();
        if people.is_empty() {
            lines.push(Line::styled("No matching agents", muted));
        }
        let start = self
            .messages
            .picker_selected
            .saturating_sub(height.saturating_sub(1));
        lines.extend(
            people
                .iter()
                .enumerate()
                .skip(start)
                .take(height)
                .map(|(i, p)| {
                    let chosen = p["id"]
                        .as_str()
                        .is_some_and(|id| self.messages.picker_members.iter().any(|m| m == id));
                    Line::styled(
                        format!(
                            "{} {}{}",
                            if chosen { "[✓]" } else { "[ ]" },
                            p["name"].as_str().unwrap_or("?"),
                            if p["present"] == true {
                                ""
                            } else {
                                " (offline)"
                            }
                        ),
                        Style::default().fg(theme::CHAT_AGENT).bg(
                            if i == self.messages.picker_selected {
                                theme::CHAT_SELECTION
                            } else {
                                theme::CHAT_PANEL
                            },
                        ),
                    )
                }),
        );
        frame.render_widget(
            Paragraph::new(lines)
                .style(Style::default().bg(theme::CHAT_PANEL))
                .block(chat_block("New DM · choose one or several people", true)),
            area,
        );
    }
    pub(super) fn draw_messaging_timeline(&mut self, frame: &mut Frame, area: Rect, focused: bool) {
        let inner = area.inner(ratatui::layout::Margin::new(1, 1));
        let width = inner.width.max(1) as usize;
        if self.messages.timeline_size != (inner.width, inner.height) {
            self.messages.reveal_selection = true;
            self.messages.timeline_size = (inner.width, inner.height);
        }
        let muted = Style::default().fg(theme::CHAT_MUTED);
        let mut lines = Vec::new();
        let mut selected_range = (0, 0);
        for (i, m) in self.messages.items.iter().enumerate() {
            let start = lines.len();
            let selected = i == self.messages.selected;
            let bg = if selected {
                theme::CHAT_SELECTION
            } else {
                theme::CHAT_PANEL
            };
            let (marker, marker_style) = self.messages.unread_marker(m);
            let stamp = m["created_at"].as_str().unwrap_or("");
            let stamp = stamp.get(..16).unwrap_or(stamp).replace('T', " ");
            lines.push(
                Line::from(vec![
                    Span::styled(
                        if selected { "▎ " } else { "  " },
                        Style::default().fg(theme::CHAT_FOCUS),
                    ),
                    Span::styled(
                        if marker.is_empty() {
                            String::new()
                        } else {
                            format!("{marker} ")
                        },
                        marker_style,
                    ),
                    Span::styled(
                        m["actor"]["name"].as_str().unwrap_or("?").to_owned(),
                        chat_actor_style(m).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("  {stamp}"), muted),
                ])
                .style(Style::default().bg(bg)),
            );
            if self.messages.target["inbox"] == true
                || self.messages.target["dms"] == true
                || self.messages.target["channel"] == "*"
            {
                lines.push(Line::styled(
                    self.messages.conversation_label(&m["conversation_id"]),
                    Style::default().fg(theme::CHAT_FOCUS).bg(bg),
                ));
            }
            if m["data"]["reply_to"].is_string() {
                lines.push(Line::styled("↳ reply", muted.bg(bg)));
            }
            lines.extend(
                manage::wrap_readable(m["body"].as_str().unwrap_or(""), width)
                    .into_iter()
                    .map(|s| Line::styled(s, Style::default().fg(theme::CHAT_TEXT).bg(bg))),
            );
            if let Some(tags) = m["data"]["tags"].as_array().filter(|a| !a.is_empty()) {
                let text = tags
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|s| format!("#{s}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                lines.extend(
                    manage::wrap_readable(&text, width)
                        .into_iter()
                        .map(|s| Line::styled(s, Style::default().fg(theme::CHAT_TAG).bg(bg))),
                );
            }
            if let Some(links) = m["data"]["links"].as_array() {
                for link in links {
                    let text = format!(
                        "{}: {}",
                        link["label"].as_str().unwrap_or("Reference"),
                        link["uri"].as_str().unwrap_or("")
                    );
                    lines.extend(
                        manage::wrap_readable(&text, width).into_iter().map(|s| {
                            Line::styled(s, Style::default().fg(theme::CHAT_FOCUS).bg(bg))
                        }),
                    );
                }
            }
            if selected {
                selected_range = (start, lines.len());
            }
            lines.push(Line::from(""));
        }
        if lines.is_empty() {
            lines.push(Line::styled(
                "No messages yet. c composes · d starts a DM.",
                muted,
            ));
        }
        let height = inner.height as usize;
        let scroll = &mut self.messages.timeline_scroll;
        if self.messages.reveal_selection {
            if selected_range.0 < *scroll
                || selected_range.1.saturating_sub(selected_range.0) > height
            {
                *scroll = selected_range.0;
            } else if selected_range.1 > *scroll + height {
                *scroll = selected_range.1.saturating_sub(height);
            }
            self.messages.reveal_selection = false;
        }
        *scroll = (*scroll).min(lines.len().saturating_sub(height));
        let selected_top = selected_range.0.saturating_sub(*scroll).min(height);
        let selected_bottom = selected_range.1.saturating_sub(*scroll).min(height);
        let visible = lines
            .into_iter()
            .skip(*scroll)
            .take(height)
            .collect::<Vec<_>>();
        let title = format!("{} · j/k select · Enter read", self.messages.target_label());
        frame.render_widget(
            Paragraph::new(visible)
                .style(Style::default().bg(theme::CHAT_PANEL))
                .block(chat_block(title, focused)),
            area,
        );
        // Paragraph line styles stop at the final glyph. Fill the selected
        // message's visible rows too, including empty lines and trailing space.
        frame.buffer_mut().set_style(
            Rect::new(inner.x, inner.y + selected_top as u16, inner.width,
                (selected_bottom - selected_top) as u16),
            Style::default().bg(theme::CHAT_SELECTION),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn message(id: &str, body: &str, kind: &str, mentions: Value) -> Value {
        json!({"id":id,"body":body,"conversation_id":"chat","conversation_kind":kind,"read":false,
            "actor":{"id":"agent:a","name":"Scout"},"data":{"mentions":mentions},"created_at":"2026-09-07T12:00:00Z"})
    }
    fn app() -> App {
        App::new(crate::config::Config {
            api_url: String::new(),
            api_token: String::new(),
            gcp_project: String::new(),
            gcp_zone: String::new(),
            repos: HashMap::new(),
        })
    }
    #[test]
    fn messaging_timeline_orders_full_messages_and_keeps_selection_when_loading_older() {
        let _lock = crate::test_support::home_lock();
        let _home = super::super::tests::Home::new();
        let mut a = app();
        a.messages.visible = true;
        a.messages.pane = 1;
        let one = message(
            "one",
            "Oldest full message.\nIts second paragraph stays visible.",
            "channel",
            json!([]),
        );
        let two = message("two", "Newest full message.", "channel", json!(["owner"]));
        a.messages
            .accept_messages(&json!({"items":[two,one],"next_cursor":{"page":1}}));
        assert_eq!(a.messages.items[0]["id"], "one");
        assert_eq!(a.messages.selected, 1);
        a.messaging_event(&CrosstermEvent::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('k'),
            KeyModifiers::NONE,
        )));
        assert_eq!(a.messages.selected, 0);
        let backend = ratatui::backend::TestBackend::new(110, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| a.draw_messages(f)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(
            text.find("Oldest full message.").unwrap() < text.find("Newest full message.").unwrap()
        );
        assert!(text.contains("Its second paragraph stays visible."));
        assert!(text.contains("● @you"));
        assert!(!text.contains("PgUp/PgDn scroll")); // the old separate detail pane is gone
        a.messages.append_older = true;
        a.messages.accept_messages(
            &json!({"items":[message("zero","Earlier history","channel",json!([]))]}),
        );
        assert_eq!(a.messages.items.len(), 3);
        assert_eq!(a.messages.items[a.messages.selected]["id"], "one");
        assert_eq!(a.messages.items[0]["id"], "zero");
        a.messages.selected = 0;
        a.messages.next = json!({"page":2});
        a.messaging_event(&CrosstermEvent::Key(crossterm::event::KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE)));
        assert!(a.messages.select_older);
        a.messages.accept_messages(&json!({"items":[message("before", "Fetched by k", "channel", json!([]))]}));
        assert_eq!(a.messages.items[a.messages.selected]["id"], "before");
        let oldest_cursor = a.messages.next.clone();
        a.messages.accept_messages(&json!({"items":[message("new", "Arrived while reading history", "channel", json!([]))], "next_cursor":{"newer_page":1}}));
        assert_eq!(a.messages.items[a.messages.selected]["id"], "before");
        assert_eq!(a.messages.items.last().unwrap()["id"], "new");
        assert_eq!(a.messages.next, oldest_cursor);
        a.messages.management.request = json!({"ack_receipt":{"ids":["before"]}});
        a.messages.accept_messages(&json!({"items":[]}));
        assert_eq!(a.messages.items[0]["read"], true);
        assert_eq!(
            a.messages
                .unread_marker(&message("dm", "private", "dm", json!([])))
                .0,
            "● DM"
        );
        assert_eq!(a.messages.unread_marker(&a.messages.items[0]).0, "");
        assert_eq!(a.messages.unread_marker(&a.messages.items[1]).0, "·");
    }
    #[test]
    fn messaging_dm_sidebar_contains_started_conversations_and_picker_selects_a_group() {
        let _lock = crate::test_support::home_lock();
        let _home = super::super::tests::Home::new();
        let mut a = app();
        a.messages.visible = true;
        a.messages.people = vec![
            json!({"id":"a","name":"Alpha","present":true}),
            json!({"id":"b","name":"Beta","present":true}),
            json!({"id":"owner","name":"Owner"}),
        ];
        assert!(!a
            .messages
            .menu_items()
            .iter()
            .any(|(label, _)| label.contains("Alpha")));
        a.messages.dms =
            vec![json!({"id":"pair","peer":"a","peers":["a"],"members":["a","owner"],"unread":1})];
        assert!(a
            .messages
            .menu_items()
            .iter()
            .any(|(label, _)| label.contains("●1 Alpha")));
        a.messages.saved.dms_collapsed = true;
        assert!(!a
            .messages
            .menu_items()
            .iter()
            .any(|(_, t)| t["conversation"] == "pair"));
        let key = |c| CrosstermEvent::Key(crossterm::event::KeyEvent::new(c, KeyModifiers::NONE));
        a.messaging_event(&key(KeyCode::Char('d')));
        a.messaging_event(&key(KeyCode::Char('A')));
        a.messaging_event(&key(KeyCode::Char('l')));
        assert_eq!(a.messages.picker_people().len(), 1);
        a.messaging_event(&key(KeyCode::Tab));
        a.messaging_event(&key(KeyCode::Backspace));
        a.messaging_event(&key(KeyCode::Backspace));
        a.messaging_event(&key(KeyCode::Char('B')));
        a.messaging_event(&key(KeyCode::Tab));
        a.messaging_event(&key(KeyCode::Enter));
        assert_eq!(a.messages.target, json!({"dm":["a","b"]}));
        assert_eq!(a.messages.mode, "compose");
        assert_eq!(a.messages.dms.len(), 1); // opening a draft creates no sidebar conversation
    }
    #[test]
    fn messaging_long_message_scroll_is_not_reset_by_refresh_and_wraps_narrow() {
        let _lock = crate::test_support::home_lock();
        let _home = super::super::tests::Home::new();
        let mut a = app();
        a.messages.visible = true;
        a.messages.pane = 1;
        let value = json!({"items":[message("long", &(0..60).map(|i|format!("Line {i}: complete message text.\n")).collect::<String>(),"dm",json!([]))]});
        a.messages.accept_messages(&value);
        let backend = ratatui::backend::TestBackend::new(48, 16);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| a.draw_messages(f)).unwrap();
        a.messages.timeline_scroll = 20;
        a.messages.accept_messages(&value);
        terminal.draw(|f| a.draw_messages(f)).unwrap();
        assert_eq!(a.messages.timeline_scroll, 20);
    }
}
