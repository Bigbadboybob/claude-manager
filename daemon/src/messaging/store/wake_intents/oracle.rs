// Frozen pre-P4 evaluator: intentionally scans history and uses linear channel lookup.
use super::*;
impl Store {
    pub(super) fn wake_intents_oracle(&self, actor: &str) -> Vec<WakeIntent> {
        let mut intents = Vec::new();
        let personal = self.personal.get(actor);
        for cid in self
            .read_model
            .conversations
            .keys()
            .filter(|cid| self.visible_oracle(actor, cid))
        {
            for e in self.message_candidates(actor, cid, None, false, true, false) {
                self.event_wake_intents_oracle(actor, personal, e, &mut intents);
            }
        }
        intents.sort_by_key(|intent| {
            self.published(&intent.event_id)
                .map(|e| e.position)
                .unwrap_or(0)
        });
        intents
    }

    pub(super) fn event_wake_intents_oracle(
        &self,
        actor: &str,
        personal: Option<&Personal>,
        e: &Published,
        intents: &mut Vec<WakeIntent>,
    ) {
        tests::count_oracle();
        if e.event["type"] != "message.create"
            || self
                .replication
                .rejections
                .contains_key(strv(&e.event, "id"))
        {
            return;
        }
        let (_, wake, muted) = self.preference_for_event_oracle(actor, e);
        let id = strv(&e.event, "id");
        if wake && !self.is_read(actor, &e.event) {
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
                && !self.is_read(actor, &e.event)
                && e.position > m.acknowledged
                && e.position <= m.hit_high
                && self.monitor_matches_oracle(actor, m, e)
            {
                intents.push(WakeIntent {
                    key: format!("monitor:{}:{id}", m.id),
                    event_id: id.into(),
                    monitor: Some(m.id.clone()),
                });
            }
        }
    }
    pub(super) fn preference_for_event_oracle(
        &self,
        actor: &str,
        event: &Published,
    ) -> (bool, bool, bool) {
        let v = &event.event;
        if v["type"] != "message.create" || !self.visible_oracle(actor, strv(v, "conversation_id"))
        {
            return (false, false, false);
        }
        let dm = self.conversations.contains_key(strv(v, "conversation_id"));
        let mention = mention_recipients(v).contains(&actor);
        let mut inbox = dm || mention;
        let mut wake = actor != "owner" && inbox;
        let mut muted = false;
        if let Some(state) = self.personal.get(actor) {
            let prefs = &state.preferences;
            muted = prefs.dnd;
            let mut rules: Vec<_> = prefs
                .rules
                .values()
                .filter(|r| self.scope_matches_oracle(actor, &r.scope, v))
                .collect();
            rules.sort_by_key(|r| r.scope.specificity());
            for rule in rules {
                muted |= rule.muted;
                // New follows start at their commit boundary; enabling a
                // channel never floods an inbox with its entire old history.
                let later = rule
                    .hub_since
                    .and_then(|boundary| {
                        self.replication
                            .receipts
                            .get(strv(v, "id"))
                            .and_then(|r| r["position"].as_str())
                            .and_then(|p| p.parse::<u64>().ok())
                            .map(|p| p > boundary)
                    })
                    .unwrap_or(event.position > rule.since);
                if later || !rule.inbox {
                    inbox = rule.inbox;
                }
                if event.position > rule.wake_since.unwrap_or(rule.since) || !rule.wake {
                    wake = rule.wake;
                }
            }
        }
        (
            inbox && v["actor"]["id"] != actor,
            wake && !muted && v["actor"]["id"] != actor,
            muted,
        )
    }
    pub(super) fn scope_matches_oracle(&self, actor: &str, scope: &Scope, event: &Value) -> bool {
        let cid = strv(event, "conversation_id");
        if !self.visible_oracle(actor, cid) {
            return false;
        }
        if let Some(path) = &scope.channel {
            let actual = self
                .channels
                .iter()
                .find_map(|(p, id)| (id == cid).then_some(p));
            if !actual.is_some_and(|actual| {
                path == "*"
                    || actual == path
                    || scope.include_children && actual.starts_with(&format!("{path}/"))
            }) {
                return false;
            }
        }
        if scope.conversation.as_deref().is_some_and(|id| id != cid) {
            return false;
        }
        if scope.dms && !self.conversations.contains_key(cid) {
            return false;
        }
        if let Some(peer) = &scope.peer {
            if !self.conversations.get(cid).is_some_and(|members| {
                members.len() == 2 && members.contains(peer) && members.iter().any(|m| m == actor)
            }) {
                return false;
            }
        }
        if let Some(root) = &scope.thread {
            if event["id"] != *root && event["data"]["thread_root"] != *root {
                return false;
            }
        }
        true
    }

    pub(super) fn monitor_matches_oracle(
        &self,
        actor: &str,
        monitor: &Monitor,
        e: &Published,
    ) -> bool {
        e.event["type"] == "message.create"
            && self.task_monitor_matches(actor, monitor, strv(&e.event, "id"))
            && (monitor.include_self || e.event["actor"]["id"] != actor)
            && self.scope_matches_oracle(actor, &monitor.scope, &e.event)
    }

    fn visible_oracle(&self, actor: &str, conv: &str) -> bool {
        self.channels.values().any(|c| c == conv)
            || self
                .conversations
                .get(conv)
                .is_some_and(|m| m.iter().any(|p| p == actor))
    }
}
