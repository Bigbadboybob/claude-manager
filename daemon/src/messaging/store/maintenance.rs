//! `cm.maintenance`: the hub's maintenance notice as a replicated,
//! system-authored event. See `crate::maintenance` for the model.
use super::*;
use crate::maintenance;

impl Store {
    /// Shape check shared by local reduce and replica ingest preflight.
    pub(super) fn validate_maintenance(e: &Value) -> Result<()> {
        if e["actor"]["id"] != "system" || !e["conversation_id"].is_null() {
            return Err(err("invalid_record", "A maintenance notice is system-authored and conversation-free"));
        }
        let d = &e["data"];
        if !d["active"].is_boolean() || !d["changed_at"].is_string() {
            return Err(err("invalid_record", "A maintenance notice needs active and changed_at"));
        }
        Ok(())
    }

    pub(super) fn reduce_maintenance(&mut self, e: &Value) -> Result<()> {
        Self::validate_maintenance(e)?;
        // A failed projection never fails the event; the next notice or
        // restart rewrites it.
        if let Err(error) = maintenance::project(self.cm_root(), &e["data"]) {
            eprintln!("cm messaging: maintenance projection: {error}");
        }
        Ok(())
    }

    /// Hub only: replicate `record` (a `crate::maintenance` record) to every
    /// host. Returns false without publishing on a replica, whose own
    /// operator call covers it locally.
    pub fn publish_maintenance(&mut self, record: &Value) -> Result<bool> {
        if !self.is_coordinator() {
            return Ok(false);
        }
        let key = format!(
            "maintenance-v1:{}:{}",
            strv(record, "id"),
            strv(record, "changed_at")
        );
        if self.prior_system_event(&key).is_some() {
            return Ok(true);
        }
        let body = if record["active"] == true {
            format!("CM maintenance: {}", strv(record, "reason"))
        } else {
            "CM maintenance cleared".to_owned()
        };
        self.publish(maintenance::EVENT_TYPE, None, &body, record.clone(), "system", "System", "system", &key, "")?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hub_projects_its_own_notice_and_a_replica_cannot_publish_one() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = Store::open(tmp.path()).unwrap();
        let now = Utc::now();
        let (_, rec) = maintenance::apply_local(tmp.path(), &json!({"action":"set"}), now).unwrap();
        let rec = rec.unwrap();
        assert!(s.publish_maintenance(&rec).unwrap());
        let count = s.events.len();
        assert!(s.publish_maintenance(&rec).unwrap(), "idempotent per record");
        assert_eq!(s.events.len(), count);
        let projected: Value = serde_json::from_slice(&std::fs::read(maintenance::projection_path(tmp.path())).unwrap()).unwrap();
        assert_eq!(projected["id"], rec["id"]);
        // A forged author is refused.
        let mut forged = s.events.iter().find(|e| e.event["type"] == maintenance::EVENT_TYPE).unwrap().event.clone();
        forged["actor"]["id"] = json!("owner");
        assert!(Store::validate_maintenance(&forged).is_err());
    }
}
