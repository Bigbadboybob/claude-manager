//! Atomic participant-owned state. A single replacement also commits the
//! idempotency result, so a lost response never repeats a private mutation.
use super::*;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Personal {
    pub actor: String,
    pub revision: u64,
    pub bell_position: u64,
    pub bell_monitors: BTreeMap<String, u64>,
    pub norms_ack: Option<String>,
    pub channel_norms_ack: BTreeMap<String, String>,
    pub norms_supplied: BTreeSet<String>,
    pub norms_offered: BTreeSet<String>,
    pub norm_pages: BTreeMap<String, NormPage>,
    pub operations: BTreeMap<String, Operation>,
    pub monitor_receipts: BTreeSet<String>,
    pub monitors: BTreeMap<String, super::watches::Monitor>,
    pub preferences: super::preferences::Preferences,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct NormPage {
    #[serde(default = "global_scope")]
    pub scope: String,
    pub revision: String,
    pub since: Option<String>,
    pub action: String,
    pub digest: String,
    pub through: usize,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Operation {
    pub digest: String,
    pub result: Value,
}

impl Store {
    pub(super) fn personal_path(&self, actor: &str) -> PathBuf {
        self.root
            .join("_state/participants")
            .join(format!("{}.json", hash(actor.as_bytes())))
    }

    pub(super) fn load_personal(&mut self) -> Result<()> {
        let dir = self.root.join("_state/participants");
        mkdir(&dir)?;
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let state: Personal = serde_json::from_value(load(&path)?)?;
            if state.actor.is_empty() || path != self.personal_path(&state.actor) {
                return Err(err(
                    "invalid_personal_state",
                    "Participant state has the wrong owner",
                ));
            }
            self.personal.insert(state.actor.clone(), state);
        }
        Ok(())
    }

    pub(super) fn personal_state(&self, actor: &str) -> Personal {
        self.personal
            .get(actor)
            .cloned()
            .unwrap_or_else(|| Personal {
                actor: actor.into(),
                ..Personal::default()
            })
    }

    pub(super) fn save_personal(&mut self, mut state: Personal) -> Result<()> {
        self.ensure_messaging_writable()?;
        if let Some(reason) = &self.degraded {
            return Err(err("store_read_only", reason.clone()));
        }
        state.revision = self
            .personal
            .get(&state.actor)
            .map(|s| s.revision)
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| err("counter_overflow", "Personal state revision exhausted"))?;
        if let Err(e) = atomic_replace(
            &self.personal_path(&state.actor),
            &serde_json::to_value(&state)?,
        ) {
            // Rename may have succeeded before directory fsync failed. Do not
            // overwrite uncertain state with an older in-memory copy.
            self.degraded = Some(format!(
                "Personal-state outcome requires reconciliation: {e}"
            ));
            return Err(err("outcome_unknown", self.degraded.clone().unwrap()));
        }
        self.personal.insert(state.actor.clone(), state);
        self.replication.signal.signal();
        Ok(())
    }

    /// Monitor scans stage only changed monitors. Borrow the other potentially
    /// large histories for encoding; never mutate committed state before the
    /// durable write, including when rename succeeds but directory fsync fails.
    pub(super) fn save_monitor_updates(
        &mut self,
        actor: &str,
        updates: BTreeMap<String, super::watches::Monitor>,
    ) -> Result<()> {
        self.ensure_messaging_writable()?;
        if let Some(reason) = &self.degraded {
            return Err(err("store_read_only", reason.clone()));
        }
        let state = &self.personal[actor];
        let revision = state
            .revision
            .checked_add(1)
            .ok_or_else(|| err("counter_overflow", "Personal state revision exhausted"))?;
        let bytes = serde_json::to_vec_pretty(&state.monitor_checkpoint(revision, &updates))?;
        if let Err(e) = write_file(&self.personal_path(actor), &bytes, true) {
            self.degraded = Some(format!(
                "Personal-state outcome requires reconciliation: {e}"
            ));
            return Err(err("outcome_unknown", self.degraded.clone().unwrap()));
        }
        let state = self
            .personal
            .get_mut(actor)
            .expect("monitor owner remains present");
        state.revision = revision;
        state.monitors.extend(updates);
        self.replication.signal.signal();
        Ok(())
    }

    pub(super) fn personal_request(
        &self,
        actor: &str,
        p: &Value,
        operation: &str,
    ) -> Result<(String, String, Option<Value>)> {
        let key = required(p, "request_id")?;
        if key.len() > 160 || key.chars().any(char::is_control) {
            return Err(err("invalid_params", "Invalid request_id"));
        }
        if p["origin_daemon_id"]
            .as_str()
            .is_some_and(|o| o != self.operation_origin())
        {
            return Err(err(
                "retry_origin_unavailable",
                "Retry through the original daemon",
            ));
        }
        if self.requests.contains_key(&format!("{actor}\n{key}")) {
            return Err(err(
                "idempotency_conflict",
                "This request_id belongs to a shared mutation",
            ));
        }
        let mut intent = p.clone();
        for field in ["origin_daemon_id", "norms_seen", "ack_receipt"] {
            intent
                .as_object_mut()
                .ok_or_else(|| err("invalid_params", "Expected object"))?
                .remove(field);
        }
        let digest = hash(&serde_json::to_vec(
            &json!({"operation":operation,"params":intent}),
        )?);
        let prior = self
            .personal
            .get(actor)
            .and_then(|s| s.operations.get(&key));
        if prior.is_some_and(|old| old.digest != digest) {
            return Err(err(
                "idempotency_conflict",
                "This request_id already has different content",
            ));
        }
        Ok((key, digest, prior.map(|op| op.result.clone())))
    }

    pub(super) fn commit_personal_operation(
        &mut self,
        mut state: Personal,
        key: String,
        digest: String,
        result: Value,
    ) -> Result<Value> {
        state.operations.insert(
            key,
            Operation {
                digest,
                result: result.clone(),
            },
        );
        self.save_personal(state)?;
        Ok(result)
    }
}

fn global_scope() -> String {
    "global".into()
}
impl Personal {
    fn monitor_checkpoint<'a>(
        &'a self,
        revision: u64,
        updates: &'a BTreeMap<String, super::watches::Monitor>,
    ) -> impl Serialize + 'a {
        #[derive(Serialize)]
        struct Checkpoint<'a> {
            actor: &'a str,
            revision: u64,
            bell_position: u64,
            bell_monitors: &'a BTreeMap<String, u64>,
            norms_ack: &'a Option<String>,
            channel_norms_ack: &'a BTreeMap<String, String>,
            norms_supplied: &'a BTreeSet<String>,
            norms_offered: &'a BTreeSet<String>,
            norm_pages: &'a BTreeMap<String, NormPage>,
            operations: &'a BTreeMap<String, Operation>,
            monitor_receipts: &'a BTreeSet<String>,
            monitors: MonitorUpdates<'a>,
            preferences: &'a super::preferences::Preferences,
        }
        // Exhaustive destructuring makes any new Personal field a compile-time
        // obligation here, rather than silently dropping it from checkpoints.
        let Personal {
            actor,
            revision: _,
            bell_position,
            bell_monitors,
            norms_ack,
            channel_norms_ack,
            norms_supplied,
            norms_offered,
            norm_pages,
            operations,
            monitor_receipts,
            monitors,
            preferences,
        } = self;
        Checkpoint {
            actor,
            revision,
            bell_position: *bell_position,
            bell_monitors,
            norms_ack,
            channel_norms_ack,
            norms_supplied,
            norms_offered,
            norm_pages,
            operations,
            monitor_receipts,
            monitors: MonitorUpdates { monitors, updates },
            preferences,
        }
    }

    pub(super) fn norms_ack_for(&self, scope: &str) -> Option<&str> {
        if scope == "global" {
            self.norms_ack.as_deref()
        } else {
            self.channel_norms_ack.get(scope).map(String::as_str)
        }
    }
}

struct MonitorUpdates<'a> {
    monitors: &'a BTreeMap<String, super::watches::Monitor>,
    updates: &'a BTreeMap<String, super::watches::Monitor>,
}
impl Serialize for MonitorUpdates<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.monitors.len()))?;
        for (id, old) in self.monitors {
            map.serialize_entry(id, self.updates.get(id).unwrap_or(old))?;
        }
        map.end()
    }
}
