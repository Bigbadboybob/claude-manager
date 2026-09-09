//! Public channel discovery, self-service membership and admin member additions.
use super::*;

impl Messages {
    pub(super) fn browse_channels(&mut self) {
        self.start_form("channel_browser", vec![]);
        self.picker_selected = 0;
        self.pane = 1;
    }
    fn channel_matches(&self) -> Vec<Value> {
        if self.mode == "channel_add_member" {
            if !self.channel_candidates_ready {
                return vec![];
            }
            return self
                .picker_people()
                .into_iter()
                .filter(|p| !self.channel_members.iter().any(|m| m["id"] == p["id"]))
                .cloned()
                .collect();
        }
        if self.mode == "channel_roster" {
            let query = self.text.to_lowercase();
            let mut matches: Vec<_> = self
                .channel_members
                .iter()
                .filter(|p| {
                    ["name", "id"].iter().any(|key| {
                        p[key]
                            .as_str()
                            .unwrap_or("")
                            .to_lowercase()
                            .contains(&query)
                    })
                })
                .cloned()
                .collect();
            matches.sort_by(|a, b| {
                a["name"]
                    .as_str()
                    .cmp(&b["name"].as_str())
                    .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
            });
            return matches;
        }
        let query = self.text.trim().trim_start_matches('#').to_lowercase();
        let mut matches: Vec<_> = self
            .channels
            .iter()
            .filter(|c| {
                ["path", "name", "description"].iter().any(|key| {
                    c[key]
                        .as_str()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(&query)
                })
            })
            .cloned()
            .collect();
        let rank = |c: &Value| {
            let path = c["path"].as_str().unwrap_or("").to_lowercase();
            let name = c["name"].as_str().unwrap_or("").to_lowercase();
            if path == query || name == query {
                0
            } else if path.starts_with(&query) || name.starts_with(&query) {
                1
            } else {
                2
            }
        };
        matches.sort_by(|a, b| {
            rank(a)
                .cmp(&rank(b))
                .then_with(|| a["path"].as_str().cmp(&b["path"].as_str()))
        });
        matches
    }
    pub(super) fn can_post(&mut self) -> bool {
        if self
            .current_channel()
            .is_some_and(|c| c["archived"] == true)
        {
            self.error = "Channel archived · S opens channel settings. Your draft is saved.".into();
            return false;
        }
        if self.current_channel().is_some_and(|c| c["joined"] == false) {
            self.error =
                "Join before posting · J joins (Esc first from composer). Your draft is saved."
                    .into();
            false
        } else {
            true
        }
    }
}
impl App {
    pub(super) fn messaging_add_member(&mut self) {
        if self.messages.saved.management.pending.is_some() {
            self.messages.error = "A saved operation is pending; Esc then R retries it".into();
            return;
        }
        let Some(channel) = self.messages.current_channel() else {
            self.messages.error = "Choose a channel first; b browses channels".into();
            return;
        };
        if channel["can_manage"] != true {
            self.messages.error = "Only channel admins and Owner can add members".into();
            return;
        }
        let params = json!({"action":"members","conversation":channel["id"]});
        self.messages.start_form("channel_add_member", vec![]);
        self.messages.channel_candidates_ready = false;
        self.messages.picker_selected = 0;
        self.messages.pane = 1;
        self.messaging_request("channel_add_candidates", params);
    }

    pub(super) fn messaging_channel_members(&mut self) {
        let Some(channel) = self.messages.current_channel() else {
            self.messages.error = "Choose a channel first; b browses channels".into();
            return;
        };
        let params = json!({"action":"members","conversation":channel["id"]});
        self.messages.start_form("channel_roster", vec![]);
        self.messages.picker_selected = 0;
        self.messages.channel_members.clear();
        self.messaging_request("channel_members", params);
    }
    pub(super) fn messaging_membership(&mut self, join: bool) {
        let Some(c) = self.messages.current_channel() else {
            self.messages.error = "Choose a channel first; b browses channels".into();
            return;
        };
        self.messaging_mutation(
            "messaging.channels",
            json!({
            "action": if join { "join" } else { "leave" }, "conversation": c["id"]}),
        );
    }
    pub(super) fn messaging_channel_browser_key(
        &mut self,
        key: &crossterm::event::KeyEvent,
    ) -> bool {
        if !matches!(
            self.messages.mode.as_str(),
            "channel_browser" | "channel_roster" | "channel_add_member"
        ) {
            return false;
        }
        let matches = self.messages.channel_matches();
        match key.code {
            KeyCode::Esc => self.messages.mode.clear(),
            KeyCode::Char('a')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && self.messages.mode == "channel_roster" =>
            {
                self.messaging_add_member()
            }
            KeyCode::Down => {
                self.messages.picker_selected =
                    (self.messages.picker_selected + 1).min(matches.len().saturating_sub(1))
            }
            KeyCode::Up => {
                self.messages.picker_selected = self.messages.picker_selected.saturating_sub(1)
            }
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.messages.picker_selected =
                    (self.messages.picker_selected + 1).min(matches.len().saturating_sub(1))
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.messages.picker_selected = self.messages.picker_selected.saturating_sub(1)
            }
            KeyCode::Enter if self.messages.mode == "channel_browser" => {
                if let Some(c) = matches.get(self.messages.picker_selected) {
                    self.messages.target = json!({"channel": c["path"]});
                    self.messages.mode.clear();
                    self.messages.filter = json!({});
                    self.messages.page_cursor = Value::Null;
                    self.messages.loaded_target = Value::Null;
                    self.messages.channel_selection_pending = true;
                    self.messages.select_saved_channel();
                    self.messaging_refresh_target();
                }
            }
            KeyCode::Enter if self.messages.mode == "channel_add_member" => {
                if let Some(member) = matches.get(self.messages.picker_selected) {
                    if let Some(channel) = self.messages.current_channel() {
                        self.messaging_mutation(
                            "messaging.channels",
                            json!({
                                "action":"add_member", "conversation":channel["id"],
                                "participant_id":member["id"]
                            }),
                        );
                    }
                }
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
    pub(super) fn draw_channel_browser(&self, frame: &mut Frame, area: Rect) {
        let roster = self.messages.mode == "channel_roster";
        let add = self.messages.mode == "channel_add_member";
        let matches = self.messages.channel_matches();
        let mut lines = vec![
            Line::from(format!("Search: {}▏", self.messages.text)),
            Line::styled(
                if add {
                    "↑/↓ or Ctrl+j/k select · Enter adds · Esc cancels"
                } else if roster {
                    "↑/↓ select · Ctrl+a add member · type to filter · Esc returns"
                } else {
                    "↑/↓ select · Enter preview · then J join / L leave"
                },
                Style::default().fg(theme::CHAT_MUTED),
            ),
            Line::from(""),
        ];
        let visible = area.height.saturating_sub(5) as usize;
        let start = self
            .messages
            .picker_selected
            .saturating_sub(visible.saturating_sub(1));
        for (index, c) in matches.iter().enumerate().skip(start).take(visible) {
            let text = format!(
                "{} #{}{}{} · {}",
                if index == self.messages.picker_selected {
                    "›"
                } else {
                    " "
                },
                c["path"].as_str().unwrap_or("?"),
                if c["joined"] == true {
                    " · joined"
                } else {
                    ""
                },
                if c["default_join"] == true {
                    " · default"
                } else {
                    ""
                },
                c["description"].as_str().unwrap_or("")
            );
            let text = if roster || add {
                format!(
                    "{} {}{}{}",
                    if index == self.messages.picker_selected {
                        "›"
                    } else {
                        " "
                    },
                    c["name"]
                        .as_str()
                        .or_else(|| c["id"].as_str())
                        .unwrap_or("?"),
                    if c["present"] == false {
                        " · not running"
                    } else {
                        ""
                    },
                    if add {
                        format!(" · {}", c["id"].as_str().unwrap_or(""))
                    } else {
                        String::new()
                    }
                )
            } else {
                text
            };
            lines.push(Line::styled(
                text,
                Style::default().fg(theme::CHAT_TEXT).bg(
                    if index == self.messages.picker_selected {
                        theme::CHAT_SELECTION
                    } else {
                        theme::CHAT_PANEL
                    },
                ),
            ));
        }
        if matches.is_empty() {
            lines.push(Line::from(
                if add && !self.messages.channel_candidates_ready {
                    "Waiting for the participant directory"
                } else if add {
                    "No matching agents outside this channel"
                } else if roster {
                    "No matching members"
                } else {
                    "No matching channels"
                },
            ));
        }
        frame.render_widget(
            Paragraph::new(lines)
                .style(Style::default().bg(theme::CHAT_PANEL))
                .block(chat_block(
                    if add {
                        "Add channel member"
                    } else if roster {
                        "Channel members · @here audience"
                    } else {
                        "Browse channels"
                    },
                    true,
                )),
            area,
        );
        let selected = self.messages.picker_selected;
        if selected < matches.len() && selected >= start && selected - start < visible {
            frame.buffer_mut().set_style(
                Rect::new(
                    area.x + 1,
                    area.y + 4 + (selected - start) as u16,
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
    fn owner_add_member_picker_filters_roster_preserves_draft_and_retry_identity() {
        let _lock = crate::test_support::home_lock();
        let _home = super::super::tests::Home::new();
        let mut app = App::new(crate::config::Config {
            api_url: String::new(),
            api_token: String::new(),
            gcp_project: String::new(),
            gcp_zone: String::new(),
            repos: HashMap::new(),
        });
        app.messages.visible = true;
        app.messages.daemon_id = "origin".into();
        app.messages.channels =
            vec![json!({"id":"work-id","path":"work","joined":false,"can_manage":true})];
        app.messages.target = json!({"channel":"work"});
        app.messages.set_draft(Draft {
            body: "Keep this draft".into(),
            ..Draft::default()
        });
        app.messaging_channel_members();
        app.messaging_channel_browser_key(&crossterm::event::KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        ));
        assert_eq!(app.messages.mode, "channel_add_member");
        assert!(app.messages.channel_matches().is_empty()); // No stale candidates on a failed fetch.
        app.messages.channel_members = vec![json!({"id":"a","name":"Already-Joined"})];
        app.messages.people = vec![
            json!({"id":"owner","name":"Owner","present":true}),
            json!({"id":"a","name":"Already-Joined","present":true}),
            json!({"id":"b","name":"Parser","aliases":["Scout"],"present":true}),
            json!({"id":"c","name":"Builder","present":false}),
        ];
        app.messages.channel_candidates_ready = true;
        assert_eq!(app.messages.channel_matches().len(), 2);
        app.messages.text = "scout".into();
        assert_eq!(app.messages.channel_matches()[0]["id"], "b");
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 26)).unwrap();
        terminal.draw(|f| app.draw_messages(f)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.contains("Add channel member"), "{text}");
        assert!(text.contains("Parser"), "{text}");
        assert!(!text.contains("Already-Joined"), "{text}");
        app.messaging_channel_browser_key(&crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ));
        let pending = serde_json::to_value(&app.messages.saved.management.pending).unwrap();
        assert_eq!(pending["params"]["action"], "add_member");
        assert_eq!(pending["params"]["participant_id"], "b");
        assert_eq!(pending["params"]["conversation"], "work-id");
        assert_eq!(pending["params"]["origin_daemon_id"], "origin");
        assert!(pending["params"]["request_id"].is_string());
        app.messaging_channel_browser_key(&crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ));
        assert_eq!(
            serde_json::to_value(&app.messages.saved.management.pending).unwrap(),
            pending
        );
        app.messages.saved.management.pending = None;
        app.messaging_channel_result("messaging.channels", &json!({"status":"saved",
            "membership":{"joined":true,"current_joined":false,"added_by":"owner","participant_id":"b"},
            "channel":{"id":"work-id"}}));
        assert_eq!(app.messages.target, json!({"channel":"work"}));
        assert_eq!(app.messages.draft().body, "Keep this draft");
        assert!(app.messages.status.contains("has since left"));
        app.messages.channels[0]["can_manage"] = json!(false);
        app.messaging_add_member();
        assert!(app.messages.error.contains("Only channel admins"));
        assert!(app.messages.mode.is_empty());
    }

    #[test]
    fn membership_browser_and_compose_guard_preserve_public_preview_and_draft() {
        let _lock = crate::test_support::home_lock();
        let _home = super::super::tests::Home::new();
        let mut app = App::new(crate::config::Config {
            api_url: String::new(),
            api_token: String::new(),
            gcp_project: String::new(),
            gcp_zone: String::new(),
            repos: HashMap::new(),
        });
        app.messages.visible = true;
        app.messages.channels = vec![
            json!({"id":"g","path":"general","joined":true,"default_join":true}),
            json!({"id":"work","path":"work","description":"Parser handoffs","joined":false}),
        ];
        app.messages.target = json!({"channel":"work"});
        app.messages.set_draft(Draft {
            body: "Keep my draft".into(),
            ..Draft::default()
        });
        assert!(!app.messages.can_post());
        assert!(!app
            .messages
            .menu_items()
            .iter()
            .any(|(_, t)| t["channel"] == "work"));
        app.messages.browse_channels();
        app.messages
            .channels
            .push(json!({"id":"cm","path":"cm-general","joined":true}));
        app.messages.text = "general".into();
        assert_eq!(app.messages.channel_matches()[0]["path"], "general");
        app.messages.text = "parser".into();
        assert_eq!(app.messages.channel_matches().len(), 1);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| app.draw_messages(f)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.contains("Parser handoffs"), "{text}");
        app.messaging_channel_browser_key(&crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ));
        assert_eq!(app.messages.target, json!({"channel":"work"}));
        assert_eq!(app.messages.draft().body, "Keep my draft");
        assert!(app.messages.saved.management.pending.is_none()); // preview does not join
        app.messaging_membership(true);
        let pending = serde_json::to_value(&app.messages.saved.management.pending).unwrap();
        assert_eq!(pending["params"]["action"], "join");
        assert_eq!(pending["params"]["conversation"], "work");
        app.messaging_channel_result(
            "messaging.channels",
            &json!({"status":"saved", "membership":{"joined":true}, "channel":{"id":"work"}}),
        );
        assert_eq!(app.messages.target, json!({"channel":"work"}));
        assert_eq!(app.messages.draft().body, "Keep my draft");
    }
}
