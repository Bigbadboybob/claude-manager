//! Full conversation timeline and recipient search, shared by DMs and groups.
use super::*;

impl Messages {
    pub(super) fn actor_name(&self, event: &Value) -> String {
        self.people
            .iter()
            .find(|p| p["id"].as_str().is_some() && p["id"] == event["actor"]["id"])
            .and_then(|p| p["name"].as_str())
            .or_else(|| event["actor"]["name"].as_str())
            .or_else(|| event["actor"]["id"].as_str())
            .unwrap_or("?")
            .to_owned()
    }

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
    pub(super) fn picker_people(&self) -> Vec<&Value> {
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
        self.layout_epoch += 1;
        let broad = self.target["inbox"] == true
            || self.target["dms"] == true
            || self.target["mentions"] == true
            || self.target["channel"] == "*";
        let mut stale_layout = false;
        for (key, dst) in [
            ("_dms", &mut self.dms),
            ("_people", &mut self.people),
            ("_channels", &mut self.channels),
        ] {
            if let Some(items) = value[key].as_array() {
                // Names (and, in mixed lists, conversation labels) are part
                // of each rendered message.
                stale_layout |= (key == "_people" || broad) && dst != items;
                *dst = items.clone();
            }
        }
        if stale_layout {
            self.layout.clear();
        }
        let query = self.query();
        let same = self.loaded_target == query;
        let old = same
            .then(|| self.items.get(self.selected))
            .flatten()
            .map(|m| m["id"].clone());
        let at_bottom = same && self.selected + 1 >= self.items.len();
        let first_before = self.items.first().map(|m| m["id"].clone());
        if !same {
            self.layout.clear();
        }
        let mut incoming = value["items"].as_array().cloned().unwrap_or_default();
        // The service pages newest first. The screen always reads top to bottom.
        incoming.reverse();
        if same && (self.append_older || !self.page_cursor.is_null() || self.live_conversation()) {
            // Keep loaded history and update read flags on overlapping pages.
            let fresh: HashMap<String, usize> = incoming
                .iter()
                .enumerate()
                .filter_map(|(i, m)| Some((m["id"].as_str()?.to_owned(), i)))
                .collect();
            let mut known: HashSet<String> = HashSet::with_capacity(self.items.len());
            for old in &mut self.items {
                let Some(id) = old["id"].as_str().map(str::to_owned) else { continue };
                if let Some(&i) = fresh.get(&id) {
                    if *old != incoming[i] {
                        *old = incoming[i].clone();
                        self.layout.remove(&id);
                    }
                }
                known.insert(id);
            }
            incoming.retain(|m| !m["id"].as_str().is_some_and(|id| known.contains(id)));
            if self.append_older {
                incoming.append(&mut self.items);
            } else {
                self.items.append(&mut incoming);
                incoming = std::mem::take(&mut self.items);
            }
        }
        // Cross-host backlog may arrive behind the visible tail. Stable event
        // ordering keeps every replica's timeline consistent and selection by ID.
        incoming.sort_by(|a, b| {
            let clock = |e: &Value| {
                e["logical_time"]
                    .as_str()
                    .and_then(|n| n.parse::<u64>().ok())
                    .unwrap_or(0)
            };
            clock(a)
                .cmp(&clock(b))
                .then_with(|| {
                    a["origin_daemon_id"]
                        .as_str()
                        .cmp(&b["origin_daemon_id"].as_str())
                })
                .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
        });
        self.items = incoming;
        if self.target["mentions"] == true {
            // The mention list reads newest first.
            self.items.reverse();
        }
        if let Some(pins) = value["pins"].as_object() {
            for item in &mut self.items {
                let id = item["id"].as_str().unwrap_or("").to_owned();
                let pinned = json!(pins.contains_key(&id));
                if item["pinned"] != pinned {
                    self.layout.remove(&id);
                }
                item["pinned"] = pinned;
                item["pin"] = pins.get(&id).cloned().unwrap_or(Value::Null);
            }
        }
        // Older history landed above: keep the view on what it showed.
        self.prepended = if same && self.append_older {
            first_before
                .and_then(|id| self.items.iter().position(|m| m["id"] == id))
                .unwrap_or(0)
        } else {
            0
        };
        self.selected = if at_bottom && !self.append_older {
            self.items.len().saturating_sub(1)
        } else {
            old.as_ref()
                .and_then(|id| self.items.iter().position(|m| m["id"] == *id))
                .unwrap_or(self.items.len().saturating_sub(1))
        };
        if self.target["mentions"] == true && !same {
            self.selected = 0;
        }
        self.reveal_selection |= !same
            || self.select_older
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
                if ids.contains(&item["id"]) && item["read"] != true {
                    item["read"] = json!(true);
                    self.layout.remove(item["id"].as_str().unwrap_or(""));
                }
            }
        }
        self.receipt = value["receipt"].clone();
        self.management.last_position = value["position"].clone();
    }
    /// One message's rows (without the blank separator).
    pub(super) fn message_lines(&self, m: &Value, selected: bool, width: usize) -> Vec<Line<'static>> {
        let muted = Style::default().fg(theme::CHAT_MUTED);
        let mut lines = Vec::new();
        let bg = if selected {
            theme::CHAT_SELECTION
        } else {
            theme::CHAT_PANEL
        };
        let (marker, marker_style) = self.unread_marker(m);
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
                    self.actor_name(m),
                    chat_actor_style(m).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  {stamp}"), muted),
                Span::styled(
                    match m["replication"]["status"].as_str() {
                        Some("pending_sync") => {
                            if self.sync["connected"] == false {
                                "  saved · offline"
                            } else {
                                "  syncing"
                            }
                        }
                        Some("replication_rejected") => "  sync rejected",
                        Some("replication_blocked") => "  sync blocked",
                        _ => "",
                    },
                    Style::default().fg(theme::CHAT_TAG),
                ),
                Span::styled(
                    if m["replication"]["receipt"]["accepted_from_stale_revision"] == true {
                        "  delayed · pre-archive"
                    } else {
                        ""
                    },
                    muted,
                ),
                Span::styled(
                    if m["pinned"] == true {
                        "  ◆ pinned"
                    } else {
                        ""
                    },
                    Style::default().fg(theme::CHAT_TAG),
                ),
            ])
            .style(Style::default().bg(bg)),
        );
        if m["replication"]["status"] == "replication_rejected" {
            lines.push(Line::styled(
                format!(
                    "  Retained locally: {}",
                    m["replication"]["decision"]["reason"]
                        .as_str()
                        .unwrap_or("hub rejected this upload")
                ),
                Style::default().fg(theme::ERROR).bg(bg),
            ));
        }
        if self.target["inbox"] == true
            || self.target["dms"] == true
            || self.target["mentions"] == true
            || self.target["channel"] == "*"
        {
            lines.push(Line::styled(
                self.conversation_label(&m["conversation_id"]),
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
        // Reactions: `✅ Alpha, Owner  👀 Beta`, Owner's own in bold.
        if let Some(reactions) = m["reactions"].as_object().filter(|r| !r.is_empty()) {
            let mut spans = vec![Span::styled("  ", Style::default().bg(bg))];
            for (emoji, r) in reactions {
                let names = super::reaction_names(r);
                let count = r["count"].as_u64().map_or(names.len(), |c| c as usize);
                if count == 0 {
                    continue;
                }
                let mine = super::reacted_by_me(m, emoji);
                let style = if mine {
                    Style::default().fg(theme::CHAT_OWNER).add_modifier(Modifier::BOLD)
                } else {
                    muted
                };
                let list = if count > 3 && names.len() >= 3 {
                    format!("{} +{}", names[..3].join(", "), count - 3)
                } else if names.is_empty() {
                    count.to_string()
                } else {
                    names.join(", ")
                };
                spans.push(Span::styled(format!("{emoji} {list}  "), style.bg(bg)));
            }
            if spans.len() > 1 {
                lines.push(Line::from(spans).style(Style::default().bg(bg)));
            }
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
        lines
    }
    /// Apply a `messaging.react` reply to the loaded message.
    pub(super) fn accept_reaction(&mut self, v: &Value) {
        let Some(item) = self.items.iter_mut().find(|m| m["id"] == v["message_id"]) else {
            return;
        };
        if v["reactions"].is_object() {
            item["reactions"] = v["reactions"].clone();
        }
        let has_mine = v["reactions"].as_object().is_some_and(|r| r.values().any(|e| e["mine"].is_boolean()));
        if v["reactions_mine"].is_array() {
            item["reactions_mine"] = v["reactions_mine"].clone();
        } else if has_mine {
            // The aggregate carries `mine` per emoji.
        } else if let Some(emoji) = self.management.request["emoji"].as_str() {
            // Older replies omit the caller's set: track our own toggle.
            let mut mine: Vec<Value> = item["reactions_mine"].as_array().cloned().unwrap_or_default();
            mine.retain(|e| e != emoji);
            if self.management.request["remove"] != true {
                mine.push(json!(emoji));
            }
            item["reactions_mine"] = json!(mine);
        }
        let id = item["id"].as_str().unwrap_or("").to_owned();
        self.layout.remove(&id);
        self.layout_epoch += 1;
        self.status = "Reaction saved".into();
    }
    fn unread_marker(&self, m: &Value) -> (&'static str, Style) {
        let muted = Style::default().fg(theme::CHAT_MUTED);
        if m["read"] == true || m["actor"]["id"] == "owner" {
            return ("", muted);
        }
        let mentioned = m["data"].get("mention_recipients").unwrap_or(&m["data"]["mentions"])
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
        let area = self.draw_channel_summary(frame, area, focused);
        let inner = area.inner(ratatui::layout::Margin::new(1, 1));
        let width = inner.width.max(1) as usize;
        if self.messages.timeline_size != (inner.width, inner.height) {
            self.messages.reveal_selection = true;
            self.messages.timeline_size = (inner.width, inner.height);
        }
        let muted = Style::default().fg(theme::CHAT_MUTED);
        // Each message's rows are cached per width; only the selected one
        // and messages still syncing are rebuilt per frame, and only the
        // visible rows are handed to the widget.
        let n = self.messages.items.len();
        let key = (self.messages.layout_epoch, inner.width, n);
        let cached_heights = {
            let (epoch, w, len, _) = &self.messages.row_heights;
            (*epoch, *w, *len) == key
        };
        let mut heights = if cached_heights {
            std::mem::take(&mut self.messages.row_heights.3)
        } else {
            Vec::with_capacity(n)
        };
        let mut volatile = false;
        for i in 0..n {
            if cached_heights {
                break;
            }
            let m = &self.messages.items[i];
            let id = m["id"].as_str().unwrap_or("");
            let cacheable = !id.is_empty() && m["replication"]["status"] != "pending_sync";
            volatile |= !cacheable;
            let cached = cacheable
                .then(|| self.messages.layout.get(id).filter(|(w, _)| *w == inner.width).map(|(_, l)| l.len()))
                .flatten();
            let height = match cached {
                Some(h) => h,
                None => {
                    let lines = self.messages.message_lines(m, false, width);
                    self.messages.layout_builds += 1;
                    let h = lines.len();
                    if cacheable {
                        let id = id.to_owned();
                        self.messages.layout.insert(id, (inner.width, lines));
                    }
                    h
                }
            };
            heights.push(height + 1);
        }
        let mut offsets = Vec::with_capacity(n + 1);
        offsets.push(0usize);
        for h in &heights {
            offsets.push(offsets.last().unwrap() + h);
        }
        // Messages still syncing change shape; measure them every frame.
        self.messages.row_heights = if volatile { (0, 0, usize::MAX, Vec::new()) } else { (key.0, key.1, key.2, heights) };
        let total = *offsets.last().unwrap();
        let selected_range = if self.messages.selected < n {
            let i = self.messages.selected;
            (offsets[i], offsets[i + 1] - 1)
        } else {
            (0, 0)
        };
        let height = inner.height as usize;
        let prepended = std::mem::take(&mut self.messages.prepended).min(n);
        let scroll = &mut self.messages.timeline_scroll;
        *scroll += offsets[prepended];
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
        *scroll = (*scroll).min(total.saturating_sub(height));
        let scroll = *scroll;
        // Within a screen of the oldest loaded message: fetch more history.
        self.messages.near_top = n > 0 && scroll < height && !self.messages.next.is_null();
        let selected_top = selected_range.0.saturating_sub(scroll).min(height);
        let selected_bottom = selected_range.1.saturating_sub(scroll).min(height);
        let first = offsets.partition_point(|&o| o <= scroll).saturating_sub(1);
        let mut visible: Vec<Line<'static>> = Vec::with_capacity(height);
        let mut skip = scroll.saturating_sub(offsets.get(first).copied().unwrap_or(0));
        for i in first..n {
            if visible.len() >= height {
                break;
            }
            let m = &self.messages.items[i];
            let rows: Vec<Line<'static>> = if i == self.messages.selected {
                self.messages.layout_builds += 1;
                self.messages.message_lines(m, true, width)
            } else {
                match self.messages.layout.get(m["id"].as_str().unwrap_or("")).filter(|(w, _)| *w == inner.width) {
                    Some((_, lines)) => lines.clone(),
                    None => self.messages.message_lines(m, false, width),
                }
            };
            for line in rows.into_iter().chain(std::iter::once(Line::from(""))) {
                if skip > 0 {
                    skip -= 1;
                } else if visible.len() < height {
                    visible.push(line);
                }
            }
        }
        if n == 0 {
            visible.push(Line::styled(
                "No messages yet. c composes · d starts a DM.",
                muted,
            ));
        }
        let title = format!("{}{} · j/k select · Enter read", self.messages.target_label(),
            if self.messages.filter["pinned_only"] == true { " · Pinned messages" } else { "" });
        frame.render_widget(
            Paragraph::new(visible)
                .style(Style::default().bg(theme::CHAT_PANEL))
                .block(chat_block(title, focused)),
            area,
        );
        // Paragraph line styles stop at the final glyph. Fill the selected
        // message's visible rows too, including empty lines and trailing space.
        frame.buffer_mut().set_style(
            Rect::new(
                inner.x,
                inner.y + selected_top as u16,
                inner.width,
                (selected_bottom - selected_top) as u16,
            ),
            Style::default().bg(theme::CHAT_SELECTION),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn message(id: &str, body: &str, kind: &str, mentions: Value) -> Value {
        json!({"id":id,"body":body,"conversation_id":"chat","conversation_kind":kind,"read":false,
            "actor":{"id":"agent:a","name":"Scout"},"data":{"mentions":mentions},"created_at":"2026-09-07T12:00:00Z","logical_time":match id {"before"=>"0","zero"=>"1","one"=>"2","two"=>"3",_=>"4"},"origin_daemon_id":"fixture"})
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
        a.messages.people = vec![json!({"id":"agent:a","name":"health-triage-orchestrator","aliases":["Scout"]})];
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
        assert!(text.contains("health-triage-orchestrator"));
        assert!(!text.contains("Scout"));
        assert_eq!(a.messages.items[0]["actor"]["name"], "Scout"); // immutable history
        assert_eq!(a.messages.actor_name(&json!({"actor":{"id":"absent","name":"Historical"}})), "Historical");
        a.messages.text = "Scout".into();
        assert_eq!(a.messages.picker_people()[0]["id"], "agent:a");
        a.messages.text.clear();
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
        a.messaging_event(&CrosstermEvent::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('k'),
            KeyModifiers::NONE,
        )));
        assert!(a.messages.select_older);
        a.messages.accept_messages(
            &json!({"items":[message("before", "Fetched by k", "channel", json!([]))]}),
        );
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
        let items = a.messages.menu_items();
        let row = items.iter().position(|(label, _)| label.contains("Alpha")).expect("DM row");
        assert_eq!(a.messages.count_columns().1[row].1, "●1 ", "the count sits in its own column");
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
