use super::*;

#[derive(Clone, Debug)]
pub struct WakeIntent {
    pub key: String,
    pub event_id: String,
    pub monitor: Option<String>,
}

impl Store {
    pub(super) fn load_known_reads(&mut self) -> Result<()> {
        let mut actors: BTreeSet<_> = self
            .names
            .keys()
            .chain(self.personal.keys())
            .cloned()
            .collect();
        actors.insert("owner".into());
        actors.extend(self.memberships.values().flatten().cloned());
        for members in self.conversations.values() {
            actors.extend(members.iter().cloned());
        }
        for e in &self.events {
            actors.extend(
                mention_recipients(&e.event).into_iter()
                    .map(str::to_owned),
            );
        }
        for actor in actors {
            self.load_read(&actor)?;
        }
        Ok(())
    }
    pub fn wake_intents(&self) -> BTreeMap<String, Vec<WakeIntent>> {
        let mut actors: BTreeSet<String> = self
            .names
            .keys()
            .chain(self.personal.keys())
            .cloned()
            .collect();
        for members in self.conversations.values() {
            actors.extend(members.iter().cloned());
        }
        for e in &self.events {
            actors.extend(
                mention_recipients(&e.event).into_iter()
                    .map(str::to_owned),
            );
        }
        let mut out = BTreeMap::new();
        for actor in actors {
            // Owner is always passive. The optional TUI bell is handled by a
            // persisted badge claim, never by injecting into an agent session.
            if actor == "owner" {
                continue;
            }
            let Some(uid) = actor.strip_prefix(&format!("agent:{}:", self.daemon_id)) else {
                continue;
            };
            out.insert(uid.to_owned(), self.wake_intents_for_actor(&actor));
        }
        out
    }

    /// Evaluate exactly one local recipient. Delivery must never rescan all
    /// historical participants once per recipient while holding the chat lock.
    pub fn wake_intents_for_session(&self, uid: &str) -> Vec<WakeIntent> {
        self.wake_intents_for_actor(&self.participant_id(uid))
    }

    fn wake_intents_for_actor(&self, actor: &str) -> Vec<WakeIntent> {
        let mut intents = Vec::new();
        let personal = self.personal.get(actor);
        for e in self
            .events
            .iter()
            .filter(|e| e.event["type"] == "message.create" && !self.replication.rejections.contains_key(strv(&e.event, "id")))
        {
            let (_, wake, muted) = self.preference_for_event(actor, e);
            let id = strv(&e.event, "id");
            if wake && !self.reads.get(actor).is_some_and(|r| r.ids.contains(id)) {
                intents.push(WakeIntent {
                    key: id.into(),
                    event_id: id.into(),
                    monitor: None,
                });
            }
            if muted {
                continue;
            }
            for m in personal.into_iter().flat_map(|p| p.monitors.values()) {
                if m.notify == "wake"
                    && !["cancelled", "dismissed"].contains(&m.state.as_str())
                    && e.position > m.acknowledged
                    && e.position <= m.hit_high
                    && self.monitor_matches(actor, m, e)
                {
                    intents.push(WakeIntent {
                        key: format!("monitor:{}:{id}", m.id),
                        event_id: id.into(),
                        monitor: Some(m.id.clone()),
                    });
                }
            }
        }
        intents
    }

}

impl Store {
    /// Passive Owner counts. A bell claim is at-most-once across TUI restarts:
    /// persist its boundaries before returning the optional terminal signal.
    pub fn attention(&mut self, actor: &str, claim: bool) -> Result<Value> {
        if actor != "owner" {
            return Ok(Value::Null);
        }
        self.load_read(actor)?;
        let mut state = self.personal_state(actor);
        let inbox: Vec<_> = self
            .events
            .iter()
            .filter(|e| {
                self.preference_for_event(actor, e).0
                    && !self.reads[actor].ids.contains(strv(&e.event, "id"))
            })
            .collect();
        let unread = inbox.len();
        let mut ring = inbox
            .iter()
            .any(|e| e.position > state.bell_position && !self.preference_for_event(actor, e).2);
        for m in state
            .monitors
            .values()
            .filter(|m| m.notify != "none" && m.state != "dismissed")
        {
            let seen = state
                .bell_monitors
                .get(&m.id)
                .copied()
                .unwrap_or(m.start)
                .max(m.acknowledged);
            ring |= self.events.iter().any(|e| {
                e.position > seen
                    && e.position <= m.hit_high
                    && self.monitor_matches(actor, m, e)
                    && !self.preference_for_event(actor, e).2
            });
        }
        ring &= state.preferences.bell && !state.preferences.dnd;
        if claim && state.preferences.bell {
            state.bell_position = self.position;
            for m in state.monitors.values() {
                state.bell_monitors.insert(m.id.clone(), m.hit_high);
            }
            self.save_personal(state)?;
        }
        Ok(
            json!({"unread":unread,"ring":claim && ring,"monitor_badges":self.monitor_status(actor)["badges"]}),
        )
    }
}
