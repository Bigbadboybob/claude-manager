//! Bulk inbox catch-up: `messaging.read(inbox|dms, mark_read_before=T)` marks
//! every eligible unread message created before T as read and advances the
//! reader's monitors past what is now read, so a long absence can be cleared
//! in one call instead of paging and acknowledging hundreds of messages.
use super::*;

impl Store {
    /// Load the caller's read receipts if they are not cached yet, so
    /// read-dependent counts (`monitor_status`) see them.
    pub fn load_reads(&mut self, actor: &str) -> Result<()> {
        self.load_read(actor)
    }

    pub(super) fn mark_read_before(
        &mut self,
        actor: &str,
        p: &Value,
        people: &[Person],
    ) -> Result<Value> {
        let before = p["mark_read_before"]
            .as_str()
            .ok_or_else(|| err("invalid_params", "mark_read_before must be an RFC3339 time"))?;
        parse_time(before)?;
        let scope = if p["inbox"] == true {
            "inbox"
        } else if p["dms"] == true {
            "dms"
        } else {
            return Err(err(
                "invalid_target",
                "mark_read_before applies to inbox=True or dms=True",
            ));
        };
        if p["dms"] == true && p["inbox"] == true {
            return Err(err("invalid_target", "Choose inbox or incoming DMs"));
        }
        self.acknowledge(actor, &p["ack_receipt"])?;
        // Reuse read()'s eligibility rules exactly: page an unread-only query
        // bounded at T and collect every id it would supply.
        let mut marked: BTreeSet<String> = BTreeSet::new();
        let mut cursor = Value::Null;
        loop {
            let mut q = json!({scope: true, "unread_only": true, "time": {"end": before}, "limit": 200});
            if !cursor.is_null() {
                q["cursor"] = cursor;
            }
            let page = self.read(actor, &q, people)?;
            marked.extend(
                page["items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v["id"].as_str().map(str::to_owned)),
            );
            cursor = page["next_cursor"].clone();
            if cursor.is_null() {
                break;
            }
        }
        if !marked.is_empty() {
            self.merge_read_ids(actor, marked.clone())?;
        }
        let monitors_advanced = self.advance_monitor_acks_past_reads(actor, &marked)?;
        Ok(json!({
            "marked": marked.len(),
            "monitors_advanced": monitors_advanced,
            "before": before,
            "scope": scope,
            "_marked_ids": marked,
        }))
    }

    /// Move each monitor's acknowledged position forward over hits that are
    /// now read, stopping at the first unread hit so nothing newer is lost.
    fn advance_monitor_acks_past_reads(
        &mut self,
        actor: &str,
        marked: &BTreeSet<String>,
    ) -> Result<usize> {
        self.load_read(actor)?;
        let mut state = self.personal_state(actor);
        let mut advanced = 0;
        for m in state.monitors.values_mut().filter(|m| m.state != "dismissed") {
            let mut hits: Vec<&Published> = self.events_between(m.acknowledged,m.hit_high).iter()
                .collect();
            hits.sort_by_key(|e| e.position);
            let mut through = m.acknowledged;
            for e in hits {
                let id = strv(&e.event, "id");
                if self.monitor_matches(actor, m, e) && !self.is_read(actor,&e.event) && !marked.contains(id) {
                    break;
                }
                through = e.position;
            }
            if through > m.acknowledged {
                m.acknowledged = through;
                advanced += 1;
            }
        }
        if advanced > 0 {
            self.save_personal(state)?;
        }
        Ok(advanced)
    }
}
