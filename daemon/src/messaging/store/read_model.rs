//! Conversation cursors use the same total order as history, independent of
//! replica arrival positions. Canonical events remain immutable.
use super::*;

#[derive(Clone, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub(super) struct ReadCursor {
    #[serde(with = "decimal")]
    pub logical_time: u64,
    pub event_id: String,
}
mod decimal {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(n: &u64, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&n.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<u64, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}
impl ReadCursor {
    pub fn of(e: &Value) -> Self {
        Self {
            logical_time: strv(e, "logical_time").parse().unwrap_or(0),
            event_id: strv(e, "id").into(),
        }
    }
}
#[derive(Default, Clone, Serialize, Deserialize)]
pub(super) struct ReadState {
    #[serde(default)]
    pub cursors: BTreeMap<String, ReadCursor>,
    #[serde(default)]
    pub legacy_pending: BTreeSet<String>,
}
#[derive(Default)]
pub(super) struct ConversationIndex {
    pub messages: Vec<(ReadCursor, usize)>,
    pub arrivals: Vec<u64>,
    pub timeline: Vec<(ReadCursor, usize)>,
    pub authors: BTreeMap<String, Vec<ReadCursor>>,
    pub mentions: BTreeMap<String, Vec<ReadCursor>>,
}
#[derive(Default)]
pub(super) struct ReadModel {
    pub conversations: BTreeMap<String, ConversationIndex>,
    pub metadata: Vec<usize>,
    pub pending_local: BTreeSet<usize>,
    pub pins: Vec<usize>,
    pub loaded: BTreeSet<String>,
    // Legacy ack events may arrive before their messages in sparse replicas.
    pub pending_legacy: BTreeMap<String, BTreeSet<String>>,
    pub owner_retained: BTreeMap<String, ReadCursor>,
    pub reactions: BTreeMap<String, BTreeMap<String, BTreeMap<String, (ReadCursor, bool, String)>>>,
}
fn insert<T: Ord>(v: &mut Vec<T>, value: T) {
    let at = v.partition_point(|old| old < &value);
    if v.get(at) != Some(&value) {
        v.insert(at, value);
    }
}
impl Store {
    pub(super) fn readable_event(event: &Value) -> bool {
        event["conversation_id"].is_string()
            && !matches!(
                strv(event, "type"),
                "conversation.pin" | "channel.membership" | "read.ack" | "read.cursor" | "reaction"
            )
    }
    pub(super) fn index_read_model(&mut self, e: &Published, index: usize) {
        if e.event["origin_daemon_id"] == self.daemon_id
            && !self.replication.receipts.contains_key(strv(&e.event, "id"))
            && !self
                .replication
                .rejections
                .contains_key(strv(&e.event, "id"))
        {
            self.read_model.pending_local.insert(index);
        }
        if Self::readable_event(&e.event) {
            insert(
                &mut self
                    .read_model
                    .conversations
                    .entry(strv(&e.event, "conversation_id").into())
                    .or_default()
                    .timeline,
                (ReadCursor::of(&e.event), index),
            );
        }
        if e.event["type"] == "message.create" {
            let key = ReadCursor::of(&e.event);
            let cid = strv(&e.event, "conversation_id").to_owned();
            let conv = self
                .read_model
                .conversations
                .entry(cid.clone())
                .or_default();
            insert(&mut conv.messages, (key.clone(), index));
            conv.arrivals.push(e.position);
            insert(
                conv.authors
                    .entry(strv(&e.event["actor"], "id").into())
                    .or_default(),
                key.clone(),
            );
            for actor in mention_recipients(&e.event) {
                if e.event["actor"]["id"] != actor {
                    insert(conv.mentions.entry(actor.into()).or_default(), key.clone());
                }
            }
            if let Some(actors) = self.read_model.pending_legacy.remove(strv(&e.event, "id")) {
                for actor in actors {
                    self.merge_cursor_memory(&actor, &cid, key.clone());
                }
            }
        } else if e.event["type"] == "conversation.pin" {
            self.read_model.pins.push(index);
        } else if !matches!(
            strv(&e.event, "type"),
            "read.ack" | "read.cursor" | "reaction"
        ) {
            self.read_model.metadata.push(index);
        }
    }
    fn merge_cursor_memory(&mut self, actor: &str, cid: &str, cursor: ReadCursor) -> bool {
        let current = self
            .reads
            .entry(actor.into())
            .or_default()
            .cursors
            .entry(cid.into())
            .or_default();
        if cursor > *current {
            *current = cursor;
            true
        } else {
            false
        }
    }
    pub(super) fn is_read(&self, actor: &str, event: &Value) -> bool {
        event["actor"]["id"] == actor
            || self
                .reads
                .get(actor)
                .and_then(|r| r.cursors.get(strv(event, "conversation_id")))
                .is_some_and(|cursor| ReadCursor::of(event) <= *cursor)
    }
    pub(super) fn read_cursor(&self, actor: &str, cid: &str) -> Value {
        json!(self.reads.get(actor).and_then(|r| r.cursors.get(cid)))
    }
    fn read_path(&self, actor: &str) -> PathBuf {
        self.root
            .join("_state/read-cursors")
            .join(format!("{}.json", hash(actor.as_bytes())))
    }
    fn save_read(&mut self, actor: &str) -> Result<()> {
        if let Some(reason) = &self.degraded {
            return Err(err("store_read_only", reason));
        }
        let pending: BTreeSet<_> = self
            .read_model
            .pending_legacy
            .iter()
            .filter(|(_, actors)| actors.contains(actor))
            .map(|(id, _)| id.clone())
            .collect();
        atomic_replace(
            &self.read_path(actor),
            &json!({"version":1,"actor":actor,"cursors":self.reads[actor].cursors,"legacy_pending":pending}),
        )
        .map_err(|e| {
            self.degraded = Some(format!("Read cursor outcome requires reconciliation: {e}"));
            err("outcome_unknown", self.degraded.clone().unwrap())
        })?;
        self.replication.signal.signal();
        Ok(())
    }
    pub(super) fn load_read(&mut self, actor: &str) -> Result<()> {
        if self.read_model.loaded.contains(actor) {
            return Ok(());
        }
        self.reads.entry(actor.into()).or_default();
        let path = self.read_path(actor);
        let mut migrated = false;
        if path.exists() {
            let v = load(&path)?;
            if v["version"] != 1 || v["actor"] != actor {
                return Err(err("invalid_record", "Invalid read cursor checkpoint"));
            }
            let state: ReadState = serde_json::from_value(v)?;
            self.merge_legacy_memory(actor, state.legacy_pending);
            for (cid, cursor) in state.cursors {
                self.merge_cursor_memory(actor, &cid, cursor);
            }
        } else {
            // Keep this original file untouched for one release. A cursor
            // checkpoint is the migration marker, including an empty result.
            let legacy = self
                .root
                .join("_state")
                .join(format!("{}.json", hash(actor.as_bytes())));
            if legacy.exists() {
                let ids: BTreeSet<String> = serde_json::from_value(load(&legacy)?["ids"].clone())?;
                self.merge_legacy_memory(actor, ids);
                migrated = true;
            }
        }
        self.read_model.loaded.insert(actor.into());
        if migrated && self.degraded.is_none() && !self.messaging_frozen() {
            self.save_read(actor)?;
        }
        Ok(())
    }
    pub(super) fn merge_legacy_memory(&mut self, actor: &str, ids: BTreeSet<String>) {
        for id in ids {
            if let Some(e) = self.published(&id) {
                let cid = strv(&e.event, "conversation_id").to_owned();
                let cursor = ReadCursor::of(&e.event);
                self.merge_cursor_memory(actor, &cid, cursor);
            } else {
                self.read_model
                    .pending_legacy
                    .entry(id)
                    .or_default()
                    .insert(actor.into());
            }
        }
    }
    pub(super) fn merge_read_ids(&mut self, actor: &str, ids: BTreeSet<String>) -> Result<()> {
        self.load_read(actor)?;
        let mut cursors: BTreeMap<String, ReadCursor> = BTreeMap::new();
        for id in ids {
            if let Some(e) = self.published(&id) {
                let cursor = ReadCursor::of(&e.event);
                let old = cursors
                    .entry(strv(&e.event, "conversation_id").into())
                    .or_default();
                if cursor > *old {
                    *old = cursor;
                }
            }
        }
        self.advance_cursors(actor, cursors).map(|_| ())
    }
    pub(super) fn advance_cursors(
        &mut self,
        actor: &str,
        mut cursors: BTreeMap<String, ReadCursor>,
    ) -> Result<bool> {
        self.load_read(actor)?;
        cursors.retain(|cid, c| {
            self.reads[actor]
                .cursors
                .get(cid)
                .is_none_or(|old| &*c > old)
        });
        if cursors.is_empty() {
            return Ok(false);
        }
        self.ensure_messaging_writable()?;
        if actor == "owner" && self.sync_enabled() {
            self.publish(
                "read.cursor",
                None,
                "Owner read conversations",
                json!({"cursors":cursors}),
                "owner",
                "Owner",
                "owner",
                &uuid(),
                "",
            )?;
        } else {
            let previous = self.reads[actor].clone();
            for (cid, cursor) in cursors {
                self.merge_cursor_memory(actor, &cid, cursor);
            }
            if let Err(error) = self.save_read(actor) {
                self.reads.insert(actor.into(), previous);
                return Err(error);
            }
        }
        Ok(true)
    }
    pub fn mark_read(&mut self, actor: &str, p: &Value, people: &[Person]) -> Result<Value> {
        self.load_read(actor)?;
        if p["all"] == true || p["mentions"] == true {
            if p["all"] == true && p["mentions"] == true
                || ["conversation", "channel", "dm", "through"]
                    .iter()
                    .any(|k| !p[k].is_null())
            {
                return Err(err(
                    "invalid_target",
                    "Choose all, mentions, or one conversation",
                ));
            }
            let mut cursors = BTreeMap::new();
            for (cid, c) in &self.read_model.conversations {
                if !self.visible(actor, cid) {
                    continue;
                }
                let cursor = if p["mentions"] == true {
                    c.mentions.get(actor).and_then(|v| v.last()).cloned()
                } else {
                    c.messages.last().map(|v| v.0.clone())
                };
                if let Some(cursor) = cursor {
                    cursors.insert(cid.clone(), cursor);
                }
            }
            let mut changed = false;
            let entries: Vec<_> = cursors.into_iter().collect();
            for chunk in entries.chunks(200) {
                changed |= self.advance_cursors(actor, chunk.iter().cloned().collect())?;
            }
            return Ok(json!({"changed":changed,"counts":self.counts(actor)?}));
        }
        let cid = self.resolve(actor, p, people, false)?.0;
        let cursor = if let Some(id) = p["through"].as_str() {
            let e = self
                .published(id)
                .filter(|e| {
                    e.event["type"] == "message.create" && e.event["conversation_id"] == cid
                })
                .ok_or_else(|| err("not_found", "Message is not in this conversation"))?;
            Some(ReadCursor::of(&e.event))
        } else {
            self.read_model
                .conversations
                .get(&cid)
                .and_then(|c| c.messages.last())
                .map(|e| e.0.clone())
        };
        let changed = if let Some(cursor) = cursor {
            self.advance_cursors(actor, BTreeMap::from([(cid.clone(), cursor)]))?
        } else {
            false
        };
        Ok(
            json!({"conversation_id":cid,"read_cursor":self.read_cursor(actor,&cid),"changed":changed,"counts":self.counts(actor)?}),
        )
    }
    pub(super) fn unread_counts(&self, actor: &str, cid: &str) -> (usize, usize) {
        let Some(c) = self.read_model.conversations.get(cid) else {
            return (0, 0);
        };
        let cursor = self.reads.get(actor).and_then(|r| r.cursors.get(cid));
        let count =
            c.messages.len() - cursor.map_or(0, |r| c.messages.partition_point(|(k, _)| k <= r));
        let after = |v: Option<&Vec<ReadCursor>>| {
            v.map_or(0, |v| {
                v.len() - cursor.map_or(0, |r| v.partition_point(|k| k <= r))
            })
        };
        (
            count - after(c.authors.get(actor)),
            after(c.mentions.get(actor)),
        )
    }
    pub fn counts(&mut self, actor: &str) -> Result<Value> {
        self.load_read(actor)?;
        let mut conversations = serde_json::Map::new();
        let (mut dms, mut mentions) = (0, 0);
        for cid in self
            .read_model
            .conversations
            .keys()
            .filter(|cid| self.visible(actor, cid))
        {
            let (unread, m) = self.unread_counts(actor, cid);
            if self.conversations.contains_key(cid) {
                dms += unread;
            } else {
                mentions += m;
            }
            conversations.insert(cid.clone(), json!({"unread":unread,"mentions":m}));
        }
        Ok(
            json!({"conversations":conversations,"dms":dms,"mentions":mentions,"unread":dms+mentions}),
        )
    }
    pub(super) fn validate_cursor_event(
        &self,
        e: &Value,
        require_messages: bool,
    ) -> Result<BTreeMap<String, ReadCursor>> {
        if e["actor"]["id"] != "owner"
            || e["actor"]["kind"] != "owner"
            || !e["conversation_id"].is_null()
        {
            return Err(err(
                "unauthorized",
                "Only Owner can publish Owner read cursors",
            ));
        }
        let cursors: BTreeMap<String, ReadCursor> =
            serde_json::from_value(e["data"]["cursors"].clone())?;
        if cursors.is_empty() || cursors.len() > 200 {
            return Err(err("invalid_receipt", "Invalid cursor checkpoint size"));
        }
        for (cid, cursor) in &cursors {
            Uuid::parse_str(cid).map_err(|_| err("invalid_receipt", "Invalid conversation ID"))?;
            let (host, id) = cursor
                .event_id
                .split_once(':')
                .ok_or_else(|| err("invalid_receipt", "Invalid cursor message"))?;
            Uuid::parse_str(host)
                .and_then(|_| Uuid::parse_str(id))
                .map_err(|_| err("invalid_receipt", "Invalid cursor message"))?;
            if require_messages {
                let target = self.published(&cursor.event_id).ok_or_else(|| {
                    err(
                        "dependency_missing",
                        "Cursor message has not reached the hub",
                    )
                })?;
                if target.event["conversation_id"] != *cid
                    || !Self::readable_event(&target.event)
                    || ReadCursor::of(&target.event) != *cursor
                    || !self.visible("owner", cid)
                {
                    return Err(err(
                        "unauthorized",
                        "Cursor does not match a visible message",
                    ));
                }
            }
        }
        Ok(cursors)
    }
    pub(super) fn reduce_cursor_event(&mut self, e: &Value) -> Result<()> {
        for (cid, cursor) in self.validate_cursor_event(e, false)? {
            let old = self
                .read_model
                .owner_retained
                .entry(cid.clone())
                .or_default();
            if cursor > *old {
                *old = cursor.clone();
            }
            self.merge_cursor_memory("owner", &cid, cursor);
        }
        Ok(())
    }
    pub(super) fn retain_legacy_owner_reads(&mut self) -> Result<()> {
        self.load_read("owner")?;
        let pending: Vec<_> = self.reads["owner"]
            .cursors
            .iter()
            .filter(|(cid, c)| {
                self.read_model
                    .owner_retained
                    .get(*cid)
                    .is_none_or(|old| *c > old)
            })
            .map(|(cid, c)| (cid.clone(), c.clone()))
            .collect();
        for chunk in pending.chunks(200) {
            let cursors: BTreeMap<_, _> = chunk.iter().cloned().collect();
            self.publish(
                "read.cursor",
                None,
                "Migrated Owner read cursors",
                json!({"cursors":cursors}),
                "owner",
                "Owner",
                "owner",
                &uuid(),
                "",
            )?;
        }
        Ok(())
    }
    pub(super) fn messages_in(&self, cid: &str) -> impl DoubleEndedIterator<Item = &Published> {
        self.read_model
            .conversations
            .get(cid)
            .into_iter()
            .flat_map(|c| c.messages.iter())
            .map(|(_, i)| &self.events[*i])
    }
    pub(super) fn message_candidates<'a>(
        &'a self,
        actor: &str,
        cid: &str,
        anchor: Option<&ReadCursor>,
        newest: bool,
        unread: bool,
        mentions: bool,
    ) -> Box<dyn Iterator<Item = &'a Published> + 'a> {
        let Some(c) = self.read_model.conversations.get(cid) else {
            return Box::new(std::iter::empty());
        };
        let read = unread
            .then(|| self.reads.get(actor).and_then(|r| r.cursors.get(cid)))
            .flatten();
        let lower = if newest {
            read
        } else {
            read.into_iter().chain(anchor).max()
        };
        let upper = newest.then_some(anchor).flatten();
        if mentions {
            let Some(v) = c.mentions.get(actor) else {
                return Box::new(std::iter::empty());
            };
            let lo = lower.map_or(0, |k| v.partition_point(|e| e <= k));
            let hi = upper.map_or(v.len(), |k| v.partition_point(|e| e < k));
            let range = &v[lo.min(hi)..hi];
            let iter: Box<dyn Iterator<Item = &ReadCursor>> = if newest {
                Box::new(range.iter().rev())
            } else {
                Box::new(range.iter())
            };
            Box::new(iter.filter_map(|key| self.published(&key.event_id)))
        } else {
            let v = &c.timeline;
            let lo = lower.map_or(0, |k| v.partition_point(|(e, _)| e <= k));
            let hi = upper.map_or(v.len(), |k| v.partition_point(|(e, _)| e < k));
            let range = &v[lo.min(hi)..hi];
            let iter: Box<dyn Iterator<Item = &(ReadCursor, usize)>> = if newest {
                Box::new(range.iter().rev())
            } else {
                Box::new(range.iter())
            };
            Box::new(iter.map(|(_, i)| &self.events[*i]))
        }
    }
    pub(super) fn events_between(&self, after: u64, high: u64) -> &[Published] {
        let lo = self.events.partition_point(|e| e.position <= after);
        let hi = self.events.partition_point(|e| e.position <= high);
        &self.events[lo.min(hi)..hi]
    }
}

#[cfg(test)]
mod tests;
