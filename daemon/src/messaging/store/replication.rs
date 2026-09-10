//! Durable replication state. Transport is only a carrier: publication, receipt
//! and coverage records are retained so replay never depends on a socket's RAM.
use super::*;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

#[derive(Default)]
pub struct ChangeSignal {
    version: Mutex<u64>,
    changed: Condvar,
}
impl ChangeSignal {
    pub fn signal(&self) {
        let mut v = self.version.lock().unwrap_or_else(|p| p.into_inner());
        *v = v.wrapping_add(1);
        self.changed.notify_all();
    }
    pub fn version(&self) -> u64 {
        *self.version.lock().unwrap_or_else(|p| p.into_inner())
    }
    pub fn wait(&self, since: u64, timeout: Duration) {
        let v = self.version.lock().unwrap_or_else(|p| p.into_inner());
        let _ = self.changed.wait_timeout_while(v, timeout, |v| *v == since);
    }
}

pub(super) struct Replication {
    pub coordinator_id: String,
    pub coordinator_lineage: BTreeSet<String>,
    pub enabled: bool,
    pub operation_origin: Option<String>,
    pub receipts: BTreeMap<String, Value>,
    pub rejections: BTreeMap<String, Value>,
    pub coverage: BTreeMap<String, Value>,
    pub hosts: BTreeMap<String, Value>,
    pub people: BTreeMap<String, Person>,
    pub owner_snapshot: Option<Value>,
    pub connected: bool,
    pub last_reconciled: Option<String>,
    pub error: Option<String>,
    pub signal: Arc<ChangeSignal>,
}
impl Replication {
    pub fn new(meta: &Value, enabled: bool) -> Result<Self> {
        let coordinator_id = required(meta, "coordinator_id")?;
        Uuid::parse_str(&coordinator_id)
            .map_err(|_| err("invalid_space", "Invalid coordinator ID"))?;
        let coordinator_lineage: BTreeSet<String> = serde_json::from_value(
            meta.get("coordinator_lineage")
                .cloned()
                .unwrap_or(json!([])),
        )?;
        for id in &coordinator_lineage {
            Uuid::parse_str(id).map_err(|_| err("invalid_space", "Invalid coordinator lineage"))?;
        }
        Ok(Self {
            coordinator_lineage,
            coordinator_id,
            enabled,
            operation_origin: None,
            receipts: BTreeMap::new(),
            rejections: BTreeMap::new(),
            coverage: BTreeMap::new(),
            hosts: BTreeMap::new(),
            people: BTreeMap::new(),
            owner_snapshot: None,
            connected: false,
            last_reconciled: None,
            error: None,
            signal: Arc::new(ChangeSignal::default()),
        })
    }
}

impl Store {
    /// A/B committed locally before receipts existed. The original coordinator
    /// may attest those verified publications, preserving bytes, IDs, arrival
    /// positions and its original journal generation. Never invent acceptance
    /// for a replica's pending uploads.
    pub(super) fn retain_legacy_receipts(&mut self) -> Result<()> {
        if !self.is_coordinator() {
            return Ok(());
        }
        let missing = self
            .events
            .iter()
            .filter(|e| {
                !self.replication.receipts.contains_key(strv(&e.event, "id"))
                    && !self
                        .replication
                        .rejections
                        .contains_key(strv(&e.event, "id"))
            })
            .map(|e| (e.event.clone(), e.position))
            .collect::<Vec<_>>();
        for (event, position) in missing {
            if event["origin_daemon_id"] != self.daemon_id {
                return Err(err(
                    "invalid_receipt",
                    "Foreign publication has no retained acceptance",
                ));
            }
            let bytes = fs::read(self.event_path(&event)?)?;
            let receipt = json!({"space_id":self.space_id,"coordinator_id":self.daemon_id,
                "generation":self.generation,"position":format!("{position:020}"),
                "event_id":event["id"],"event_sha256":hash(&bytes)});
            self.record_receipt(&receipt)?;
        }
        Ok(())
    }
    pub fn is_coordinator(&self) -> bool {
        self.replication.coordinator_id == self.daemon_id
    }
    fn receipt_coordinator_valid(&self, receipt: &Value) -> bool {
        receipt["coordinator_id"] == self.replication.coordinator_id
            || receipt["coordinator_id"]
                .as_str()
                .is_some_and(|id| self.replication.coordinator_lineage.contains(id))
    }
    pub fn coordinator_id(&self) -> &str {
        &self.replication.coordinator_id
    }
    pub fn sync_enabled(&self) -> bool {
        self.replication.enabled
    }
    pub fn change_signal(&self) -> Arc<ChangeSignal> {
        self.replication.signal.clone()
    }
    pub fn publication_position(&self) -> u64 {
        self.position
    }
    pub(super) fn operation_origin(&self) -> &str {
        self.replication
            .operation_origin
            .as_deref()
            .unwrap_or(&self.daemon_id)
    }
    pub fn sync_status(&self) -> Value {
        json!({"enabled":self.sync_enabled(),"role":if self.is_coordinator(){"coordinator"}else{"replica"},
            "coordinator_id":self.coordinator_id(),"connected":self.is_coordinator() || self.replication.connected,
            "last_reconciled":self.replication.last_reconciled,"error":self.replication.error,
            "handoff_paused":self.messaging_frozen(),"pending":self.events.iter().filter(|e| e.event["origin_daemon_id"] == self.daemon_id && !self.replication.receipts.contains_key(strv(&e.event,"id")) && !self.replication.rejections.contains_key(strv(&e.event,"id"))).count()})
    }
    pub fn set_sync_connection(&mut self, connected: bool, error: Option<String>) {
        self.replication.connected = connected;
        self.replication.error = error;
    }
    pub fn event_replication(&self, id: &str) -> Value {
        if let Some(rejection) = self.replication.rejections.get(id) {
            return json!({"status":"replication_rejected","decision":rejection});
        }
        if let Some(receipt) = self.replication.receipts.get(id) {
            return json!({"status":"replicated","receipt":receipt});
        }
        if self.replication.error.as_deref().is_some_and(|e| {
            matches!(
                e.split(':').next().unwrap_or(""),
                "unauthorized"
                    | "event_conflict"
                    | "receipt_conflict"
                    | "invalid_receipt"
                    | "invalid_record"
                    | "invalid_config"
                    | "idempotency_conflict"
                    | "resync_required"
            )
        }) {
            return json!({"status":"replication_blocked","reason":self.replication.error});
        }
        json!({"status":"pending_sync"})
    }
    pub(super) fn shared_mutation_allowed(&self) -> Result<()> {
        if !self.is_coordinator() {
            return Err(err("coordinator_required", "This change needs the messaging coordinator; retain the draft and retry when connected"));
        }
        Ok(())
    }
    pub(super) fn apply_replication_journal(&mut self, journal: &Value) -> Result<()> {
        match strv(journal, "kind") {
            "publish" => {
                if let Some(receipt) = journal["data"].get("hub_receipt").filter(|v| !v.is_null()) {
                    let id = required(journal, "event_id")?;
                    self.validate_receipt(receipt, &id, &required(journal, "event_sha256")?)?;
                    self.replication.receipts.insert(id, receipt.clone());
                }
            }
            "replication.status" => {
                let id = required(journal, "event_id")?;
                let decision = &journal["data"];
                let event = self
                    .events
                    .iter()
                    .find(|e| e.event["id"] == id)
                    .ok_or_else(|| err("invalid_receipt", "Decision precedes its local event"))?;
                if journal["event_sha256"] != hash(&fs::read(self.event_path(&event.event)?)?) {
                    return Err(err(
                        "invalid_receipt",
                        "Decision digest does not match its retained event",
                    ));
                }
                if decision["status"] == "replicated" {
                    if self.replication.rejections.contains_key(&id) {
                        return Err(err(
                            "receipt_conflict",
                            "A rejected event cannot later become accepted",
                        ));
                    }
                    self.validate_receipt(
                        &decision["receipt"],
                        &id,
                        &required(journal, "event_sha256")?,
                    )?;
                    if !self.events.iter().any(|e| e.event["id"] == id) {
                        return Err(err("invalid_receipt", "Receipt precedes its local event"));
                    }
                    self.replication
                        .receipts
                        .insert(id, decision["receipt"].clone());
                } else if decision["status"] == "replication_rejected" {
                    if self.replication.receipts.contains_key(&id) {
                        return Err(err(
                            "receipt_conflict",
                            "An accepted event cannot later become rejected",
                        ));
                    }
                    self.replication.rejections.insert(id, decision.clone());
                } else {
                    return Err(err("invalid_record", "Unknown replication decision"));
                }
            }
            "coverage" => {
                let scope = required(&journal["data"], "scope")?;
                let data = &journal["data"];
                Uuid::parse_str(&required(data, "generation")?)
                    .map_err(|_| err("invalid_cursor", "Invalid coverage generation"))?;
                if data["coordinator_id"] != self.coordinator_id()
                    || if scope == "transport" {
                        data["cursor"].as_u64().is_none()
                            || data["revision"].as_u64().is_none()
                            || serde_json::from_value::<BTreeSet<String>>(data["scopes"].clone())
                                .is_err()
                    } else {
                        data["through"].as_u64().is_none() || data["complete"] != true
                    }
                {
                    return Err(err("invalid_cursor", "Invalid coverage checkpoint"));
                }
                self.replication
                    .coverage
                    .insert(scope, journal["data"].clone());
                self.replication.last_reconciled = Some(required(journal, "recorded_at")?);
            }
            _ => return Err(err("invalid_record", "Unsupported journal kind")),
        }
        Ok(())
    }
    fn journal_status(
        &mut self,
        kind: &str,
        id: Option<&str>,
        digest: Option<&str>,
        data: Value,
    ) -> Result<()> {
        if let Some(reason) = &self.degraded {
            return Err(err("store_read_only", reason.clone()));
        }
        let pos = self
            .position
            .checked_add(1)
            .ok_or_else(|| err("position_overflow", "Journal exhausted"))?;
        let j = json!({"protocol":1,"replica_id":self.daemon_id,"generation":self.generation,"position":format!("{pos:020}"),
            "recorded_at":now(),"kind":kind,"event_id":id,"event_sha256":digest,"data":data});
        if let Err(e) = write_file(
            &self.journal_dir().join(format!("{pos:020}.json")),
            &serde_json::to_vec_pretty(&j)?,
            false,
        ) {
            self.degraded = Some(format!(
                "Replication journal outcome requires reconciliation: {e}"
            ));
            return Err(err("outcome_unknown", self.degraded.clone().unwrap()));
        }
        self.position = pos;
        self.apply_replication_journal(&j)?;
        self.replication.signal.signal();
        Ok(())
    }
    fn validate_receipt(&self, receipt: &Value, id: &str, digest: &str) -> Result<()> {
        if receipt["space_id"] != self.space_id
            || !self.receipt_coordinator_valid(receipt)
            || receipt["event_id"] != id
            || receipt["event_sha256"] != digest
        {
            return Err(err(
                "invalid_receipt",
                "Receipt has the wrong space, coordinator, event or digest",
            ));
        }
        Uuid::parse_str(&required(receipt, "generation")?)
            .map_err(|_| err("invalid_receipt", "Bad receipt generation"))?;
        let pos = required(receipt, "position")?
            .parse::<u64>()
            .map_err(|_| err("invalid_receipt", "Bad receipt position"))?;
        if pos == 0 {
            return Err(err("invalid_receipt", "Receipt position must be positive"));
        }
        if self.replication.rejections.contains_key(id) {
            return Err(err("receipt_conflict", "Event was already rejected"));
        }
        if self
            .replication
            .receipts
            .get(id)
            .is_some_and(|old| old != receipt)
        {
            return Err(err("receipt_conflict", "Conflicting acceptance receipt"));
        }
        Ok(())
    }
    /// Receipt changes are journal entries, never new message arrivals.
    pub fn record_receipt(&mut self, receipt: &Value) -> Result<()> {
        let id = required(receipt, "event_id")?;
        let digest = required(receipt, "event_sha256")?;
        self.validate_receipt(receipt, &id, &digest)?;
        if receipt["space_id"] != self.space_id || !self.receipt_coordinator_valid(receipt) {
            return Err(err(
                "invalid_receipt",
                "Receipt has the wrong space/coordinator",
            ));
        }
        Uuid::parse_str(&required(receipt, "generation")?)
            .map_err(|_| err("invalid_receipt", "Bad receipt generation"))?;
        required(receipt, "position")?
            .parse::<u64>()
            .map_err(|_| err("invalid_receipt", "Bad receipt position"))?;
        if !self.events.iter().any(|e| e.event["id"] == id) {
            return Err(err(
                "invalid_receipt",
                "Receipt has no retained local event",
            ));
        }
        if let Some(event) = self.events.iter().find(|e| e.event["id"] == id) {
            if hash(&fs::read(self.event_path(&event.event)?)?) != digest {
                return Err(err(
                    "receipt_conflict",
                    "Receipt digest does not match the retained event",
                ));
            }
        }
        if let Some(old) = self.replication.receipts.get(&id) {
            return if old == receipt {
                Ok(())
            } else {
                Err(err("receipt_conflict", "Conflicting acceptance receipt"))
            };
        }
        if self.replication.rejections.contains_key(&id) {
            return Err(err("receipt_conflict", "Event was already rejected"));
        }
        self.journal_status(
            "replication.status",
            Some(&id),
            Some(&digest),
            json!({"status":"replicated","receipt":receipt}),
        )
    }
    pub fn reject_replication(&mut self, id: &str, reason: &str) -> Result<()> {
        if self.replication.rejections.contains_key(id) {
            return Ok(());
        }
        if self.replication.receipts.contains_key(id) {
            return Err(err(
                "receipt_conflict",
                "Previously accepted event cannot be rejected",
            ));
        }
        let e = self
            .events
            .iter()
            .find(|e| e.event["id"] == id)
            .ok_or_else(|| err("not_found", "Local event not found"))?;
        let digest = hash(&fs::read(self.event_path(&e.event)?)?);
        self.journal_status("replication.status",Some(id),Some(&digest),json!({"status":"replication_rejected","reason":reason,"coordinator_id":self.coordinator_id()}))?;
        let children: Vec<_> = self
            .events
            .iter()
            .filter(|e| {
                e.event["data"]["reply_to"] == id
                    && !self.replication.receipts.contains_key(strv(&e.event, "id"))
            })
            .map(|e| required(&e.event, "id"))
            .collect::<Result<_>>()?;
        for child in children {
            self.reject_replication(&child, "dependency_rejected")?;
        }
        Ok(())
    }
    pub fn wire_event(&self, id: &str) -> Result<Value> {
        let e = self
            .events
            .iter()
            .find(|e| e.event["id"] == id)
            .ok_or_else(|| err("not_found", "Event not found"))?;
        let bytes = fs::read(self.event_path(&e.event)?)?;
        let body =
            String::from_utf8(bytes).map_err(|_| err("invalid_record", "Event is not UTF-8"))?;
        Ok(json!({"event":body,"receipt":self.replication.receipts.get(id),"position":e.position}))
    }
    pub fn pending_uploads(&self, limit: usize) -> Result<Vec<Value>> {
        self.events
            .iter()
            .filter(|e| {
                e.event["origin_daemon_id"] == self.daemon_id
                    && !self.replication.receipts.contains_key(strv(&e.event, "id"))
                    && !self
                        .replication
                        .rejections
                        .contains_key(strv(&e.event, "id"))
            })
            .take(limit.min(64))
            .map(|e| self.wire_event(strv(&e.event, "id")))
            .collect()
    }
    /// Caller must be an authenticated coordinator transport. Preserve the exact
    /// original UTF-8 bytes, including unknown fields and JSON formatting.
    pub fn ingest_replica(&mut self, wire: &Value) -> Result<bool> {
        let raw = required(wire, "event")?;
        let event = self.validate_replication_envelope(raw.as_bytes())?;
        let id = required(&event, "id")?;
        if self.events.iter().any(|e| e.event["id"] == id) {
            if fs::read(self.event_path(&event)?)? != raw.as_bytes() {
                return Err(err("event_conflict", "Same event ID has different bytes"));
            }
            if let Some(receipt) = wire.get("receipt").filter(|v| !v.is_null()) {
                self.record_receipt(receipt)?;
            }
            return Ok(false);
        }
        let receipt = wire
            .get("receipt")
            .filter(|v| !v.is_null())
            .ok_or_else(|| {
                err(
                    "invalid_receipt",
                    "Hub ingestion requires an acceptance receipt",
                )
            })?;
        if receipt["event_id"] != id
            || receipt["event_sha256"] != hash(raw.as_bytes())
            || receipt["space_id"] != self.space_id
            || !self.receipt_coordinator_valid(receipt)
        {
            return Err(err(
                "invalid_receipt",
                "Receipt does not attest the incoming bytes",
            ));
        }
        self.validate_receipt(receipt, &id, &hash(raw.as_bytes()))?;
        self.validate_event_dependencies(&event)?;
        self.preflight_projection(&event)?;
        self.commit_bytes(event, raw.into_bytes(), "replica", Some(receipt.clone()))?;
        self.project()?;
        Ok(true)
    }
    pub(super) fn validate_replication_envelope(&self, raw: &[u8]) -> Result<Value> {
        if raw.len() > 65536 {
            return Err(err("event_too_large", "Serialized event exceeds 64 KiB"));
        }
        let e: Value = serde_json::from_slice(raw)?;
        if e["protocol"] != 1
            || e["space_id"] != self.space_id
            || !e["data"].is_object()
            || !e["extensions"].is_object()
            || strv(&e["actor"], "id").is_empty()
            || strv(&e, "body").is_empty()
            || strv(&e["request"], "key").is_empty()
            || strv(&e["request"], "sha256").len() != 64
            || e["origin_daemon_id"].as_str() != strv(&e, "id").split_once(':').map(|(o, _)| o)
        {
            return Err(err("invalid_record", "Malformed event envelope"));
        }
        self.event_path(&e)?;
        Uuid::parse_str(&required(&e["request"], "origin_daemon_id")?)
            .map_err(|_| err("invalid_record", "Invalid request origin"))?;
        required(&e, "logical_time")?
            .parse::<u64>()
            .map_err(|_| err("invalid_record", "Invalid logical time"))?;
        parse_time(&required(&e, "created_at")?)?;
        Ok(e)
    }
    pub(super) fn validate_event_dependencies(&self, e: &Value) -> Result<()> {
        if let Some(conv) = e["conversation_id"].as_str() {
            if !self.channels.values().any(|id| id == conv)
                && !self.conversations.contains_key(conv)
                && e["data"]["conversation_create"]["id"] != conv
            {
                return Err(err(
                    "dependency_missing",
                    "Conversation metadata is not available yet",
                ));
            }
        }
        if let Some(parent) = e["data"]["reply_to"].as_str() {
            let p = self
                .events
                .iter()
                .find(|p| p.event["id"] == parent)
                .ok_or_else(|| err("dependency_missing", "Reply parent is not available yet"))?;
            if p.event["type"] != "message.create"
                || p.event["conversation_id"] != e["conversation_id"]
                || required(&p.event, "logical_time")?
                    .parse::<u64>()
                    .unwrap_or(u64::MAX)
                    >= required(e, "logical_time")?.parse::<u64>().unwrap_or(0)
            {
                return Err(err("invalid_reply", "Reply dependency is invalid"));
            }
            let root = p.event["data"]["thread_root"].as_str().unwrap_or(parent);
            if e["data"]["thread_root"] != root {
                return Err(err("invalid_reply", "Reply has a forged thread root"));
            }
        }
        if e["type"] == "message.create"
            && e["data"]["reply_to"].is_null()
            && !e["data"]["thread_root"].is_null()
        {
            return Err(err(
                "invalid_reply",
                "A top-level message cannot claim a thread root",
            ));
        }
        Ok(())
    }
    fn preflight_projection(&self, e: &Value) -> Result<()> {
        if let Some(channels) = e["data"]["channels"].as_array() {
            for c in channels {
                channel_path(&required(c, "path")?)?;
                Uuid::parse_str(&required(c, "id")?)
                    .map_err(|_| err("invalid_record", "Invalid channel UUID"))?;
            }
        }
        if e["type"] == "host.update" {
            if e["actor"]["id"] != "system" {
                return Err(err("invalid_record", "Host enrollment is system-owned"));
            }
            let host = required(&e["data"], "host_id")?;
            Uuid::parse_str(&host).map_err(|_| err("invalid_record", "Invalid host UUID"))?;
            Uuid::parse_str(&required(&e["data"], "enrollment_revision")?)
                .map_err(|_| err("invalid_record", "Invalid enrollment revision"))?;
            let people: Vec<Person> = serde_json::from_value(e["data"]["people"].clone())?;
            if people.iter().any(|p| {
                p.id != format!("agent:{host}:{}", p.session_uid)
                    || p.kind != "agent"
                    || p.session_uid.is_empty()
            }) {
                return Err(err(
                    "invalid_record",
                    "Foreign participant in host directory",
                ));
            }
        }
        // Validate every fallible reducer input before creating a durable
        // publication. A bad remote record must not poison journal replay.
        let data = &e["data"];
        match strv(e, "type") {
            "membership.enrollment" => {
                let actor = required(data, "participant_id")?;
                let channels: Vec<String> =
                    serde_json::from_value(data["default_channels"].clone())?;
                if actor.is_empty()
                    || actor == "system"
                    || e["actor"]["id"] != "system"
                    || self.enrolled.contains(&actor)
                    || channels.iter().any(|id| !self.memberships.contains_key(id))
                {
                    return Err(err("invalid_record", "Invalid participant enrollment"));
                }
            }
            "channel.membership.initialize" => {
                let id = required(data, "channel_id")?;
                let members: BTreeSet<String> = serde_json::from_value(data["members"].clone())?;
                if !self.channels.values().any(|cid| cid == &id)
                    || self.memberships.contains_key(&id)
                    || e["actor"]["id"] != "system"
                    || members.iter().any(|m| m.is_empty() || m == "system")
                {
                    return Err(err(
                        "invalid_record",
                        "Invalid channel membership initialization",
                    ));
                }
            }
            "channel.membership" => {
                self.validate_membership_event(e)?;
            }
            "read.ack" => {
                self.validate_read_event(e, false)?;
            }
            _ => {}
        }
        if let Some(c) = data.get("conversation_create") {
            let id = required(c, "id")?;
            let members: Vec<String> = serde_json::from_value(c["members"].clone())?;
            if !(2..=32).contains(&members.len())
                || members.windows(2).any(|pair| pair[0] >= pair[1])
                || !members.iter().any(|m| m == strv(&e["actor"], "id"))
                || members.iter().any(|m| m.is_empty() || m == "system")
                || self
                    .conversations
                    .get(&id)
                    .is_some_and(|old| old != &members)
            {
                return Err(err("invalid_record", "Invalid immutable DM membership"));
            }
        }
        if let Some(n) = if e["type"] == "identity.update" {
            data.get("identity")
        } else {
            data.get("identity_claim")
        } {
            let id = required(n, "participant_id")?;
            let v: Name = serde_json::from_value(n.clone())?;
            if self
                .names
                .get(&id)
                .is_some_and(|old| old.revision == v.revision && old.name != v.name)
            {
                return Err(err(
                    "identity_conflict",
                    "Same revision with different name",
                ));
            }
        }
        let key = format!(
            "{}\n{}",
            required(&e["actor"], "id")?,
            required(&e["request"], "key")?
        );
        if self.requests.contains_key(&key) {
            return Err(err(
                "idempotency_conflict",
                "A retained request already has a different event ID",
            ));
        }
        Ok(())
    }
}

impl Store {
    pub(super) fn reduce_replication(&mut self, event: &Value) -> Result<()> {
        if event["type"] != "host.update" {
            return Ok(());
        }
        if event["actor"]["id"] != "system" {
            return Err(err("invalid_record", "Host enrollment is system-owned"));
        }
        let data = &event["data"];
        let host = required(data, "host_id")?;
        Uuid::parse_str(&host).map_err(|_| err("invalid_record", "Invalid host ID"))?;
        Uuid::parse_str(&required(data, "enrollment_revision")?)
            .map_err(|_| err("invalid_record", "Invalid enrollment revision"))?;
        let people: Vec<Person> = serde_json::from_value(data["people"].clone())?;
        for person in self
            .replication
            .people
            .values_mut()
            .filter(|p| p.id.starts_with(&format!("agent:{host}:")))
        {
            person.present = false;
        }
        for mut person in people {
            person.present &= data["active"] == true;
            if person.id != format!("agent:{host}:{}", person.session_uid)
                || person.session_uid.is_empty()
                || person.kind != "agent"
            {
                return Err(err(
                    "invalid_record",
                    "Host directory has a foreign participant",
                ));
            }
            self.replication.people.insert(person.id.clone(), person);
        }
        if host == self.daemon_id {
            self.enrollment_revision = required(data, "enrollment_revision")?;
        }
        self.replication.hosts.insert(host, data.clone());
        Ok(())
    }
    pub fn remote_people(&self) -> Vec<Person> {
        self.replication
            .people
            .values()
            .filter(|p| !p.id.starts_with(&format!("agent:{}:", self.daemon_id)))
            .cloned()
            .collect()
    }
    /// Pairing administration calls this under the coordinator writer lock.
    /// Presence refreshes retain the existing enrollment revision; revocation
    /// and re-enrollment always mint another revision.
    pub fn register_host(
        &mut self,
        host: &str,
        active: bool,
        owner_access: bool,
        people: &[Person],
    ) -> Result<Value> {
        self.shared_mutation_allowed()?;
        Uuid::parse_str(host).map_err(|_| err("invalid_params", "Invalid host ID"))?;
        for p in people {
            if p.id != format!("agent:{host}:{}", p.session_uid)
                || p.session_uid.is_empty()
                || p.kind != "agent"
                || p.name.chars().count() > 256
            {
                return Err(err(
                    "unauthorized",
                    "A host may register only its own participants",
                ));
            }
        }
        let old = self.replication.hosts.get(host);
        let revision = old
            .filter(|v| v["active"] == active && v["owner_access"] == owner_access)
            .and_then(|v| v["enrollment_revision"].as_str())
            .map(str::to_owned)
            .unwrap_or_else(uuid);
        let mut people = people.to_vec();
        people.sort_by(|a, b| a.id.cmp(&b.id));
        let data = json!({"host_id":host,"active":active,"owner_access":owner_access,"enrollment_revision":revision,"people":people});
        if old != Some(&data) {
            self.publish(
                "host.update",
                None,
                &format!(
                    "System {} messaging host {host}",
                    if active {
                        "enrolled/refreshed"
                    } else {
                        "revoked"
                    }
                ),
                data.clone(),
                "system",
                "System",
                "system",
                &uuid(),
                "",
            )?;
        }
        if active {
            self.enroll_participants(&people)?;
        }
        Ok(data)
    }
    pub fn host_authorized(&self, host: &str, actor: Option<&str>) -> bool {
        let Some(h) = self
            .replication
            .hosts
            .get(host)
            .filter(|h| h["active"] == true)
        else {
            return false;
        };
        actor.is_none_or(|actor| {
            if actor == "owner" {
                h["owner_access"] == true
            } else {
                actor.starts_with(&format!("agent:{host}:"))
                    && (self.replication.people.contains_key(actor)
                        || self.names.contains_key(actor))
            }
        })
    }
    /// A transport authenticates the peer before entering this method. The
    /// forwarded actor must still belong to that peer; no control RPCs exist.
    pub fn coordinate(
        &mut self,
        host: &str,
        actor: &str,
        method: &str,
        p: &Value,
    ) -> Result<Value> {
        self.shared_mutation_allowed()?;
        if !self.host_authorized(host, Some(actor)) {
            return Err(err(
                "unauthorized",
                "Host cannot speak for this participant",
            ));
        }
        if p.get("from").is_some() || p.get("_authenticated_actor").is_some() {
            return Err(err("invalid_params", "Sender is authenticated"));
        }

        let uid = actor.strip_prefix(&format!("agent:{host}:")).unwrap_or("");
        let people = self
            .replication
            .people
            .values()
            .cloned()
            .chain(std::iter::once(Person {
                id: "owner".into(),
                name: "Owner".into(),
                session_uid: String::new(),
                task: None,
                present: true,
                kind: "owner".into(),
            }))
            .collect::<Vec<_>>();
        if p["origin_daemon_id"].as_str().is_some_and(|o| o != host) {
            return if actor == "owner" && method == "messaging.send" {
                self.resolve_pinned_retry(actor, p, &people)
            } else {
                Err(err(
                    "retry_origin_unavailable",
                    "Request is pinned to another origin",
                ))
            };
        }
        self.replication.operation_origin = Some(host.into());
        let result = (|| match method {
            "messaging.send" => self.send(
                actor,
                uid,
                if actor == "owner" { "owner" } else { "agent" },
                p,
                &people,
            ),
            "sync.owner_attention" if actor == "owner" => self.attention(actor, true),
            "messaging.monitor" if actor == "owner" => self.register_monitor(actor, p, &people),
            "messaging.monitors" if actor == "owner" => self.monitors(actor, p),
            "messaging.follow" if actor == "owner" => self.follow(actor, p, &people),
            "messaging.channels" => self.channel_action(actor, p, &people),
            "messaging.pins" => self.pins(actor, p, &people),
            "messaging.norms" => {
                let name = self
                    .names
                    .get(actor)
                    .map(|n| n.name.clone())
                    .unwrap_or_else(|| {
                        if actor == "owner" {
                            "Owner".into()
                        } else {
                            "Participant".into()
                        }
                    });
                if matches!(p["action"].as_str(), Some("publish" | "revert")) {
                    self.change_norms(
                        actor,
                        if actor == "owner" { "owner" } else { "agent" },
                        &name,
                        p,
                    )
                } else {
                    self.norms_document(actor, p)
                }
            }
            "session.set_name" => {
                let target = p["uid"].as_str().unwrap_or(uid);
                if actor != "owner" && target != uid {
                    return Err(err("unauthorized", "An agent may rename only itself"));
                }
                let target_host = p["target_daemon_id"].as_str().unwrap_or(host);
                if actor != "owner" && target_host != host {
                    return Err(err("unauthorized", "An agent may rename only itself"));
                }
                let target_actor = format!("agent:{target_host}:{target}");
                let mut params = p.clone();
                params["_authenticated_actor"] = json!(actor);
                self.rename(&target_actor, target, &params)
            }
            _ => Err(err(
                "unsupported_feature",
                "This is not a coordinated messaging operation",
            )),
        })();
        self.replication.operation_origin = None;
        result.map(|mut v| {
            if actor == "owner" && v.is_object() {
                v["execution_host"] = json!(self.coordinator_id());
            }
            v
        })
    }
    pub fn accept_upload(&mut self, host: &str, wire: &Value) -> Result<Value> {
        self.shared_mutation_allowed()?;
        let raw = required(wire, "event")?;
        let e = self.validate_replication_envelope(raw.as_bytes())?;
        let id = required(&e, "id")?;
        if e["origin_daemon_id"] != host || e["request"]["origin_daemon_id"] != host {
            return Err(err("unauthorized", "Upload has a foreign origin"));
        }
        // Preserve an already accepted result even if metadata has since moved.
        if self.events.iter().any(|p| p.event["id"] == id) {
            if fs::read(self.event_path(&e)?)? != raw.as_bytes() {
                return Err(err("event_conflict", "Same event ID has different bytes"));
            }
            return self
                .replication
                .receipts
                .get(&id)
                .cloned()
                .ok_or_else(|| err("outcome_unknown", "Hub receipt is pending"));
        }
        let actor = required(&e["actor"], "id")?;
        if !self.host_authorized(host, Some(&actor)) {
            return Err(err("host_revoked", "Host or participant is not enrolled"));
        }
        if !matches!(e["type"].as_str(), Some("message.create" | "read.ack"))
            || [
                "channels",
                "identity_claim",
                "identity_attestation",
                "conversation_create",
            ]
            .iter()
            .any(|k| e["data"].get(*k).is_some())
        {
            return Err(err(
                "unauthorized",
                "Only ordinary messages may originate at a replica",
            ));
        }
        if e["type"] == "read.ack" {
            self.validate_read_event(&e, true)?;
            let key = format!("{actor}\n{}", required(&e["request"], "key")?);
            if self.requests.contains_key(&key) {
                return Err(err(
                    "idempotency_conflict",
                    "Read acknowledgement request was already used",
                ));
            }
            self.commit_bytes(e, raw.into_bytes(), "replica", None)?;
            return self
                .replication
                .receipts
                .get(&id)
                .cloned()
                .ok_or_else(|| err("outcome_unknown", "Hub receipt is pending"));
        }
        if e["body"].as_str().unwrap().chars().count() > 3000
            || e["body"].as_str().unwrap().trim().is_empty()
        {
            return Err(err("invalid_message", "Message exceeds the body policy"));
        }
        if e["actor"]["kind"] != if actor == "owner" { "owner" } else { "agent" } {
            return Err(err(
                "unauthorized",
                "Actor kind does not match its identity",
            ));
        }
        let request_key = format!("{actor}\n{}", required(&e["request"], "key")?);
        if self.requests.contains_key(&request_key) {
            return Err(err(
                "idempotency_conflict",
                "Request already has another event ID",
            ));
        }
        self.validate_event_dependencies(&e)?;
        if let Some(parent) = e["data"]["reply_to"].as_str() {
            if !self.replication.receipts.contains_key(parent) {
                return Err(err("dependency_missing", "Parent has no hub acceptance"));
            }
        }
        let conv = required(&e, "conversation_id")?;
        if !self.visible(&actor, &conv) {
            return Err(err(
                "unauthorized",
                "Sender cannot access this conversation",
            ));
        }
        let identity = required(&e["data"]["metadata_seen"], "identity")?;
        if actor == "owner" {
            if identity != self.owner_identity_revision {
                return Err(err("invalid_metadata", "Unknown Owner identity revision"));
            }
        } else if !self.events.iter().any(|p| {
            let n = if p.event["type"] == "identity.update" {
                &p.event["data"]["identity"]
            } else {
                &p.event["data"]["identity_claim"]
            };
            n["participant_id"] == actor
                && n["revision_id"] == identity
                && n["name"] == e["actor"]["name"]
        }) {
            return Err(err("invalid_metadata", "Unknown sender identity revision"));
        }
        let enrollment = required(&e["data"]["metadata_seen"], "enrollment")?;
        if self.replication.hosts[host]["enrollment_revision"] != enrollment {
            return Err(err(
                "enrollment_revoked",
                "An earlier host enrollment cannot submit new operations",
            ));
        }
        if !self.events.iter().any(|p| {
            p.event["type"] == "host.update"
                && p.event["data"]["host_id"] == host
                && p.event["data"]["enrollment_revision"] == enrollment
                && p.event["data"]["active"] == true
        }) {
            return Err(err("invalid_metadata", "Unknown host enrollment revision"));
        }
        let tags: Vec<String> = serde_json::from_value(e["data"]["tags"].clone())?;
        let links: Vec<Value> = serde_json::from_value(e["data"]["links"].clone())?;
        if tags.len() > 16
            || tags.iter().any(|t| t.is_empty() || t.chars().count() > 32)
            || links.len() > 8
            || links.iter().any(|l| {
                strv(l, "uri").is_empty()
                    || strv(l, "uri").chars().count() > 2048
                    || strv(l, "label").chars().count() > 120
            })
        {
            return Err(err(
                "invalid_message",
                "Invalid tag or file-reference fields",
            ));
        }
        let conversation_revision = required(&e["data"]["metadata_seen"], "conversation")?;
        if self.conversations.contains_key(&conv) {
            if conversation_revision != conv {
                return Err(err("invalid_metadata", "Invalid immutable DM revision"));
            }
        } else if conversation_revision != conv
            && !self.events.iter().any(|p| {
                p.event["type"] == "channel.update"
                    && p.event["data"]["channel"]["id"] == conv
                    && p.event["data"]["channel"]["revision"] == conversation_revision
            })
        {
            return Err(err("invalid_metadata", "Unknown channel revision"));
        }
        let mut stale_archive = false;
        if let Some((path, cid)) = self.channels.iter().find(|(_, cid)| *cid == &conv) {
            let observed = self
                .events
                .iter()
                .find(|p| {
                    p.event["type"] == "channel.update"
                        && p.event["data"]["channel"]["id"] == conv
                        && p.event["data"]["channel"]["revision"] == conversation_revision
                })
                .map(|p| &p.event["data"]["channel"]);
            let created_archived = conversation_revision == conv
                && self.events.iter().any(|p| {
                    p.event["data"]["channels"]
                        .as_array()
                        .is_some_and(|a| a.iter().any(|c| c["id"] == conv && c["archived"] == true))
                });
            if created_archived || observed.is_some_and(|c| c["archived"] == true) {
                return Err(err(
                    "conversation_archived",
                    "Origin already observed the archive revision",
                ));
            }
            let latest = self.channel_info(path, cid);
            stale_archive =
                latest["archived"] == true && latest["revision"] != conversation_revision;
        }
        let mentions: Vec<String> = serde_json::from_value(e["data"]["mentions"].clone())?;
        let recipients: BTreeSet<String> =
            serde_json::from_value(e["data"]["mention_recipients"].clone())?;
        let here = e["data"]["mention_here"]
            .as_bool()
            .ok_or_else(|| err("invalid_mention", "Missing mention_here flag"))?;
        let mut expected: BTreeSet<String> = mentions.iter().cloned().collect();
        if let Some(members) = self.conversations.get(&conv) {
            if here || mentions.iter().any(|m| !members.contains(m)) {
                return Err(err("invalid_mention", "Invalid DM mention audience"));
            }
        } else {
            let revision = required(&e["data"]["metadata_seen"], "membership")?;
            let members = self.members_at_revision(&conv, &revision)?;
            if !members.contains(&actor) {
                return Err(err(
                    "join_required",
                    "Sender was not a member at the supplied revision",
                ));
            }
            if here {
                expected.extend(members);
            }
        }
        if expected != recipients
            || mentions.len() > 32
            || mentions.iter().any(|m| {
                m != "owner"
                    && !self.names.contains_key(m)
                    && !self.replication.people.contains_key(m)
            })
        {
            return Err(err("invalid_mention", "Forged or unknown mention audience"));
        }
        let receipt = if stale_archive {
            Some(
                json!({"space_id":self.space_id,"coordinator_id":self.daemon_id,"generation":self.generation,
            "position":format!("{:020}",self.position.checked_add(1).ok_or_else(||err("position_overflow","Journal exhausted"))?),"event_id":id,"event_sha256":hash(raw.as_bytes()),"accepted_from_stale_revision":true}),
            )
        } else {
            None
        };
        self.commit_bytes(e, raw.into_bytes(), "replica", receipt)?;
        self.replication
            .receipts
            .get(&id)
            .cloned()
            .ok_or_else(|| err("outcome_unknown", "Hub receipt is pending"))
    }
    fn members_at_revision(&self, channel: &str, revision: &str) -> Result<BTreeSet<String>> {
        let mut members = BTreeSet::new();
        for p in &self.events {
            let e = &p.event;
            match strv(e, "type") {
                "channel.create"
                    if e["data"]["channels"]
                        .as_array()
                        .is_some_and(|a| a.iter().any(|c| c["id"] == channel)) =>
                {
                    if e["actor"]["id"] != "system" {
                        members.insert(required(&e["actor"], "id")?);
                    }
                }
                "channel.membership.initialize" if e["data"]["channel_id"] == channel => {
                    members = serde_json::from_value(e["data"]["members"].clone())?;
                }
                "membership.enrollment"
                    if e["data"]["default_channels"]
                        .as_array()
                        .is_some_and(|a| a.iter().any(|id| id == channel)) =>
                {
                    members.insert(required(&e["data"], "participant_id")?);
                }
                "channel.membership" if e["conversation_id"] == channel => {
                    let actor = required(&e["data"], "participant_id")?;
                    if e["data"]["joined"] == true {
                        members.insert(actor);
                    } else {
                        members.remove(&actor);
                    }
                }
                _ => continue,
            }
            if e["id"] == revision {
                return Ok(members);
            }
        }
        Err(err(
            "invalid_metadata",
            "Unknown channel membership revision",
        ))
    }
}

impl Store {
    /// Provisioning only writes into an unused messaging root. Live pairing and
    /// coordinator handoff use explicit operator operations, not this helper.
    pub fn provision_replica(cm_root: &Path, descriptor: &Value) -> Result<Self> {
        if cm_root.join("messages/main").exists() {
            return Err(err(
                "store_exists",
                "Refusing to replace an existing messaging store",
            ));
        }
        mkdir(cm_root)?;
        let daemon_id = if cm_root.join("daemon-id").exists() {
            fs::read_to_string(cm_root.join("daemon-id"))?
                .trim()
                .to_owned()
        } else {
            let id = uuid();
            write_file(&cm_root.join("daemon-id"), id.as_bytes(), false)?;
            id
        };
        let mut meta = descriptor.clone();
        for key in ["space_id", "coordinator_id", "owner_identity_revision"] {
            Uuid::parse_str(&required(&meta, key)?)
                .map_err(|_| err("invalid_space", "Invalid replica descriptor"))?;
        }
        meta["protocol"] = json!(1);
        meta["replica_id"] = json!(daemon_id);
        meta["generation"] = json!(uuid());
        meta["enrollment_revision"] = json!(uuid());
        atomic_replace(
            &cm_root.join("messaging-sync.json"),
            &json!({"version":1,"coordinator_id":meta["coordinator_id"],"space_id":meta["space_id"],"configured":false}),
        )?;
        atomic_replace(&cm_root.join("messages/main/space.json"), &meta)?;
        Self::open(cm_root)
    }
    pub fn space_descriptor(&self) -> Value {
        json!({"protocol":1,"space_id":self.space_id,"coordinator_id":self.coordinator_id(),"owner_identity_revision":self.owner_identity_revision,"coordinator_lineage":self.replication.coordinator_lineage})
    }
    fn eligible_for_host(&self, host: &str, interests: &BTreeSet<String>, e: &Value) -> bool {
        if e["type"] == "read.ack" {
            return self
                .replication
                .hosts
                .get(host)
                .is_some_and(|h| h["owner_access"] == true);
        }
        let Some(conv) = e["conversation_id"].as_str() else {
            return true;
        };
        let owns = |actor: &str| {
            actor.starts_with(&format!("agent:{host}:"))
                || actor == "owner"
                    && self
                        .replication
                        .hosts
                        .get(host)
                        .is_some_and(|h| h["owner_access"] == true)
        };
        if let Some(members) = self.conversations.get(conv) {
            return members.iter().any(|m| owns(m));
        }
        if !self.channels.values().any(|id| id == conv) {
            return false;
        }
        if e["type"] != "message.create" {
            return true;
        }
        interests.contains(conv)
            || interests.contains("*")
            || self
                .memberships
                .get(conv)
                .is_some_and(|members| members.iter().any(|m| owns(m)))
            || mention_recipients(e).iter().any(|m| owns(m))
    }
    fn dependency_ids(&self, e: &Value) -> Result<Vec<String>> {
        // Parent chains are walked iteratively. Pages can stop within a chain;
        // the publication cursor advances only after its target is durable.
        let mut parent = e["data"]["reply_to"].as_str().or_else(|| {
            (e["type"] == "conversation.pin")
                .then(|| e["data"]["target_id"].as_str())
                .flatten()
        });
        let mut ids = Vec::new();
        let mut seen = BTreeSet::new();
        while let Some(id) = parent {
            if !seen.insert(id) {
                return Err(err("invalid_reply", "Cyclic reply dependency"));
            }
            let p = self
                .events
                .iter()
                .find(|p| p.event["id"] == id)
                .ok_or_else(|| err("dependency_missing", "Missing retained parent"))?;
            if p.event["conversation_id"] != e["conversation_id"] {
                return Err(err("invalid_reply", "Cross-conversation dependency"));
            }
            ids.push(id.to_owned());
            parent = p.event["data"]["reply_to"].as_str();
        }
        ids.reverse();
        Ok(ids)
    }
    pub fn export_page(
        &self,
        host: &str,
        interests: &BTreeSet<String>,
        after: u64,
        through: u64,
    ) -> Result<Value> {
        self.export_slice(host, interests, after, through, 0)
    }
    /// One bounded priority page between bulk pages. This carries only messages
    /// and their small dependency closures; it never certifies history coverage.
    /// Channel metadata must already be in the acknowledged bulk prefix. First
    /// contact DMs carry their creation record. Large reply chains use bulk paging.
    pub fn export_live(
        &self,
        host: &str,
        interests: &BTreeSet<String>,
        covered: u64,
        after: u64,
        through: u64,
    ) -> Result<Value> {
        if !self.host_authorized(host, None) {
            return Err(err("host_revoked", "Messaging host is not enrolled"));
        }
        let owns = |actor: &str| {
            actor.starts_with(&format!("agent:{host}:"))
                || actor == "owner"
                    && self
                        .replication
                        .hosts
                        .get(host)
                        .is_some_and(|h| h["owner_access"] == true)
        };
        let mut items = Vec::new();
        let mut visited = BTreeSet::new();
        let mut bytes = 0;
        let mut scanned = after;
        for p in self
            .events
            .iter()
            .filter(|p| p.position > after && p.position <= through)
        {
            let e = &p.event;
            let conv = strv(e, "conversation_id");
            if e["type"] == "message.create"
                && self.eligible_for_host(host, interests, e)
                && (self.conversations.contains_key(conv)
                    || mention_recipients(e).iter().any(|m| owns(m)))
            {
                let creation = self
                    .events
                    .iter()
                    .find(|v| v.event["data"]["conversation_create"]["id"] == conv);
                let channel_known = self.events.iter().any(|v| {
                    v.position <= covered
                        && v.event["data"]["channels"]
                            .as_array()
                            .is_some_and(|cs| cs.iter().any(|c| c["id"] == conv))
                });
                if !channel_known && creation.is_none() {
                    scanned = p.position;
                    continue;
                }
                let mut ids = Vec::new();
                if let Some(c) = creation.filter(|v| v.position > covered) {
                    ids.push(required(&c.event, "id")?);
                }
                ids.extend(self.dependency_ids(e)?.into_iter().filter(|id| {
                    self.events
                        .iter()
                        .any(|v| v.event["id"] == *id && v.position > covered)
                }));
                ids.push(required(e, "id")?);
                let mut extra = Vec::new();
                let mut extra_ids = BTreeSet::new();
                let mut size = 0;
                for id in ids {
                    if visited.contains(&id) || !extra_ids.insert(id.clone()) {
                        continue;
                    }
                    let wire = self.wire_event(&id)?;
                    size += serde_json::to_vec(&wire)?.len();
                    extra.push(wire);
                }
                if extra.len() > 64 || size > 256 * 1024 {
                    scanned = p.position;
                    continue;
                }
                if items.len() + extra.len() > 64 || bytes + size > 256 * 1024 {
                    return Ok(json!({"items":items,"scanned":scanned}));
                }
                items.extend(extra);
                visited.extend(extra_ids);
                bytes += size;
            }
            scanned = p.position;
        }
        Ok(json!({"items":items,"scanned":through}))
    }
    /// At most 64 records / 256 KiB. `dependency_offset` resumes a long parent
    /// chain without claiming coverage for an incompletely delivered message.
    /// On reconnect it is safe to replay from offset zero: IDs are immutable.
    pub fn export_slice(
        &self,
        host: &str,
        interests: &BTreeSet<String>,
        after: u64,
        through: u64,
        mut dependency_offset: usize,
    ) -> Result<Value> {
        if !self.host_authorized(host, None) {
            return Err(err("host_revoked", "Messaging host is not enrolled"));
        }
        if through > self.position || after > through {
            return Err(err("resync_required", "Invalid replication boundary"));
        }
        let mut items = Vec::new();
        let mut bytes = 0;
        let mut cursor = after;
        let mut visited = BTreeSet::new();
        for p in self
            .events
            .iter()
            .filter(|p| p.position > after && p.position <= through)
        {
            if self.eligible_for_host(host, interests, &p.event) {
                let mut ids = self.dependency_ids(&p.event)?;
                ids.push(required(&p.event, "id")?);
                if dependency_offset >= ids.len() {
                    return Err(err(
                        "invalid_cursor",
                        "Dependency offset is outside the publication",
                    ));
                }
                for (offset, id) in ids.iter().enumerate().skip(dependency_offset) {
                    if visited.contains(id) {
                        continue;
                    }
                    let wire = self.wire_event(id)?;
                    let size = serde_json::to_vec(&wire)?.len();
                    if !items.is_empty() && (items.len() >= 64 || bytes + size > 256 * 1024) {
                        return Ok(
                            json!({"items":items,"cursor":cursor,"dependency_offset":offset,"through":through,"complete":false,"generation":self.generation,"space_id":self.space_id}),
                        );
                    }
                    bytes += size;
                    items.push(wire);
                    visited.insert(id.clone());
                }
                dependency_offset = 0;
            }
            cursor = p.position;
        }
        Ok(
            json!({"items":items,"cursor":through,"dependency_offset":0,"through":through,"complete":true,"generation":self.generation,"space_id":self.space_id}),
        )
    }
    pub fn record_coverage(&mut self, scope: &str, generation: &str, through: u64) -> Result<()> {
        Uuid::parse_str(generation).map_err(|_| err("invalid_cursor", "Invalid hub generation"))?;
        if self.replication.coverage.get(scope).is_some_and(|old| {
            old["generation"] == generation && old["through"].as_u64().is_some_and(|p| p >= through)
        }) {
            return Ok(());
        }
        let data = json!({"scope":scope,"coordinator_id":self.coordinator_id(),"generation":generation,"through":through,"complete":true});
        if self.replication.coverage.get(scope) == Some(&data) {
            return Ok(());
        }
        self.journal_status("coverage", None, None, data)
    }
    pub fn cached_coverage(&self, scope: &str) -> Value {
        if self.is_coordinator() {
            return json!({"status":"complete","through":self.position,"generation":self.generation});
        }
        let checkpoint = self
            .replication
            .coverage
            .get(scope)
            .or_else(|| self.replication.coverage.get("*"));
        json!({"status":if checkpoint.is_some(){"complete_through_checkpoint"}else{"partial"},"checkpoint":checkpoint,"connected":self.replication.connected})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Pair {
        _tmp: tempfile::TempDir,
        hub: Store,
        replica: Store,
        people: Vec<Person>,
    }
    fn pair() -> Pair {
        let tmp = tempfile::tempdir().unwrap();
        let mut hub = Store::open(&tmp.path().join("hub")).unwrap();
        let replica =
            Store::provision_replica(&tmp.path().join("replica"), &hub.space_descriptor()).unwrap();
        let people = ["a", "b"]
            .map(|uid| Person {
                id: replica.participant_id(uid),
                name: uid.into(),
                session_uid: uid.into(),
                task: None,
                present: true,
                kind: "agent".into(),
            })
            .to_vec();
        hub.register_host(&replica.daemon_id, true, true, &people)
            .unwrap();
        for p in &people {
            hub.coordinate(&replica.daemon_id,&p.id,"messaging.send",&json!({"channel":"general","body":"Ready","name":format!("Scout-{}",p.session_uid),"request_id":format!("claim-{}",p.session_uid)})).unwrap();
        }
        let mut pair = Pair {
            _tmp: tmp,
            hub,
            replica,
            people,
        };
        catch_up(&mut pair);
        pair
    }
    fn catch_up(pair: &mut Pair) {
        let mut after = 0;
        let through = pair.hub.position;
        loop {
            let page = pair
                .hub
                .export_page(&pair.replica.daemon_id, &BTreeSet::new(), after, through)
                .unwrap();
            for wire in page["items"].as_array().unwrap() {
                pair.replica.ingest_replica(wire).unwrap();
            }
            after = page["cursor"].as_u64().unwrap();
            if page["complete"] == true {
                break;
            }
        }
    }
    #[test]
    fn self_rename_preserves_identity_history_and_routing_across_replication_and_restart() {
        let mut p = pair();
        let actor = p.people[0].id.clone();
        let peer = p.people[1].id.clone();
        let host = p.replica.daemon_id.clone();
        let dm = p.hub.coordinate(&host, &peer, "messaging.send", &json!({"dm":actor,"body":"Review ready","request_id":"before-dm"})).unwrap();
        let mention = p.hub.coordinate(&host, &peer, "messaging.send", &json!({"channel":"general","body":"For Scout-a","mentions":[actor],"request_id":"before-mention"})).unwrap();
        catch_up(&mut p);
        let conversation = dm["event"]["conversation_id"].clone();
        let history = p.replica.read(&actor, &json!({"conversation":conversation}), &p.people).unwrap();
        p.replica.read(&actor, &json!({"conversation":conversation,"ack_receipt":history["receipt"]}), &p.people).unwrap();
        let memberships = p.replica.memberships.clone();
        let watch = p.replica.register_monitor(&actor, &json!({"scope":{"dm":peer},"request_id":"watch","mode":"continuous"}), &p.people).unwrap();
        let rename = json!({"name":"health-triage-orchestrator","expected_name_revision":p.replica.names[&actor].revision,"request_id":"rename"});
        let result = p.hub.coordinate(&host, &actor, "session.set_name", &rename).unwrap();
        assert_eq!(p.hub.coordinate(&host, &actor, "session.set_name", &rename).unwrap()["event_id"], result["event_id"]);
        assert_eq!(p.hub.coordinate(&host, &actor, "session.set_name", &json!({"name":"stale","expected_name_revision":1,"request_id":"stale"})).unwrap_err().code, "name_revision_conflict");
        let after = p.hub.coordinate(&host, &actor, "messaging.send", &json!({"dm":peer,"name":"Scout-a","body":"Approved","request_id":"after-dm"})).unwrap();
        assert_eq!(after["event"]["conversation_id"], conversation);
        assert_eq!(after["event"]["actor"]["id"], actor);
        assert_eq!(after["event"]["actor"]["name"], "health-triage-orchestrator");
        catch_up(&mut p);
        let root = p._tmp.path().join("replica");
        drop(p.replica);
        p.replica = Store::open(&root).unwrap();
        assert!(p.replica.degraded.is_none(), "{:?}", p.replica.degraded);
        assert_eq!(p.replica.names[&actor].name, "health-triage-orchestrator");
        assert!(p.replica.names[&actor].aliases.contains(&"Scout-a".to_string()));
        assert_eq!(p.replica.memberships, memberships);
        let history = p.replica.read(&actor, &json!({"conversation":conversation}), &p.people).unwrap();
        assert_eq!(history["items"].as_array().unwrap().len(), 2);
        let before = history["items"].as_array().unwrap().iter().find(|e| e["id"] == dm["event_id"]).unwrap();
        assert_eq!(before["read"], true);
        let inbox = p.replica.read(&actor, &json!({"inbox":true,"unread_only":true}), &p.people).unwrap();
        let historical_mention = inbox["items"].as_array().unwrap().iter().find(|e| e["id"] == mention["event_id"]).unwrap();
        assert_eq!(historical_mention["data"]["mentions"], json!([actor]));
        let watches = p.replica.monitors(&actor, &json!({"action":"list"})).unwrap();
        assert!(watches["items"].as_array().unwrap().iter().any(|w| w["id"] == watch["id"] && w["state"] == "active"));
    }

    #[test]
    fn messaging_channel_norms_coordinate_permissions_and_sync_without_global_corruption() {
        let mut p = pair();
        let actor = p.people[0].id.clone();
        let other = p.people[1].id.clone();
        let host = p.replica.daemon_id.clone();
        let global = p.hub.norms.clone();
        let channel = p.hub.coordinate(&host,&actor,"messaging.channels",&json!({"action":"create","path":"norms-test","request_id":"new-channel"})).unwrap()["channel"].clone();
        let scope = format!("channel:{}",channel["id"].as_str().unwrap());
        let request = json!({"action":"publish","scope":scope,"text":"Cross-host convention.\n","expected_revision":channel["id"],"summary":"Shared practice","request_id":"norms-publish"});
        assert_eq!(p.hub.coordinate(&host,&other,"messaging.norms",&request).unwrap_err().code,"unauthorized");
        let published = p.hub.coordinate(&host,&actor,"messaging.norms",&request).unwrap();
        catch_up(&mut p);
        assert_eq!(p.replica.norms,global);
        assert!(p.replica.needs_coordinator(&actor,"messaging.norms",&request,&p.people).unwrap());
        let doc = p.replica.norms_document(&actor,&json!({"channel":"norms-test"})).unwrap();
        assert_eq!(doc["revision"],published["revision"]);
        assert_eq!(doc["text"],"Cross-host convention.\n");
        assert_eq!(fs::read_to_string(p.replica.root.join("channels/norms-test/NORMS.md")).unwrap(),"Cross-host convention.\n");
        assert_eq!(p.hub.coordinate(&host,&actor,"messaging.norms",&request).unwrap()["revision"],published["revision"]);
    }
    #[test]
    fn live_dm_and_mention_bypass_bulk_without_certifying_missing_history() {
        let mut p = pair();
        let covered = p.hub.position;
        let actor = p.people[0].id.clone();
        let peer = p.people[1].id.clone();
        for i in 0..70 {
            p.hub.coordinate(&p.replica.daemon_id, &actor, "messaging.send",
                &json!({"channel":"general","body":"Backlog","request_id":format!("backlog-{i}")})).unwrap();
        }
        let live_start = p.hub.position;
        let dm = p
            .hub
            .coordinate(
                &p.replica.daemon_id,
                &actor,
                "messaging.send",
                &json!({"dm":peer,"body":"New private message","request_id":"live-dm"}),
            )
            .unwrap();
        let mention = p.hub.coordinate(&p.replica.daemon_id, &actor, "messaging.send",
            &json!({"channel":"general","body":"New mention","mentions":[peer],"request_id":"live-mention"})).unwrap();
        let page = p
            .hub
            .export_live(
                &p.replica.daemon_id,
                &BTreeSet::new(),
                covered,
                live_start,
                p.hub.position,
            )
            .unwrap();
        let items = page["items"].as_array().unwrap();
        assert_eq!(items.len(), 2, "{page}");
        for wire in items {
            p.replica.ingest_replica(wire).unwrap();
        }
        assert!(p
            .replica
            .events
            .iter()
            .any(|v| v.event["id"] == dm["event_id"]));
        assert!(p
            .replica
            .events
            .iter()
            .any(|v| v.event["id"] == mention["event_id"]));
        assert!(!p
            .replica
            .events
            .iter()
            .any(|v| v.event["body"] == "Backlog"));
        assert_eq!(p.replica.cached_coverage("general")["status"], "partial");
        let arrival = p
            .replica
            .events
            .iter()
            .find(|v| v.event["id"] == mention["event_id"])
            .unwrap()
            .position;
        catch_up(&mut p);
        assert_eq!(
            p.replica
                .events
                .iter()
                .filter(|v| v.event["id"] == mention["event_id"])
                .count(),
            1
        );
        assert_eq!(
            p.replica
                .events
                .iter()
                .find(|v| v.event["id"] == mention["event_id"])
                .unwrap()
                .position,
            arrival
        );
        let root = p._tmp.path().join("replica");
        drop(p.replica);
        let replica = Store::open(&root).unwrap();
        assert!(replica.degraded.is_none(), "{:?}", replica.degraded);
    }
    #[test]
    fn invalid_projection_inputs_leave_no_publication_or_poisoned_replay() {
        let mut p = pair();
        let original = p
            .hub
            .events
            .iter()
            .find(|e| e.event["type"] == "message.create")
            .unwrap()
            .event
            .clone();
        let mut cases = Vec::new();
        let mut invalid_dm = original.clone();
        invalid_dm["data"]["conversation_create"] =
            json!({"id":original["conversation_id"],"members":[]});
        cases.push(invalid_dm);
        let mut invalid_join = original.clone();
        invalid_join["type"] = json!("channel.membership");
        invalid_join["data"] = json!({"participant_id":original["actor"]["id"],"joined":"yes"});
        cases.push(invalid_join);
        for data in [
            json!({"action":"add_member","participant_id":"owner","joined":false}),
            json!({"action":"add_member","participant_id":"system","joined":true}),
            json!({"action":"add_member","participant_id":"","joined":true}),
            json!({"participant_id":"owner","joined":true}),
        ] {
            let mut invalid_add = original.clone();
            invalid_add["type"] = json!("channel.membership");
            invalid_add["data"] = data;
            cases.push(invalid_add);
        }
        let mut duplicate_enrollment = p
            .hub
            .events
            .iter()
            .find(|e| e.event["type"] == "membership.enrollment")
            .unwrap()
            .event
            .clone();
        duplicate_enrollment["body"] = json!("Malformed repeated enrollment");
        cases.push(duplicate_enrollment);
        let mut invalid_identity = original;
        invalid_identity["type"] = json!("identity.update");
        let (participant, name) = p.replica.names.iter().next().unwrap();
        let mut identity = serde_json::to_value(name).unwrap();
        identity["participant_id"] = json!(participant);
        identity["name"] = json!("Conflicting name");
        invalid_identity["data"] = json!({"identity":identity});
        cases.push(invalid_identity);
        for mut event in cases {
            event["id"] = json!(format!(
                "{}:{}",
                event["origin_daemon_id"].as_str().unwrap(),
                uuid()
            ));
            event["request"]["key"] = json!(uuid());
            let raw = serde_json::to_string(&event).unwrap();
            let receipt = json!({"space_id":p.hub.space_id,"coordinator_id":p.hub.daemon_id,"generation":p.hub.generation,
                "position":"9999","event_id":event["id"],"event_sha256":hash(raw.as_bytes())});
            let before = p.replica.position;
            assert!(p
                .replica
                .ingest_replica(&json!({"event":raw,"receipt":receipt}))
                .is_err());
            assert_eq!(p.replica.position, before);
            assert!(!p.replica.event_path(&event).unwrap().exists());
            assert!(p.replica.degraded.is_none());
        }
    }
    #[test]
    fn connection_failures_distinguish_blocked_from_pending_and_keep_coverage() {
        let mut p = pair();
        let sent = p
            .replica
            .send(
                &p.people[0].id,
                "a",
                "agent",
                &json!({"channel":"general","body":"Offline","request_id":"pending"}),
                &p.people,
            )
            .unwrap();
        let id = sent["event_id"].as_str().unwrap();
        p.replica
            .set_sync_connection(false, Some("storage_error: connection reset".into()));
        assert_eq!(p.replica.event_replication(id)["status"], "pending_sync");
        p.replica
            .set_sync_connection(false, Some("unauthorized: token invalid".into()));
        assert_eq!(
            p.replica.event_replication(id)["status"],
            "replication_blocked"
        );
        p.replica
            .record_coverage("general", &p.hub.generation, 50)
            .unwrap();
        p.replica
            .record_coverage("general", &p.hub.generation, 10)
            .unwrap();
        assert_eq!(
            p.replica.cached_coverage("general")["checkpoint"]["through"],
            50
        );
    }
    #[test]
    fn offline_messages_and_monitors_survive_receipt_loss_and_origin_restart() {
        let mut p = pair();
        let reader = &p.people[1];
        p.replica.register_monitor(&reader.id,&json!({"scope":{"channel":"general"},"mode":"continuous","notify":"none","request_id":"watch"}),&p.people).unwrap();
        let request = json!({"channel":"general","body":"Saved offline","request_id":"offline"});
        let sent = p
            .replica
            .send(&p.people[0].id, "a", "agent", &request, &p.people)
            .unwrap();
        assert_eq!(sent["replication"], "pending_sync");
        let id = sent["event_id"].as_str().unwrap().to_owned();
        assert!(p
            .replica
            .read(&reader.id, &json!({"channel":"general"}), &p.people)
            .unwrap()["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["id"] == id));
        let hits = p
            .replica
            .personal_state(&reader.id)
            .monitors
            .values()
            .next()
            .unwrap()
            .hit_high;
        let wire = p
            .replica
            .pending_uploads(64)
            .unwrap()
            .into_iter()
            .find(|w| w["event"].as_str().unwrap().contains("Saved offline"))
            .unwrap();
        let receipt = p.hub.accept_upload(&p.replica.daemon_id, &wire).unwrap();
        let count = p.hub.events.len();
        assert_eq!(
            p.hub.accept_upload(&p.replica.daemon_id, &wire).unwrap(),
            receipt
        );
        assert_eq!(p.hub.events.len(), count);
        let root = p
            .replica
            .root
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_owned();
        drop(p.replica);
        p.replica = Store::open(&root).unwrap();
        assert!(p.replica.degraded.is_none());
        assert_eq!(
            p.replica
                .send(&p.people[0].id, "a", "agent", &request, &p.people)
                .unwrap()["event_id"],
            id
        );
        let arrivals = p.replica.events.len();
        p.replica.record_receipt(&receipt).unwrap();
        assert!(!p
            .replica
            .ingest_replica(&p.hub.wire_event(&id).unwrap())
            .unwrap());
        assert_eq!(p.replica.events.len(), arrivals);
        assert_eq!(
            p.replica
                .personal_state(&reader.id)
                .monitors
                .values()
                .next()
                .unwrap()
                .hit_high,
            hits
        );
        assert!(p.replica.pending_uploads(64).unwrap().is_empty());
        drop(p.replica);
        p.replica = Store::open(&root).unwrap();
        assert_eq!(p.replica.event_replication(&id)["status"], "replicated");
    }
    #[test]
    fn metadata_stays_coordinated_and_offline_here_does_not_expand() {
        let mut p = pair();
        assert_eq!(
            p.replica
                .channel_action(
                    &p.people[0].id,
                    &json!({"action":"create","path":"offline","request_id":"new"}),
                    &p.people
                )
                .unwrap_err()
                .code,
            "coordinator_required"
        );
        let e=p.replica.send(&p.people[0].id,"a","agent",&json!({"channel":"general","body":"Old audience","mention_here":true,"request_id":"here"}),&p.people).unwrap();
        let newcomer = Person {
            id: p.replica.participant_id("new"),
            name: "New".into(),
            session_uid: "new".into(),
            task: None,
            present: true,
            kind: "agent".into(),
        };
        p.people.push(newcomer.clone());
        p.hub
            .register_host(&p.replica.daemon_id, true, true, &p.people)
            .unwrap();
        let id = e["event_id"].as_str().unwrap();
        p.hub
            .accept_upload(&p.replica.daemon_id, &p.replica.wire_event(id).unwrap())
            .unwrap();
        let retained = p.hub.events.iter().find(|v| v.event["id"] == id).unwrap();
        assert!(!mention_recipients(&retained.event).contains(&newcomer.id.as_str()));
    }
    #[test]
    fn admin_member_add_replicates_and_validates_offline_here_audience() {
        let mut p = pair();
        let host = p.replica.daemon_id.clone();
        let admin = p.people[0].id.clone();
        let member = p.people[1].id.clone();
        p.hub.coordinate(&host, &admin, "messaging.channels",
            &json!({"action":"create","path":"team","request_id":"create-team"})).unwrap();
        let cid = p.hub.channels["team"].clone();
        catch_up(&mut p);
        let add = json!({"action":"add_member","conversation":cid,"participant_id":member,"request_id":"add-team"});
        assert_eq!(p.replica.channel_action(&admin, &add, &p.people).unwrap_err().code, "coordinator_required");
        assert_eq!(p.hub.coordinate(&host, &member, "messaging.channels", &add).unwrap_err().code, "unauthorized");
        let added = p.hub.coordinate(&host, &admin, "messaging.channels", &add).unwrap();
        catch_up(&mut p);
        assert!(p.replica.joined(&member, &cid));
        assert_eq!(p.replica.membership_revisions[&cid], added["event_id"]);
        assert_eq!(p.replica.members_at_revision(&cid, added["event_id"].as_str().unwrap()).unwrap(),
            BTreeSet::from([admin.clone(), member.clone()]));
        // The new member can send offline, and @here uses the replicated roster.
        let posted = p.replica.send(&member, "b", "agent", &json!({"channel":"team","body":"Ready", "mention_here":true,
            "request_id":"member-post"}), &p.people).unwrap();
        let wire = p.replica.wire_event(posted["event_id"].as_str().unwrap()).unwrap();
        p.hub.accept_upload(&host, &wire).unwrap();
        assert!(mention_recipients(&posted["event"]).contains(&admin.as_str()));
        p.hub.coordinate(&host, &member, "messaging.channels",
            &json!({"action":"leave","conversation":cid,"request_id":"leave-team"})).unwrap();
        let retry = p.hub.coordinate(&host, &admin, "messaging.channels", &add).unwrap();
        assert_eq!(retry["membership"]["current_joined"], false);
        catch_up(&mut p);
        let root = p._tmp.path().join("replica");
        drop(p.replica);
        let replica = Store::open(&root).unwrap();
        assert!(replica.degraded.is_none(), "{:?}", replica.degraded);
        assert!(!replica.joined(&member, &cid));
        // A replica cannot forge an admin-add record instead of using the hub.
        let mut forged = added["event"].clone();
        forged["id"] = json!(format!("{host}:{}", uuid()));
        forged["origin_daemon_id"] = json!(host);
        forged["request"]["key"] = json!(uuid());
        let before = p.hub.position;
        assert_eq!(p.hub.accept_upload(&host, &json!({"event":serde_json::to_string(&forged).unwrap()}))
            .unwrap_err().code, "unauthorized");
        assert_eq!(p.hub.position, before);
    }
    #[test]
    fn revoked_uploads_and_dependent_replies_retain_local_context() {
        let mut p = pair();
        let a = &p.people[0];
        let parent = p
            .replica
            .send(
                &a.id,
                "a",
                "agent",
                &json!({"channel":"general","body":"Parent","request_id":"parent"}),
                &p.people,
            )
            .unwrap();
        let child=p.replica.send(&a.id,"a","agent",&json!({"channel":"general","body":"Child","reply_to":parent["event_id"],"request_id":"child"}),&p.people).unwrap();
        let id = parent["event_id"].as_str().unwrap();
        p.hub
            .register_host(&p.replica.daemon_id, false, true, &p.people)
            .unwrap();
        let error = p
            .hub
            .accept_upload(&p.replica.daemon_id, &p.replica.wire_event(id).unwrap())
            .unwrap_err();
        assert_eq!(error.code, "host_revoked");
        p.replica.reject_replication(id, "host_revoked").unwrap();
        assert_eq!(
            p.replica
                .event_replication(child["event_id"].as_str().unwrap())["decision"]["reason"],
            "dependency_rejected"
        );
        assert!(p.replica.events.iter().any(|v| v.event["id"] == id));
        assert!(!p
            .replica
            .wake_intents()
            .values()
            .flatten()
            .any(|w| w.event_id == id));
        p.hub
            .register_host(&p.replica.daemon_id, true, true, &p.people)
            .unwrap();
        assert_eq!(
            p.hub
                .accept_upload(&p.replica.daemon_id, &p.replica.wire_event(id).unwrap())
                .unwrap_err()
                .code,
            "enrollment_revoked"
        );
    }
    #[test]
    fn foreign_namespace_claims_and_changed_event_bytes_are_rejected_before_commit() {
        let mut p = pair();
        let e = p
            .replica
            .send(
                &p.people[0].id,
                "a",
                "agent",
                &json!({"channel":"general","body":"Ordinary","request_id":"ordinary"}),
                &p.people,
            )
            .unwrap();
        let wire = p
            .replica
            .wire_event(e["event_id"].as_str().unwrap())
            .unwrap();
        let mut forged = e["event"].clone();
        forged["data"]["channels"] = json!([{"path":"forged","id":uuid()}]);
        let count = p.hub.events.len();
        assert_eq!(
            p.hub
                .accept_upload(
                    &p.replica.daemon_id,
                    &json!({"event":serde_json::to_string(&forged).unwrap()})
                )
                .unwrap_err()
                .code,
            "unauthorized"
        );
        assert_eq!(p.hub.events.len(), count);
        p.hub.accept_upload(&p.replica.daemon_id, &wire).unwrap();
        let changed = json!({"event":format!("{}\n",wire["event"].as_str().unwrap())});
        assert_eq!(
            p.hub
                .accept_upload(&p.replica.daemon_id, &changed)
                .unwrap_err()
                .code,
            "event_conflict"
        );
    }
    #[test]
    fn queued_pre_archive_posts_sync_as_delayed_but_known_archive_refuses_new_posts() {
        let mut p = pair();
        let actor = p.people[0].id.clone();
        let sent = p
            .replica
            .send(
                &actor,
                "a",
                "agent",
                &json!({"channel":"general","body":"Queued before archive","request_id":"queued"}),
                &p.people,
            )
            .unwrap();
        let channel = p.hub.channels["general"].clone();
        let rev = p.hub.channel_info("general", &channel)["revision"].clone();
        p.hub.channel_action("owner",&json!({"action":"update","path":"general","archived":true,"expected_revision":rev,"request_id":"archive"}),&p.people).unwrap();
        let wire = p
            .replica
            .wire_event(sent["event_id"].as_str().unwrap())
            .unwrap();
        let receipt = p.hub.accept_upload(&p.replica.daemon_id, &wire).unwrap();
        assert_eq!(receipt["accepted_from_stale_revision"], true);
        catch_up(&mut p);
        assert_eq!(p.replica.send(&actor,"a","agent",&json!({"channel":"general","body":"After observing archive","request_id":"too-late"}),&p.people).unwrap_err().code,"conversation_archived");
        assert_eq!(
            p.replica
                .wire_event(sent["event_id"].as_str().unwrap())
                .unwrap()["event"],
            wire["event"]
        );
    }
    #[test]
    fn long_reply_dependencies_page_without_losing_the_selected_message() {
        let mut p = pair();
        let receiver = Uuid::new_v4().to_string();
        let person = Person {
            id: format!("agent:{receiver}:reader"),
            name: "Reader".into(),
            session_uid: "reader".into(),
            task: None,
            present: true,
            kind: "agent".into(),
        };
        p.hub
            .register_host(&receiver, true, false, std::slice::from_ref(&person))
            .unwrap();
        p.hub
            .channel_action(
                &p.people[0].id,
                &json!({"action":"create","path":"chain","request_id":"chain"}),
                &p.people,
            )
            .unwrap();
        let mut parent = Value::Null;
        for i in 0..130 {
            parent=p.hub.coordinate(&p.replica.daemon_id,&p.people[0].id,"messaging.send",&json!({"channel":"chain","body":format!("Reply {i}"),"reply_to":parent,"request_id":format!("reply-{i}"),"mentions":if i==129 {json!([person.id])}else{json!([])}})).unwrap()["event_id"].clone();
        }
        let high = p.hub.position;
        let mut cursor = 0;
        let mut offset = 0;
        let mut pages = 0;
        let mut seen = BTreeSet::new();
        loop {
            let page = p
                .hub
                .export_slice(&receiver, &BTreeSet::new(), cursor, high, offset)
                .unwrap();
            assert!(page["items"].as_array().unwrap().len() <= 64);
            for wire in page["items"].as_array().unwrap() {
                let event: Value = serde_json::from_str(wire["event"].as_str().unwrap()).unwrap();
                if let Some(id) = event["data"]["reply_to"].as_str() {
                    assert!(seen.contains(id));
                }
                seen.insert(event["id"].as_str().unwrap().to_owned());
            }
            cursor = page["cursor"].as_u64().unwrap();
            offset = page["dependency_offset"].as_u64().unwrap() as usize;
            pages += 1;
            assert!(pages < 10);
            if page["complete"] == true {
                break;
            }
        }
        assert!(pages >= 3);
        assert!(seen.contains(parent.as_str().unwrap()));
    }
}

impl Store {
    pub fn host_info(&self, host: &str) -> Option<Value> {
        self.replication.hosts.get(host).cloned()
    }
    pub fn host_transport_interests(
        &self,
        host: &str,
        requested: &BTreeSet<String>,
    ) -> Result<BTreeSet<String>> {
        if !self.host_authorized(host, None) {
            return Err(err("host_revoked", "Messaging host is not enrolled"));
        }
        let owns = |actor: &str| {
            actor.starts_with(&format!("agent:{host}:"))
                || actor == "owner" && self.replication.hosts[host]["owner_access"] == true
        };
        let mut requested = requested.clone();
        for selector in requested.clone() {
            if let Some(event) = selector.strip_prefix("thread:") {
                if let Some(conv) = self
                    .events
                    .iter()
                    .find(|e| e.event["id"] == event)
                    .and_then(|e| e.event["conversation_id"].as_str())
                {
                    requested.insert(conv.into());
                }
            }
        }
        let mut out = BTreeSet::new();
        for (path, id) in &self.channels {
            if requested.contains("*")
                || requested.contains(path)
                || requested.contains(id)
                || self
                    .memberships
                    .get(id)
                    .is_some_and(|members| members.iter().any(|m| owns(m)))
            {
                out.insert(id.clone());
            }
        }
        for (id, members) in &self.conversations {
            if members.iter().any(|m| owns(m)) {
                out.insert(id.clone());
            }
        }
        Ok(out)
    }
    pub fn local_transport_interests(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for (actor, personal) in &self.personal {
            if actor != "owner" && !actor.starts_with(&format!("agent:{}:", self.daemon_id)) {
                continue;
            }
            for scope in personal.preferences.rules.values().map(|r| &r.scope).chain(
                personal
                    .monitors
                    .values()
                    .filter(|m| m.state == "active")
                    .map(|m| &m.scope),
            ) {
                for (path, id) in &self.channels {
                    let follows = scope.channel.as_deref().is_some_and(|base| {
                        base == "*"
                            || base == path
                            || scope.include_children && path.starts_with(&format!("{base}/"))
                    }) || scope.conversation.as_ref() == Some(id)
                        || scope.thread.as_deref().is_some_and(|thread| {
                            self.events.iter().any(|e| {
                                e.event["id"] == thread && e.event["conversation_id"] == *id
                            })
                        });
                    if follows {
                        out.insert(id.clone());
                    }
                }
            }
        }
        out
    }
    pub fn record_download_checkpoint(
        &mut self,
        generation: &str,
        cursor: u64,
        revision: u64,
        scopes: &BTreeSet<String>,
    ) -> Result<()> {
        Uuid::parse_str(generation).map_err(|_| err("invalid_cursor", "Invalid hub generation"))?;
        self.journal_status("coverage",None,None,json!({"scope":"transport","coordinator_id":self.coordinator_id(),"generation":generation,"cursor":cursor,"revision":revision,"scopes":scopes,"complete":false}))
    }
    pub fn download_checkpoint(&self) -> Value {
        self.replication
            .coverage
            .get("transport")
            .cloned()
            .unwrap_or(Value::Null)
    }
}

impl Store {
    pub fn needs_coordinator(
        &self,
        actor: &str,
        method: &str,
        p: &Value,
        people: &[Person],
    ) -> Result<bool> {
        if self.is_coordinator() {
            return Ok(false);
        }
        Ok(match method {
            "messaging.send" => {
                p["origin_daemon_id"]
                    .as_str()
                    .is_some_and(|o| o != self.daemon_id)
                    || actor != "owner" && !self.names.contains_key(actor)
                    || !self.enrolled.contains(actor)
                    || self
                        .resolve(actor, p, people, true)
                        .map(|(_, new)| new.is_some())
                        .unwrap_or(true)
            }
            "messaging.channels" => matches!(
                p["action"].as_str(),
                Some("create" | "update" | "join" | "leave" | "add_member")
            ),
            "messaging.pins" => matches!(
                p["action"].as_str(),
                Some("pin" | "unpin" | "set" | "remove")
            ),
            "messaging.norms" => matches!(p["action"].as_str(), Some("publish" | "revert")),
            "session.set_name" => true,
            "messaging.follow" | "messaging.monitor" | "messaging.monitors" if actor == "owner" => {
                true
            }
            _ => false,
        })
    }
    pub fn query_interest(&self, p: &Value) -> Option<String> {
        if let Some(id) = p["thread"].as_str() {
            return self
                .events
                .iter()
                .find(|e| e.event["id"] == id)
                .and_then(|e| e.event["conversation_id"].as_str().map(str::to_owned))
                .or_else(|| Some(format!("thread:{id}")));
        }
        if let Some(path) = p["channel"].as_str() {
            return Some(path.into());
        }
        if let Some(id) = p["conversation"].as_str() {
            return Some(id.into());
        }
        if p["inbox"] == true {
            return None;
        }
        if p["dm"].is_null() && p["dms"] != true {
            return Some("general".into());
        }
        None
    }
    pub fn decorate_sync_response(&self, p: &Value, result: &mut Value) {
        if !result.is_object() {
            return;
        }
        let mut scope = self.query_interest(p).unwrap_or_else(|| "metadata".into());
        if let Some(id) = self.channels.get(&scope) {
            scope = id.clone();
        }
        result["sync"] = self.sync_status();
        result["cache"] = self.cached_coverage(&scope);
        result["connection"] = json!(if self.is_coordinator() {
            if self.sync_enabled() {
                "hub"
            } else {
                "local"
            }
        } else if self.replication.connected {
            "connected"
        } else {
            "offline"
        });
        if result.get("coverage").is_some() && !self.is_coordinator() {
            result["coverage"] = result["cache"]["status"].clone();
            result["hub_completeness_note"]=json!("Coverage ends at the acknowledged hub checkpoint; disconnected origins may still hold pending messages");
        }
        if let Some(recent) = result.get_mut("recent") {
            self.decorate_sync_response(p, recent);
        }
        if let Some(items) = result["items"].as_array_mut() {
            for item in items {
                if let Some(id) = item["id"].as_str().map(str::to_owned) {
                    if self.events.iter().any(|e| e.event["id"] == id) {
                        item["replication"] = self.event_replication(&id);
                    }
                }
            }
        }
    }
}

impl Store {
    pub fn revocation_wire(&self, host: &str) -> Result<Value> {
        let event = self
            .events
            .iter()
            .rev()
            .find(|e| {
                e.event["type"] == "host.update"
                    && e.event["data"]["host_id"] == host
                    && e.event["data"]["active"] == false
            })
            .ok_or_else(|| err("not_found", "Host revocation record is missing"))?;
        self.wire_event(strv(&event.event, "id"))
    }
}
