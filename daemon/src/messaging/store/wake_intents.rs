use super::preferences::Follow;
use super::watches::{Monitor, Scope};
use super::*;

#[cfg(test)]
mod oracle;
#[cfg(test)]
mod tests;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WakeIntent {
    pub key: String,
    pub event_id: String,
    pub monitor: Option<String>,
}

/// Actor-wide decisions are prepared once, then narrowed by conversation.
struct WakeRules<'a> {
    rules: Vec<&'a Follow>,
    monitors: Vec<WakeMonitor<'a>>,
}

struct WakeMonitor<'a> {
    monitor: &'a Monitor,
    acknowledged: Option<&'a BTreeSet<String>>,
}

impl<'a> WakeRules<'a> {
    fn new(store: &'a Store, actor: &str, personal: Option<&'a Personal>) -> Self {
        let mut rules: Vec<_> = personal
            .into_iter()
            .flat_map(|p| p.preferences.rules.values())
            .collect();
        // Stable ordering retains the BTreeMap tie-break used by preferences.
        rules.sort_by_key(|r| r.scope.specificity());
        let monitors = personal
            .into_iter()
            .flat_map(|p| p.monitors.values())
            .filter(|m| {
                m.notify == "wake"
                    && store.task_monitor_current(actor, m)
                    && !["cancelled", "dismissed"].contains(&m.state.as_str())
                    && m.hit_high > m.acknowledged
            })
            .filter_map(|monitor| {
                let acknowledged = if let Some(task) = &monitor.task_subscription {
                    let b = store.task_bindings.get(task)?;
                    if !b.active
                        || b.actor != actor
                        || monitor.id != format!("task-{}-{}", b.id, b.revision)
                    {
                        return None;
                    }
                    Some(&b.acknowledged)
                } else {
                    None
                };
                Some(WakeMonitor {
                    monitor,
                    acknowledged,
                })
            })
            .collect();
        Self { rules, monitors }
    }

    fn conversation(&self, store: &Store, actor: &str, cid: &str) -> ConversationWake<'_> {
        ConversationWake {
            rules: self
                .rules
                .iter()
                .copied()
                .filter(|r| store.scope_matches_conversation(actor, &r.scope, cid))
                .collect(),
            monitors: self
                .monitors
                .iter()
                .filter(|m| store.scope_matches_conversation(actor, &m.monitor.scope, cid))
                .collect(),
        }
    }
}

struct ConversationWake<'a> {
    rules: Vec<&'a Follow>,
    monitors: Vec<&'a WakeMonitor<'a>>,
}

fn thread_matches(scope: &Scope, id: &str, root: Option<&str>) -> bool {
    scope
        .thread
        .as_deref()
        .is_none_or(|wanted| wanted == id || root == Some(wanted))
}

impl ConversationWake<'_> {
    fn evaluate(
        &self,
        store: &Store,
        actor: &str,
        e: &Published,
        read: Option<&ReadCursor>,
        default_wake: bool,
        intents: &mut Vec<WakeIntent>,
    ) {
        #[cfg(test)]
        tests::count_candidate();
        // Each candidate is decoded/read-checked once, regardless of how many
        // follows or monitors contributed it. Own messages are always read.
        let v = &e.event;
        let id = strv(v, "id");
        if v["actor"]["id"] == actor
            || store.replication.rejections.contains_key(id)
            || read.is_some_and(|r| {
                (strv(v, "logical_time").parse::<u64>().unwrap_or(0), id)
                    <= (r.logical_time, r.event_id.as_str())
            })
        {
            return;
        }
        let root = v["data"]["thread_root"].as_str();
        let mut wake = default_wake;
        for rule in &self.rules {
            if thread_matches(&rule.scope, id, root) {
                // Mutes are sticky across all matching rules, independent of
                // follow boundaries. DND is handled before candidate selection.
                if rule.muted {
                    return;
                }
                if e.position > rule.wake_since.unwrap_or(rule.since) || !rule.wake {
                    wake = rule.wake;
                }
            }
        }
        if wake {
            intents.push(WakeIntent {
                key: id.into(),
                event_id: id.into(),
                monitor: None,
            });
        }
        for eligible in &self.monitors {
            let m = eligible.monitor;
            if e.position > m.acknowledged
                && e.position <= m.hit_high
                && thread_matches(&m.scope, id, root)
                && !eligible.acknowledged.is_some_and(|ids| ids.contains(id))
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
    /// Shared passive Owner inbox decision; the terminal bell and Web Push
    /// retain their own opt-in transports without duplicating inbox/mute rules.
    fn owner_inbox_notification(&self, e: &Published) -> bool {
        let (inbox, _, muted) = self.preference_for_event("owner", e);
        inbox && !muted && !self.is_read("owner", &e.event)
    }

    pub(super) fn owner_message_notification(&self, e: &Published) -> Option<crate::owner_notifications::Notification> {
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
        let place = self.channel_path_of(cid).map(|p| format!("#{p}")).unwrap_or_else(|| "DM".into());
        let sender = self.names.get(strv(&e.event["actor"], "id"))
            .map(|n| n.name.as_str()).unwrap_or_else(|| strv(&e.event["actor"], "name"));
        Some(crate::owner_notifications::Notification {
            sequence: 0, id: format!("cm-message:{}", strv(&e.event,"id")),
            created_at: 0, // Feed stamps the committed notification decision.
            title: format!("{sender} · {place}"), body: strv(&e.event,"body").into(),
            url: format!("/#conversation={}&message={}", crate::owner_notifications::url_component(cid), crate::owner_notifications::url_component(strv(&e.event,"id"))),
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
        let personal = self.personal.get(actor);
        if personal.is_some_and(|p| p.preferences.dnd) {
            return Vec::new();
        }
        let plan = WakeRules::new(self, actor, personal);
        let mut ordered = Vec::new();
        for (cid, index) in &self.read_model.conversations {
            if !self.visible(actor, cid) {
                continue;
            }
            let read = self.reads.get(actor).and_then(|r| r.cursors.get(cid));
            let unread_lo = read.map_or(0, |r| index.messages.partition_point(|(k, _)| k <= r));
            let unread = &index.messages[unread_lo..];
            let conversation = plan.conversation(self, actor, cid);
            // Union default unread DM/mention candidates with the local arrival
            // intervals that can enable a follow or retained monitor hit. The
            // bool retains default eligibility when ranges overlap. Threads are
            // conservatively selected by conversation, then checked once below.
            let mut candidates = BTreeMap::new();
            if actor != "owner" {
                if self.conversations.contains_key(cid) {
                    candidates.extend(unread.iter().map(|(_, i)| (*i, true)));
                } else if let Some(mentions) = index.mentions.get(actor) {
                    let lo = read.map_or(0, |r| mentions.partition_point(|k| k <= r));
                    candidates.extend(
                        mentions[lo..]
                            .iter()
                            .filter_map(|k| self.event_index.get(&k.event_id).map(|i| (*i, true))),
                    );
                }
            }
            // Coalesce overlapping follow/monitor intervals before visiting
            // arrivals: 100 watches on one channel should not rescan it 100x.
            let mut ranges: Vec<_> = conversation
                .rules
                .iter()
                .filter(|r| r.wake)
                .map(|r| (r.wake_since.unwrap_or(r.since), self.position))
                .chain(
                    conversation
                        .monitors
                        .iter()
                        .map(|m| (m.monitor.acknowledged, m.monitor.hit_high)),
                )
                .filter(|(lo, hi)| lo < hi)
                .collect();
            ranges.sort_unstable();
            let mut through = 0;
            for (after, high) in ranges {
                let after = after.max(through);
                if after >= high {
                    continue;
                }
                let lo = index.arrivals.partition_point(|(p, _)| *p <= after);
                let hi = index.arrivals.partition_point(|(p, _)| *p <= high);
                // Use the narrower index when most of a followed channel is
                // already read. Arrival order and read order can differ after
                // replication, so neither boundary substitutes for the other.
                if unread.len() < hi - lo {
                    for (_, i) in unread {
                        let position = self.events[*i].position;
                        if position > after && position <= high {
                            candidates.entry(*i).or_insert(false);
                        }
                    }
                } else {
                    for (_, i) in &index.arrivals[lo..hi] {
                        candidates.entry(*i).or_insert(false);
                    }
                }
                through = high;
            }
            for (i, default_wake) in candidates {
                let e = &self.events[i];
                let mut intents = Vec::new();
                conversation.evaluate(self, actor, e, read, default_wake, &mut intents);
                ordered.extend(intents.into_iter().map(|intent| (e.position, intent)));
            }
        }
        // Stable sort: direct intent before monitors (in monitor ID order) for
        // each event; publication order across conversations and late arrivals.
        ordered.sort_by_key(|(position, _)| *position);
        ordered.into_iter().map(|(_, intent)| intent).collect()
    }

    fn event_wake_intents(
        &self,
        actor: &str,
        personal: Option<&Personal>,
        e: &Published,
        intents: &mut Vec<WakeIntent>,
    ) {
        let cid = strv(&e.event, "conversation_id");
        if e.event["type"] != "message.create"
            || !self.visible(actor, cid)
            || personal.is_some_and(|p| p.preferences.dnd)
        {
            return;
        }
        let plan = WakeRules::new(self, actor, personal);
        let default_wake = actor != "owner"
            && (self.conversations.contains_key(cid)
                || mention_recipients(&e.event).contains(&actor));
        plan.conversation(self, actor, cid).evaluate(
            self,
            actor,
            e,
            self.reads.get(actor).and_then(|r| r.cursors.get(cid)),
            default_wake,
            intents,
        );
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
