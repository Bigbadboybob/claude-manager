//! Read acknowledgements are a grow-only set of message identities. Their
//! retained events travel only between explicitly Owner-authorized hosts.
use super::*;

impl Store {
    pub(super) fn merge_read_ids(&mut self, actor: &str, ids: BTreeSet<String>) -> Result<()> {
        self.load_read(actor)?;
        let mut next = self.reads[actor].ids.clone();
        next.extend(ids);
        if next == self.reads[actor].ids {
            return Ok(());
        }
        if let Some(reason) = &self.degraded {
            return Err(err("store_read_only", reason.clone()));
        }
        if let Err(e) = atomic_replace(
            &self
                .root
                .join("_state")
                .join(format!("{}.json", hash(actor.as_bytes()))),
            &json!({"ids":next}),
        ) {
            self.degraded = Some(format!(
                "Read checkpoint outcome requires reconciliation: {e}"
            ));
            return Err(err("outcome_unknown", self.degraded.clone().unwrap()));
        }
        self.reads.get_mut(actor).unwrap().ids = next;
        self.replication.signal.signal();
        Ok(())
    }
    pub(super) fn validate_read_event(
        &self,
        e: &Value,
        require_messages: bool,
    ) -> Result<BTreeSet<String>> {
        if e["actor"]["id"] != "owner"
            || e["actor"]["kind"] != "owner"
            || !e["conversation_id"].is_null()
        {
            return Err(err(
                "unauthorized",
                "Only Owner can publish Owner read state",
            ));
        }
        let ids: BTreeSet<String> = serde_json::from_value(e["data"]["ids"].clone())?;
        if ids.is_empty() || ids.len() > 200 {
            return Err(err("invalid_receipt", "Invalid read acknowledgement size"));
        }
        for id in &ids {
            let (host, event) = id
                .split_once(':')
                .ok_or_else(|| err("invalid_receipt", "Invalid message identity"))?;
            Uuid::parse_str(host)
                .and_then(|_| Uuid::parse_str(event))
                .map_err(|_| err("invalid_receipt", "Invalid message identity"))?;
            if require_messages {
                let event = self
                    .events
                    .iter()
                    .find(|v| v.event["id"] == *id)
                    .ok_or_else(|| {
                        err(
                            "dependency_missing",
                            "Acknowledged message has not reached the hub",
                        )
                    })?;
                if event.event["type"] != "message.create"
                    || !self.visible("owner", strv(&event.event, "conversation_id"))
                {
                    return Err(err(
                        "unauthorized",
                        "Owner cannot acknowledge another participant's private messages",
                    ));
                }
            }
        }
        Ok(ids)
    }
    pub(super) fn retain_legacy_owner_reads(&mut self) -> Result<()> {
        self.load_read("owner")?;
        let ids = self.reads["owner"].ids.iter().cloned().collect::<Vec<_>>();
        for chunk in ids.chunks(200) {
            let digest = hash(&serde_json::to_vec(chunk)?);
            let key = format!("bootstrap-owner-reads:{digest}");
            if !self.requests.contains_key(&format!("owner\n{key}")) {
                self.publish(
                    "read.ack",
                    None,
                    "Retained Owner read checkpoint",
                    json!({"ids":chunk}),
                    "owner",
                    "Owner",
                    "owner",
                    &key,
                    "",
                )?;
            }
        }
        Ok(())
    }
    pub(super) fn reduce_private_sync(&mut self, e: &Value) -> Result<()> {
        if e["type"] == "read.ack" {
            let ids = self.validate_read_event(e, false)?;
            self.merge_read_ids("owner", ids)?;
        }
        Ok(())
    }
    /// A pinned retry may resolve from a retained local/hub receipt, but cannot
    /// create an event on a different origin if its original outcome is unknown.
    pub fn resolve_pinned_retry(
        &mut self,
        actor: &str,
        p: &Value,
        people: &[Person],
    ) -> Result<Value> {
        let origin = required(p, "origin_daemon_id")?;
        let key = required(p, "request_id")?;
        let prior = self
            .requests
            .get(&format!("{actor}\n{key}"))
            .ok_or_else(|| {
                err(
                    "retry_origin_unavailable",
                    format!("No accepted receipt is available; retry through origin {origin}"),
                )
            })?;
        if prior.2 != origin {
            return Err(err(
                "idempotency_conflict",
                "Request is bound to a different origin",
            ));
        }
        let previous = self.replication.operation_origin.replace(origin);
        let result = self.send(
            actor,
            "",
            if actor == "owner" { "owner" } else { "agent" },
            p,
            people,
        );
        self.replication.operation_origin = previous;
        result
    }
}
