//! Channel settings, pin controls, and the conversation description.
use super::*;

impl Messages {
    pub(super) fn select_saved_channel(&mut self) {
        if !self.channel_selection_pending {
            return;
        }
        if let Some(c) = self.current_channel() {
            if let Some(index) = self
                .menu_items()
                .iter()
                .position(|(_, target)| target["channel"] == c["path"])
            {
                self.menu = index;
            }
        }
        self.channel_selection_pending = false;
    }

    pub(super) fn current_channel(&self) -> Option<&Value> {
        self.channels
            .iter()
            .find(|c| c["path"] == self.target["channel"] || c["id"] == self.target["conversation"])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn messaging_channel_settings_pins_and_description_render_with_stable_target() {
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
        app.messages.pane = 1;
        app.messages.target = json!({"conversation":"channel"});
        app.messages.channels = vec![
            json!({"id":"channel","path":"stable/path","name":"Review room","description":"Coordinate parser reviews here.","created_by":"a","admins":["a","b"],"allow_agent_edits":false,"revision":"r1"}),
        ];
        app.messages.people = vec![
            json!({"id":"a","name":"Creator"}),
            json!({"id":"b","name":"Reviewer"}),
        ];
        app.messages.accept_messages(&json!({"items":[{"id":"msg","type":"message.create","body":"Keep this reference.","conversation_id":"channel","actor":{"id":"a","name":"Creator"}}],"pins":{"msg":{"actor":{"id":"b","name":"Reviewer"}}}}));
        assert_eq!(app.messages.items[0]["pinned"], true);
        assert_eq!(app.messages.target_label(), "#Review room");
        app.messages.channel_selection_pending = true;
        app.messages.select_saved_channel();
        assert_eq!(
            app.messages.menu_items()[app.messages.menu].1,
            json!({"channel":"stable/path"})
        );
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
        assert!(text.contains("Coordinate parser reviews here."), "{text}");
        assert!(text.contains("pinned"), "{text}");
        app.messaging_channel_form(true);
        assert_eq!(
            app.messages.fields,
            vec![
                "Review room",
                "Coordinate parser reviews here.",
                "no",
                "Creator, Reviewer",
                "no",
                "no"
            ]
        );
        assert_eq!(app.messages.channel_edit_base["revision"], "r1");
        terminal.draw(|f| app.draw_messages(f)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.contains("All agents may edit"), "{text}");
        app.messaging_event(&CrosstermEvent::Key(crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        let pending = serde_json::to_value(&app.messages.saved.management.pending).unwrap();
        assert_eq!(pending["method"], "messaging.channels");
        assert_eq!(pending["params"]["conversation"], "channel");
        assert_eq!(pending["params"]["expected_revision"], "r1");
        assert_eq!(pending["params"]["admins"], json!(["a", "b"]));
        app.messages.saved.management.pending = None;
        app.messages.people.clear();
        app.messaging_channel_form(true);
        app.messaging_save_channel();
        let pending = serde_json::to_value(&app.messages.saved.management.pending).unwrap();
        assert_eq!(pending["params"]["admins"], json!(["a", "b"]));
        app.messages.saved.management.pending = None;
        app.messages.fields[3] = "Creator, Reviewer".into();
        app.messages.fields[1] = "Long description ".repeat(50);
        app.messages.field = 3;
        let mut narrow =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(48, 16)).unwrap();
        narrow.draw(|f| app.draw_messages(f)).unwrap();
        let text = narrow
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.contains("Reviewer"), "{text}");
        app.messages.mode.clear();
        app.messaging_show_pins();
        assert_eq!(app.messages.filter, json!({"pinned_only":true}));
        assert_eq!(app.messages.target, json!({"conversation":"channel"}));
        app.messaging_show_pins();
        assert_eq!(app.messages.filter, json!({}));
    }
}
impl App {
    pub(super) fn draw_messaging_channel_form(&self, frame: &mut Frame, area: Rect) {
        let edit = self.messages.mode == "channel_edit";
        let inner = area.inner(ratatui::layout::Margin::new(1, 1));
        let width = inner.width.max(1) as usize;
        let labels = if edit {
            vec![
                "Display name",
                "Description",
                "All agents may edit (yes/no)",
                "Admin names or IDs",
                "Join by default (yes/no)",
                "Archived (yes/no)",
            ]
        } else {
            vec![
                "Permanent path",
                "Display name (optional)",
                "Description",
                "All agents may edit (yes/no)",
                "Additional admin names or IDs",
                "Join by default (yes/no)",
                "Archived (yes/no)",
            ]
        };
        let mut lines = Vec::new();
        let help = if edit {
            format!(
                "Address: #{} · Creator: {} · Owner is always an admin",
                self.messages.channel_edit_base["path"]
                    .as_str()
                    .unwrap_or(""),
                self.messages.person_name(
                    self.messages.channel_edit_base["created_by"]
                        .as_str()
                        .unwrap_or("owner")
                )
            )
        } else {
            "Creator is an admin by default; Owner always remains an admin. Open editing allows names, descriptions and pins; only admins manage access.".into()
        };
        lines.extend(
            manage::wrap_readable(&help, width)
                .into_iter()
                .map(|s| Line::styled(s, Style::default().fg(theme::CHAT_MUTED))),
        );
        lines.push(Line::from(""));
        let mut focus_end = 0;
        for (i, value) in self.messages.fields.iter().enumerate() {
            let active = i == self.messages.field;
            let text = format!(
                "{} {}: {}{}",
                if active { "›" } else { " " },
                labels[i],
                value,
                if active { "▏" } else { "" }
            );
            let style = Style::default()
                .fg(if active {
                    theme::CHAT_TEXT
                } else {
                    theme::CHAT_MUTED
                })
                .bg(if active {
                    theme::CHAT_SELECTION
                } else {
                    theme::CHAT_PANEL
                });
            lines.extend(
                manage::wrap_readable(&text, width)
                    .into_iter()
                    .map(|line| Line::styled(line, style)),
            );
            if active {
                focus_end = lines.len();
            }
            lines.push(Line::from(""));
        }
        let start = focus_end.saturating_sub(inner.height as usize);
        frame.render_widget(
            Paragraph::new(
                lines
                    .into_iter()
                    .skip(start)
                    .take(inner.height as usize)
                    .collect::<Vec<_>>(),
            )
            .style(Style::default().bg(theme::CHAT_PANEL))
            .block(chat_block(
                format!(
                    "{} · Tab next · Enter saves · Esc cancels",
                    if edit {
                        "Channel settings"
                    } else {
                        "New channel"
                    }
                ),
                true,
            )),
            area,
        );
    }
    pub(super) fn messaging_channel_form(&mut self, edit: bool) {
        if self.messages.saved.management.pending.is_some() {
            self.messages.error = "A saved operation is pending; R retries it".into();
            return;
        }
        if edit {
            let Some(c) = self.messages.current_channel().cloned() else {
                self.messages.error = "Select a channel to change its settings".into();
                return;
            };
            let admins = c["admins"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(|id| self.messages.person_name(id))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            self.messages.start_form(
                "channel_edit",
                vec![
                    c["name"]
                        .as_str()
                        .or_else(|| c["path"].as_str())
                        .unwrap_or("")
                        .into(),
                    c["description"].as_str().unwrap_or("").into(),
                    if c["allow_agent_edits"] == true {
                        "yes"
                    } else {
                        "no"
                    }
                    .into(),
                    admins,
                    if c["default_join"] == true {
                        "yes"
                    } else {
                        "no"
                    }
                    .into(),
                    if c["archived"] == true { "yes" } else { "no" }.into(),
                ],
            );
            self.messages.channel_edit_base = c;
        } else {
            self.messages.start_form(
                "channel",
                vec![
                    String::new(),
                    String::new(),
                    String::new(),
                    "no".into(),
                    String::new(),
                    "no".into(),
                    "no".into(),
                ],
            );
            self.messages.channel_edit_base = Value::Null;
        }
    }
    pub(super) fn messaging_save_channel(&mut self) {
        let fields = self.messages.fields.clone();
        let edit = self.messages.mode == "channel_edit";
        let offset = usize::from(!edit);
        let open = match fields[offset + 2].trim().to_lowercase().as_str() {
            "yes" | "true" => true,
            "no" | "false" => false,
            _ => {
                self.messages.error = "All agents may edit: enter yes or no".into();
                return;
            }
        };
        let default_join = match fields[offset + 4].trim().to_lowercase().as_str() {
            "yes" | "true" => true,
            "no" | "false" => false,
            _ => {
                self.messages.error = "Join by default: enter yes or no".into();
                return;
            }
        };
        let archived = match fields[offset + 5].trim().to_lowercase().as_str() {
            "yes" | "true" => true,
            "no" | "false" => false,
            _ => {
                self.messages.error = "Archived: enter yes or no".into();
                return;
            }
        };
        let mut admins = Vec::new();
        for name in fields[offset + 3]
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            // A saved admin may have exited before enrolling a messaging name.
            // Preserve known IDs even when the people directory no longer has them.
            if edit
                && self.messages.channel_edit_base["admins"]
                    .as_array()
                    .is_some_and(|ids| ids.iter().any(|id| id == name))
            {
                admins.push(json!(name));
                continue;
            }
            let hits: Vec<_> = self
                .messages
                .people
                .iter()
                .filter(|p| {
                    p["id"] == name
                        || p["name"]
                            .as_str()
                            .is_some_and(|n| n.eq_ignore_ascii_case(name))
                })
                .collect();
            if hits.len() != 1 {
                self.messages.error =
                    format!("Admin '{name}' is missing or ambiguous; use their participant ID");
                return;
            }
            admins.push(hits[0]["id"].clone());
        }
        let mut p = json!({"action":if edit { "update" } else { "create" },"description":fields[offset+1],"allow_agent_edits":open,"admins":admins,"default_join":default_join,"archived":archived});
        if edit {
            p["conversation"] = self.messages.channel_edit_base["id"].clone();
            p["expected_revision"] = self.messages.channel_edit_base["revision"].clone();
        } else {
            p["path"] = json!(fields[0].trim());
        }
        if edit || !fields[offset].trim().is_empty() {
            p["name"] = json!(fields[offset].trim());
        }
        self.messaging_mutation("messaging.channels", p);
    }
    pub(super) fn messaging_pin_selected(&mut self) {
        if self.messages.pane != 1 {
            self.messages.error = "Select a message before pinning it".into();
            return;
        }
        let Some(m) = self.messages.items.get(self.messages.selected) else {
            return;
        };
        if m["type"].as_str().is_some_and(|t| t != "message.create") {
            return;
        }
        self.messaging_request("pin_prepare", json!({"conversation":m["conversation_id"],"message_id":m["id"],"pinned":m["pinned"] != true}));
    }
    pub(super) fn messaging_pin_prepared(&mut self, value: &Value) {
        let p = &value["intent"];
        self.messaging_mutation("messaging.pins", json!({"action":if p["pinned"] == true { "set" } else { "remove" },
            "conversation":p["conversation"],"message_id":p["message_id"],"expected_revision":value["revision"]}));
    }
    pub(super) fn messaging_show_pins(&mut self) {
        if self.messages.target["channel"] == "*"
            || !["channel", "dm", "conversation"]
                .iter()
                .any(|k| self.messages.target.get(*k).is_some())
        {
            self.messages.error = "Choose a channel or DM to view its pins".into();
            return;
        }
        self.messages.filter = if self.messages.filter["pinned_only"] == true {
            json!({})
        } else {
            json!({"pinned_only":true})
        };
        self.messages.page_cursor = Value::Null;
        self.messages.loaded_target = Value::Null;
        self.messages.pane = 1;
        self.messaging_refresh_target();
    }
    pub(super) fn messaging_channel_result(&mut self, method: &str, value: &Value) {
        if value["status"] == "conflict" {
            self.messages.error = if method == "messaging.channels" {
                "Channel settings changed; Esc then S loads the latest settings before you retry"
                    .into()
            } else {
                "Pins changed; review them and press p again".into()
            };
            if let Some(c) = value.get("channel") {
                if let Some(old) = self
                    .messages
                    .channels
                    .iter_mut()
                    .find(|old| old["id"] == c["id"])
                {
                    *old = c.clone();
                }
            }
            if method == "messaging.pins" {
                self.messaging_refresh_target();
            }
            return;
        }
        self.messages.mode.clear();
        self.messages.fields.clear();
        self.messages.error.clear();
        if method == "messaging.channels" {
            self.messages.status = if value["membership"]["joined"] == true { "Joined channel" }
                else if value["membership"]["joined"] == false { "Left channel · history remains browsable" }
                else { "Channel settings saved" }.into();
            self.messages.channel_selection_pending = true;
            // Joining/leaving must preserve the draft's address (path or ID).
            if value.get("membership").is_none() {
                self.messages.target = json!({"conversation":value["channel"]["id"]});
            }
            self.messages.filter = json!({});
            self.messages.loaded_target = Value::Null;
            self.messages.page_cursor = Value::Null;
            self.messaging_request("bootstrap", json!({}));
        } else {
            self.messages.status = if value["pinned"] == true {
                "Message pinned"
            } else {
                "Message unpinned"
            }
            .into();
            self.messaging_refresh_target();
        }
    }
    pub(super) fn draw_channel_summary(
        &self,
        frame: &mut Frame,
        area: Rect,
        focused: bool,
    ) -> Rect {
        let Some(c) = self.messages.current_channel() else {
            return area;
        };
        if area.height < 8 {
            return area;
        }
        let width = area.width.saturating_sub(2).max(1) as usize;
        let mut lines: Vec<Line> = Vec::new();
        let description = c["description"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("No description yet · S edits channel settings");
        let wrapped = manage::wrap_readable(description, width);
        for (i, line) in wrapped.iter().take(2).enumerate() {
            lines.push(Line::styled(
                if i == 1 && wrapped.len() > 2 {
                    format!(
                        "{}…",
                        line.chars()
                            .take(width.saturating_sub(1))
                            .collect::<String>()
                    )
                } else {
                    line.clone()
                },
                Style::default().fg(theme::CHAT_MUTED),
            ));
        }
        lines.push(Line::styled(
            format!(
                "{} · {} members · u list{}",
                if c["joined"] == false {
                    "Preview · J join to post"
                } else {
                    "Joined · L leave"
                },
                c["member_count"].as_u64().unwrap_or(0),
                if c["archived"] == true {
                    " · archived"
                } else if c["default_join"] == true {
                    " · joined by default"
                } else {
                    ""
                }
            ),
            Style::default().fg(theme::CHAT_FOCUS),
        ));
        let parts = Layout::vertical([
            Constraint::Length(lines.len() as u16 + 2),
            Constraint::Min(3),
        ])
        .split(area);
        let title = format!(
            "#{} · {} · S settings",
            c["path"].as_str().unwrap_or(""),
            if c["allow_agent_edits"] == true {
                "agents can edit"
            } else {
                "admins edit"
            }
        );
        frame.render_widget(
            Paragraph::new(lines)
                .style(Style::default().bg(theme::CHAT_PANEL))
                .block(chat_block(title, focused)),
            parts[0],
        );
        parts[1]
    }
}
