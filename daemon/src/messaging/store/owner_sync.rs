//! Owner watches execute at the coordinator. Other hosts mirror preferences and
//! summaries; local drafts, scroll and notification delivery bookkeeping stay local.
use super::*;
impl Store {
    pub fn owner_snapshot(&mut self) -> Result<Value> {
        self.advance_monitors_at(Utc::now())?;
        let state = self.personal_state("owner");
        Ok(
            json!({"execution_host":self.coordinator_id(),"state_revision":state.revision,"preferences":state.preferences,
            "monitor_status":self.monitor_status("owner"),"monitors":self.monitors("owner",&json!({"action":"list","limit":200}))?,
            "position":self.position_token(self.position)}),
        )
    }
    pub fn apply_owner_snapshot(&mut self, snapshot: &Value) -> Result<()> {
        if self.is_coordinator()
            || !self
                .replication
                .hosts
                .get(&self.daemon_id)
                .is_some_and(|h| h["owner_access"] == true)
        {
            return Err(err(
                "unauthorized",
                "This host has no Owner service binding",
            ));
        }
        if snapshot["execution_host"] != self.coordinator_id() {
            return Err(err("unauthorized", "Unexpected Owner monitor executor"));
        }
        if self.replication.owner_snapshot.as_ref().is_some_and(|old| {
            old["state_revision"].as_u64().unwrap_or(0)
                > snapshot["state_revision"].as_u64().unwrap_or(0)
        }) {
            return Ok(());
        }
        let mut preferences: super::preferences::Preferences =
            serde_json::from_value(snapshot["preferences"].clone())?;
        let mut state = self.personal_state("owner");
        // Keep source boundaries distinct from this replica's arrival positions.
        for (id, rule) in &mut preferences.rules {
            let source = rule.since;
            let old = state.preferences.rules.get(id);
            rule.hub_since = Some(source);
            rule.since = old
                .filter(|old| old.hub_since == Some(source))
                .map(|old| old.since)
                .unwrap_or(self.position);
            rule.wake = false; // Owner receives passive badges; never wakes an agent.
        }
        if serde_json::to_value(&preferences)? != serde_json::to_value(&state.preferences)? {
            state.preferences = preferences;
            self.save_personal(state)?;
        }
        let path = self.root.join("_state/owner-sync.json");
        if self.replication.owner_snapshot.as_ref() != Some(snapshot) {
            if let Err(e) = atomic_replace(&path, snapshot) {
                self.degraded = Some(format!(
                    "Owner snapshot outcome requires reconciliation: {e}"
                ));
                return Err(err("outcome_unknown", self.degraded.clone().unwrap()));
            }
            self.replication.owner_snapshot = Some(snapshot.clone());
        }
        Ok(())
    }
    pub fn cached_owner_snapshot(&self) -> Option<&Value> {
        self.replication.owner_snapshot.as_ref()
    }
    pub fn owner_execution_position(&self, position: &Value) -> Result<Value> {
        if position["replica_id"] == self.coordinator_id() {
            return Ok(position.clone());
        }
        let local = self.check_position(position)?;
        // A local position is not a global arrival fence. Translate to the most
        // recent retained hub checkpoint known at that exact local boundary.
        for n in (1..=local).rev() {
            let path = self.journal_dir().join(format!("{n:020}.json"));
            if !path.exists() {
                continue;
            }
            let j = load(&path)?;
            if j["kind"] == "coverage" && j["data"]["scope"] == "transport" {
                return Ok(
                    json!({"space_id":self.space_id,"replica_id":self.coordinator_id(),"generation":j["data"]["generation"],"position":j["data"]["cursor"].as_u64().unwrap_or(0)}),
                );
            }
        }
        Err(err(
            "resync_required",
            "Read hub-fresh history before creating an Owner monitor from a local position",
        ))
    }
}
