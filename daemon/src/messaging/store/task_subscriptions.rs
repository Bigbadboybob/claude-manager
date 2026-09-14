//! Scheduler-bound subscriptions are distinct from a session's personal watches.
use super::watches::{Monitor, Scope};
use super::*;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskBinding {
    pub id: String,
    pub task_id: String,
    pub channel_id: String,
    pub actor: String,
    pub revision: u64,
    pub active: bool,
    #[serde(default)]
    pub acknowledged: BTreeSet<String>,
}
impl Store {
    fn task_binding_path(&self, id: &str) -> Result<PathBuf> {
        Uuid::parse_str(id).map_err(|_| err("invalid_binding", "Invalid subscription identity"))?;
        Ok(self
            .root
            .join("_state/task-subscriptions")
            .join(format!("{id}.json")))
    }
    pub(super) fn load_task_bindings(&mut self) -> Result<()> {
        let dir = self.root.join("_state/task-subscriptions");
        mkdir(&dir)?;
        for path in fs::read_dir(dir)? {
            let path = path?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let binding: TaskBinding = serde_json::from_value(load(&path)?)?;
            if path != self.task_binding_path(&binding.id)? {
                return Err(err("invalid_binding", "Task subscription path mismatch"));
            }
            self.task_bindings.insert(binding.id.clone(), binding);
        }
        Ok(())
    }
    fn save_task_binding(&mut self, binding: TaskBinding) -> Result<()> {
        self.ensure_messaging_writable()?;
        if let Some(reason) = &self.degraded {
            return Err(err("store_read_only", reason.clone()));
        }
        if let Err(e) = atomic_replace(
            &self.task_binding_path(&binding.id)?,
            &serde_json::to_value(&binding)?,
        ) {
            self.degraded = Some(format!(
                "Task checkpoint outcome requires reconciliation: {e}"
            ));
            return Err(err("outcome_unknown", self.degraded.clone().unwrap()));
        }
        self.task_bindings.insert(binding.id.clone(), binding);
        self.replication.signal.signal();
        Ok(())
    }
    /// Only daemon scheduler code calls this. Channel/UID assertions from an
    /// agent tool are never used to transfer a subscription.
    pub fn bind_task_subscription(&mut self, mut next: TaskBinding) -> Result<Value> {
        self.task_binding_path(&next.id)?;
        if !self.channels.values().any(|id| id == &next.channel_id)
            || !next
                .actor
                .starts_with(&format!("agent:{}:", self.daemon_id))
        {
            return Err(err(
                "invalid_binding",
                "Task subscription requires a known channel and a local scheduler session",
            ));
        }
        if let Some(old) = self.task_bindings.get(&next.id) {
            if old.task_id != next.task_id
                || old.channel_id != next.channel_id
                || old.revision > next.revision
                || old.revision == next.revision && old.actor != next.actor
            {
                return Err(err(
                    "stale_binding",
                    "Task subscription binding is stale or conflicts",
                ));
            }
            next.acknowledged.extend(old.acknowledged.iter().cloned());
        }
        let monitor_id = format!("task-{}-{}", next.id, next.revision);
        // Persist the fencing revision before changing any private watch. A
        // crash here is repaired by replaying this same scheduler binding.
        if self.task_bindings.get(&next.id) != Some(&next) {
            self.save_task_binding(next.clone())?;
        }
        let stale = self
            .personal
            .iter()
            .filter(|(actor, _)| *actor != &next.actor)
            .filter_map(|(_, p)| {
                let mut p = p.clone();
                let mut changed = false;
                for m in p.monitors.values_mut().filter(|m| {
                    m.task_subscription.as_ref() == Some(&next.id) && m.state != "cancelled"
                }) {
                    m.state = "cancelled".into();
                    m.closing_fence.get_or_insert(self.position);
                    changed = true;
                }
                changed.then_some(p)
            })
            .collect::<Vec<_>>();
        for p in stale {
            self.save_personal(p)?;
        }
        let mut personal = self.personal_state(&next.actor);
        if !personal.monitors.contains_key(&monitor_id) {
            let mut monitor = Monitor {
                id: monitor_id.clone(),
                task_subscription: Some(next.id.clone()),
                scope: Scope {
                    conversation: Some(next.channel_id.clone()),
                    ..Scope::default()
                },
                mode: "continuous".into(),
                notify: "wake".into(),
                include_self: false,
                created_at: now(),
                expires_at: None,
                state: "active".into(),
                start: 0,
                scanned: 0,
                hit_high: 0,
                hits: 0,
                acknowledged: 0,
                closing_fence: None,
            };
            self.scan_monitor(&next.actor, &mut monitor, Utc::now())?;
            personal.monitors.insert(monitor_id.clone(), monitor);
            self.save_personal(personal)?;
        }
        Ok(
            json!({"subscription_id":next.id,"task_id":next.task_id,"channel_id":next.channel_id,"binding_revision":next.revision,"monitor_id":monitor_id,"actor_id":next.actor}),
        )
    }
    /// Is `actor` the session currently bound as some continuous task's
    /// parent? Gates the reserved `-orchestrator` name suffix.
    pub fn is_task_subscription_actor(&self, actor: &str) -> bool {
        self.task_bindings
            .values()
            .any(|b| b.active && b.actor == actor)
    }
    /// The channel a task subscription points at, for orientation blocks.
    pub fn task_binding_for(&self, task_id: &str) -> Option<&TaskBinding> {
        self.task_bindings
            .values()
            .find(|b| b.active && b.task_id == task_id)
    }
    pub fn configured_task_ids(&self) -> Vec<String> {
        self.task_bindings
            .values()
            .map(|b| b.task_id.clone())
            .collect()
    }
    pub fn fence_task_subscriptions(&mut self, active: &BTreeSet<String>) -> Result<()> {
        let old = self
            .task_bindings
            .values()
            .filter(|b| b.active && !active.contains(&b.id))
            .cloned()
            .collect::<Vec<_>>();
        for mut binding in old {
            binding.active = false;
            self.save_task_binding(binding)?;
        }
        Ok(())
    }
    pub fn task_orientation(&self, actor: &str) -> Vec<Value> {
        self.task_bindings.values().filter(|b|b.active && b.actor==actor).map(|b|json!({"subscription_id":b.id,"task_id":b.task_id,"channel_id":b.channel_id,"binding_revision":b.revision,"monitor_id":format!("task-{}-{}",b.id,b.revision),"acknowledged_messages":b.acknowledged.len(),"personal_dm_inheritance":false})).collect()
    }
    pub(super) fn task_monitor_matches(&self, actor: &str, m: &Monitor, id: &str) -> bool {
        m.task_subscription.as_ref().is_none_or(|task| {
            self.task_bindings.get(task).is_some_and(|b| {
                b.active
                    && b.actor == actor
                    && m.id == format!("task-{}-{}", b.id, b.revision)
                    && !b.acknowledged.contains(id)
            })
        })
    }
    pub(super) fn acknowledge_task_monitor(&mut self, actor: &str, m: &Monitor) -> Result<()> {
        let Some(id) = &m.task_subscription else {
            return Ok(());
        };
        let mut binding = self
            .task_bindings
            .get(id)
            .filter(|b| {
                b.active && b.actor == actor && m.id == format!("task-{}-{}", b.id, b.revision)
            })
            .cloned()
            .ok_or_else(|| {
                err(
                    "stale_binding",
                    "This session no longer owns the task subscription",
                )
            })?;
        binding.acknowledged.extend(
            self.events
                .iter()
                .filter(|e| {
                    e.position <= m.acknowledged
                        && e.event["type"] == "message.create"
                        && self.scope_matches(actor, &m.scope, &e.event)
                })
                .map(|e| strv(&e.event, "id").to_owned()),
        );
        self.save_task_binding(binding)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn person(uid: &str, id: String) -> Person {
        Person {
            id,
            name: uid.into(),
            session_uid: uid.into(),
            task: None,
            present: true,
            kind: "agent".into(),
        }
    }

    /// The `<task>-orchestrator` name follows the scheduler binding: the dead
    /// predecessor's record is released (no `-1d8` suffix for the replacement),
    /// mentions by name resolve to the live holder, and a worker that is not
    /// the bound parent cannot claim any `-orchestrator` name.
    #[test]
    fn orchestrator_name_hands_over_without_suffix_and_suffix_is_reserved() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = Store::open(tmp.path()).unwrap();
        let old = s.participant_id("old");
        let new = s.participant_id("new");
        let worker = s.participant_id("worker");
        let people = [
            person("old", old.clone()),
            person("new", new.clone()),
            person("worker", worker.clone()),
        ];
        s.enroll_participants(&people).unwrap();
        let channel = s.channels["general"].clone();
        let sub = uuid();
        let mut bind = TaskBinding {
            id: sub.clone(),
            task_id: "bug-triage".into(),
            channel_id: channel.clone(),
            actor: old.clone(),
            revision: 1,
            active: true,
            acknowledged: BTreeSet::new(),
        };
        s.bind_task_subscription(bind.clone()).unwrap();
        let first = s
            .assign_task_name(&old, "old", "bug-triage-orchestrator", &[], "k1")
            .unwrap();
        assert!(first.is_some());
        assert_eq!(s.names[&old].name, "bug-triage-orchestrator");
        // Same actor, same name: no-op.
        assert!(s
            .assign_task_name(&old, "old", "bug-triage-orchestrator", &[], "k1b")
            .unwrap()
            .is_none());
        // A worker cannot take an -orchestrator name at all.
        let e = s
            .send(
                &worker,
                "worker",
                "agent",
                &json!({"channel":"general","name":"bug-triage-orchestrator","body":"hi","request_id":"w1"}),
                &people,
            )
            .unwrap_err();
        assert_eq!(e.code, "reserved_name");
        let e = s
            .send(
                &worker,
                "worker",
                "agent",
                &json!({"channel":"general","name":"perf-triage-orchestrator","body":"hi","request_id":"w2"}),
                &people,
            )
            .unwrap_err();
        assert_eq!(e.code, "reserved_name");
        s.send(
            &worker,
            "worker",
            "agent",
            &json!({"channel":"general","name":"Scout","body":"hi","request_id":"w3"}),
            &people,
        )
        .unwrap();
        // Replacement session takes over the binding and the name, unsuffixed.
        bind.actor = new.clone();
        bind.revision = 2;
        s.bind_task_subscription(bind).unwrap();
        let release = vec![old.clone(), worker.clone()];
        s.assign_task_name(&new, "new", "bug-triage-orchestrator", &release, "k2")
            .unwrap()
            .unwrap();
        assert_eq!(s.names[&new].name, "bug-triage-orchestrator");
        assert!(s.names[&old].released);
        assert_eq!(s.names[&old].name, "bug-triage-orchestrator", "history keeps the old record");
        assert!(!s.names[&worker].released, "unrelated names are untouched");
        // A DM addressed by name reaches the live holder, not the released one.
        let dm = s
            .send(
                &worker,
                "worker",
                "agent",
                &json!({"dm":"bug-triage-orchestrator","body":"handoff","request_id":"dm1"}),
                &people,
            )
            .unwrap();
        let conv = dm["event"]["conversation_id"].as_str().unwrap();
        assert!(s.visible(&new, conv));
        assert!(!s.visible(&old, conv));
        // Survives reopen (the release travelled as an identity event).
        drop(s);
        let s = Store::open(tmp.path()).unwrap();
        assert!(s.names[&old].released);
        assert_eq!(s.names[&new].name, "bug-triage-orchestrator");
    }

    #[test]
    fn system_notice_posts_without_membership_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = Store::open(tmp.path()).unwrap();
        let channel = s.channels["general"].clone();
        let a = s.post_system_notice(&channel, "Run seq 7 held", "held:7").unwrap();
        let b = s.post_system_notice(&channel, "Run seq 7 held", "held:7").unwrap();
        assert_eq!(a["event_id"], b["event_id"]);
        assert_eq!(a["event"]["actor"]["kind"], "system");
        assert_eq!(a["event"]["conversation_id"], channel);
        assert!(s.post_system_notice("missing", "x", "k").is_err());
    }

    #[test]
    fn messaging_task_replacement_keeps_checkpoint_but_not_private_watches_or_dms() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = Store::open(tmp.path()).unwrap();
        let old = s.participant_id("old");
        let new = s.participant_id("new");
        let peer = s.participant_id("peer");
        let people = [
            ("old", old.clone()),
            ("new", new.clone()),
            ("peer", peer.clone()),
        ]
        .map(|(uid, id)| Person {
            id,
            name: uid.into(),
            session_uid: uid.into(),
            task: None,
            present: true,
            kind: "agent".into(),
        });
        s.enroll_participants(&people).unwrap();
        s.send(
            &peer,
            "peer",
            "agent",
            &json!({"channel":"general","name":"Peer","body":"First","request_id":"first"}),
            &people,
        )
        .unwrap();
        let channel = s.channels["general"].clone();
        let id = uuid();
        let bind = TaskBinding {
            id: id.clone(),
            task_id: "triage".into(),
            channel_id: channel,
            actor: old.clone(),
            revision: 1,
            active: true,
            acknowledged: BTreeSet::new(),
        };
        let initial = s.bind_task_subscription(bind.clone()).unwrap();
        let result = s
            .monitors(
                &old,
                &json!({"action":"get","monitor_id":initial["monitor_id"]}),
            )
            .unwrap();
        s.monitors(&old,&json!({"action":"ack","monitor_id":initial["monitor_id"],"receipt":result["receipt"],"request_id":"ack-first"})).unwrap();
        let private = s
            .register_monitor(
                &old,
                &json!({"scope":{"dms":true},"request_id":"personal"}),
                &people,
            )
            .unwrap();
        let dm = s
            .send(
                &peer,
                "peer",
                "agent",
                &json!({"dm":old,"body":"Personal DM","request_id":"dm"}),
                &people,
            )
            .unwrap();
        let second = s
            .send(
                &peer,
                "peer",
                "agent",
                &json!({"channel":"general","body":"Second","request_id":"second"}),
                &people,
            )
            .unwrap();
        let mut next = bind.clone();
        next.actor = new.clone();
        next.revision = 2;
        let replacement = s.bind_task_subscription(next.clone()).unwrap();
        let result = s
            .monitors(
                &new,
                &json!({"action":"get","monitor_id":replacement["monitor_id"]}),
            )
            .unwrap();
        assert_eq!(result["items"].as_array().unwrap().len(), 1, "{result}");
        assert!(result["items"][0]
            .to_string()
            .contains(second["event_id"].as_str().unwrap()));
        assert!(!s.visible(&new, dm["event"]["conversation_id"].as_str().unwrap()));
        assert!(!s
            .personal_state(&new)
            .monitors
            .contains_key(private["id"].as_str().unwrap()));
        assert_eq!(
            s.bind_task_subscription(bind).unwrap_err().code,
            "stale_binding"
        );
        assert_eq!(s.monitors(&old,&json!({"action":"ack","monitor_id":initial["monitor_id"],"receipt":result["receipt"],"request_id":"stale-ack"})).unwrap_err().code,"stale_binding");
        s.monitors(&new,&json!({"action":"ack","monitor_id":replacement["monitor_id"],"receipt":result["receipt"],"request_id":"ack-second"})).unwrap();
        drop(s);
        let mut s = Store::open(tmp.path()).unwrap();
        next.revision = 3;
        next.actor = old.clone();
        let replacement = s.bind_task_subscription(next).unwrap();
        assert!(s
            .monitors(
                &old,
                &json!({"action":"get","monitor_id":replacement["monitor_id"]})
            )
            .unwrap()["items"]
            .as_array()
            .unwrap()
            .is_empty());
    }
}
