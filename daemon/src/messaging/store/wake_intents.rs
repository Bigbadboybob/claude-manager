use super::*;

#[derive(Clone, Debug)]
pub struct WakeIntent {
    pub key: String,
    pub event_id: String,
    pub monitor: Option<String>,
}

impl Store {
    /// Shared passive Owner inbox decision; the terminal bell and Web Push
    /// retain their own opt-in transports without duplicating inbox/mute rules.
    fn owner_inbox_notification(&self, e: &Published) -> bool {
        let (inbox, _, muted) = self.preference_for_event("owner", e);
        inbox && !muted && !self.is_read("owner", &e.event)
    }

    pub(super) fn owner_message_notification(&self, e: &Published) -> Option<crate::owner_notifications::Notification> {
        use crate::owner_availability::{self, Level, Urgency};
        if self.owner_notifications.is_none() || self.sync_enabled() && !self.is_coordinator()
            || !self.owner_inbox_notification(e) {
            return None;
        }
        let cid = strv(&e.event, "conversation_id");
        // B5 pushes direct/group DMs and explicit/frozen @here mentions;
        // following all traffic in another channel is not an implicit push opt-in.
        if !self.conversations.contains_key(cid) && !mention_recipients(&e.event).contains(&"owner") {
            return None;
        }
        let level = self.owner_availability["level"].as_str().and_then(Level::parse);
        if !owner_availability::delivers(level, Urgency::Fyi) { return None; }
        let place = self.channel_path_of(cid).map(|p| format!("#{p}")).unwrap_or_else(|| "DM".into());
        let sender = self.names.get(strv(&e.event["actor"], "id"))
            .map(|n| n.name.as_str()).unwrap_or_else(|| strv(&e.event["actor"], "name"));
        Some(crate::owner_notifications::Notification {
            sequence: 0, id: format!("cm-message:{}", strv(&e.event,"id")),
            created_at: 0, // Feed stamps the committed notification decision.
            title: format!("{sender} · {place}"), body: strv(&e.event,"body").into(),
            url: format!("/#conversation={cid}&message={}", strv(&e.event,"id").replace(':', "%3A")),
        })
    }

    pub(super) fn publish_owner_notification(&self, n: crate::owner_notifications::Notification) {
        if let Some(feed) = &self.owner_notifications {
            if let Err(e) = feed.publish(self.cm_root(), n.id, &n.title, &n.body, n.url) {
                eprintln!("cm Owner message notification checkpoint failed: {e}");
            }
        }
    }

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
        if self.sync_enabled() && !self.messaging_frozen()
            && (self.is_coordinator() || self.replication.hosts.get(&self.daemon_id).is_some_and(|h|h["owner_access"]==true)) {
            self.retain_legacy_owner_reads()?;
        }
        Ok(())
    }
    /// Local agent recipients (actor, session uid). Owner is always passive:
    /// the optional TUI bell is a persisted badge claim, never an injection.
    fn wake_recipients(&self) -> Vec<(String, String)> {
        let mut actors: BTreeSet<String> = self
            .names
            .keys()
            .chain(self.personal.keys())
            .cloned()
            .collect();
        for members in self.conversations.values() {
            actors.extend(members.iter().cloned());
        }
        for conv in self.read_model.conversations.values() {
            actors.extend(conv.mentions.keys().cloned());
        }
        let prefix = format!("agent:{}:", self.daemon_id);
        actors
            .into_iter()
            .filter_map(|actor| {
                let uid = actor.strip_prefix(&prefix)?.to_owned();
                Some((actor, uid))
            })
            .collect()
    }
    pub fn wake_intents(&self) -> BTreeMap<String, Vec<WakeIntent>> {
        self.wake_recipients()
            .into_iter()
            .map(|(actor, uid)| (uid, self.wake_intents_for_actor(&actor)))
            .collect()
    }

    /// Intents for one event across all local recipients. A message's
    /// delivery status needs only this; evaluating every recipient against
    /// the whole history took seconds per displayed message.
    pub(super) fn wake_intents_for_event(&self, event_id: &str) -> Vec<(String, WakeIntent)> {
        let Some(e) = self.published(event_id) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (actor, uid) in self.wake_recipients() {
            let mut intents = Vec::new();
            self.event_wake_intents(&actor, self.personal.get(&actor), e, &mut intents);
            out.extend(intents.into_iter().map(|i| (uid.clone(), i)));
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
        for cid in self.read_model.conversations.keys().filter(|cid|self.visible(actor,cid)) {
            for e in self.message_candidates(actor,cid,None,false,true,false) {
                self.event_wake_intents(actor,personal,e,&mut intents);
            }
        }
        intents.sort_by_key(|intent| self.published(&intent.event_id).map(|e|e.position).unwrap_or(0));
        intents
    }

    fn event_wake_intents(
        &self,
        actor: &str,
        personal: Option<&Personal>,
        e: &Published,
        intents: &mut Vec<WakeIntent>,
    ) {
        if e.event["type"] != "message.create"
            || self.replication.rejections.contains_key(strv(&e.event, "id"))
        {
            return;
        }
        let (_, wake, muted) = self.preference_for_event(actor, e);
        let id = strv(&e.event, "id");
        if wake && !self.is_read(actor,&e.event) {
            intents.push(WakeIntent {
                key: id.into(),
                event_id: id.into(),
                monitor: None,
            });
        }
        if muted {
            return;
        }
        for m in personal.into_iter().flat_map(|p| p.monitors.values()) {
            if m.notify == "wake"
                && !["cancelled", "dismissed"].contains(&m.state.as_str())
                && !self.is_read(actor,&e.event)
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

}

impl Store {
    /// Passive Owner counts. A bell claim is at-most-once across TUI restarts:
    /// persist its boundaries before returning the optional terminal signal.
    pub fn attention(&mut self, actor: &str, claim: bool) -> Result<Value> {
        if actor != "owner" {
            return Ok(Value::Null);
        }
        self.load_read(actor)?;
        if !claim {
            let counts = self.counts(actor)?;
            return Ok(json!({"unread":counts["unread"],"dms":counts["dms"],"mentions":counts["mentions"],"ring":false,"monitor_badges":self.monitor_status(actor)["badges"]}));
        }
        let mut state = self.personal_state(actor);
        let counts = self.counts(actor)?;
        let inbox: Vec<_> = self.events_between(state.bell_position,self.position)
            .iter()
            .filter(|e| {
                self.owner_inbox_notification(e)
            })
            .collect();
        let unread = counts["unread"].as_u64().unwrap_or(0);
        // Split for the viewer's status-bar indicator (same pass): unread
        // DMs (incl. group DMs) and unread messages that mention Owner
        // directly or through @here.
        let dms = counts["dms"].as_u64().unwrap_or(0);
        let mentions = counts["mentions"].as_u64().unwrap_or(0);
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
            ring |= self.events_between(seen,m.hit_high).iter().any(|e| {
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
            json!({"unread":unread,"dms":dms,"mentions":mentions,"ring":claim && ring,"monitor_badges":self.monitor_status(actor)["badges"]}),
        )
    }
}
