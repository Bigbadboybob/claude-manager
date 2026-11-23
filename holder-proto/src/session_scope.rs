//! Pure session-control scope over one complete org snapshot.
//!
//! Both the daemon and the viewer can use this without consulting the task
//! hierarchy. Entries only become authoritative when the entire snapshot is
//! replaced, so a failed refresh cannot grant a newly observed relationship.
use std::collections::{HashMap, HashSet};
use serde_json::Value;

/// The snapshot protocol identifies a session by host daemon and UID.
pub fn session_key(daemon_id: &str, uid: &str) -> String {
    format!("agent:{daemon_id}:{uid}")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub org_id: String,
    pub manager: Option<String>,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub member: String,
    pub role: Role,
    pub spawned_by: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role { Main, Assistant, Worker }

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub members: HashMap<String, Member>,
    pub sessions: HashMap<String, Session>,
}

impl Snapshot {
    /// Parse the complete org.snapshot, including the session facts. Reject
    /// partial/old replies: installing one would silently discard grants.
    pub fn from_org_snapshot(value: &Value) -> Option<Self> {
        let mut out = Self::default();
        for member in value.get("members")?.as_array()? {
            let id = member.get("id")?.as_str()?.to_owned();
            let org_id = member.get("org_id")?.as_str()?.to_owned();
            let manager = member.get("manager_member_id")?.as_str().map(str::to_owned);
            let active = member.get("status")?.as_str()? == "active";
            if out.members.insert(id.clone(), Member { org_id, manager, active }).is_some() { return None; }
            for fact in member.get("assistants")?.as_array()? {
                let daemon = fact.get("daemon_id")?.as_str()?;
                let uid = fact.get("session_uid")?.as_str()?;
                if daemon.is_empty() || uid.is_empty() || daemon.contains(':') || uid.contains(':') { return None; }
                if fact.get("pid").and_then(Value::as_str).is_some_and(|pid| pid != session_key(daemon, uid)) { return None; }
                let role = match fact.get("acting_as")?.as_str()? {
                    "main" => Role::Main,
                    "assistant" => Role::Assistant,
                    "worker" => Role::Worker,
                    _ => return None,
                };
                let spawned_by = match fact.get("spawned_by")? {
                    Value::Null => None,
                    parent => {
                        let d = parent.get("daemon_id")?.as_str()?;
                        let u = parent.get("session_uid")?.as_str()?;
                        if d != daemon { return None; }
                        Some(session_key(d, u))
                    }
                };
                if out.sessions.insert(session_key(daemon, uid), Session { member: id.clone(), role, spawned_by }).is_some() { return None; }
            }
        }
        Some(out)
    }
    /// This is the same downward manager closure as the roster's
    /// `member_scope`: each edge must stay within the caller's organization.
    pub fn member_in_scope(&self, ancestor: &str, target: &str) -> bool {
        let Some(root) = self.members.get(ancestor) else { return false };
        if !root.active { return false; }
        let mut current = target;
        let mut seen = HashSet::new();
        while seen.insert(current) {
            let Some(member) = self.members.get(current) else { return false };
            if member.org_id != root.org_id { return false; }
            if current == ancestor { return true; }
            let Some(parent) = member.manager.as_deref() else { return false };
            current = parent;
        }
        false
    }

    /// Apply cached member/role authority to the current local spawner links.
    /// A session absent from the snapshot inherits its placement only from the
    /// first cached session in its local spawner chain; no local org stamp can
    /// introduce a new membership or manager grant while the API is down.
    pub fn reaches_with_spawners(
        &self, caller: &str, target: &str,
        parent_of: impl Fn(&str) -> Option<String>,
    ) -> bool {
        // The local registry is authoritative even when either endpoint has
        // not appeared in the last successful org snapshot.
        let mut current = target.to_owned();
        let mut seen = HashSet::new();
        while seen.insert(current.clone()) {
            let Some(parent) = parent_of(&current) else { break };
            if parent == caller { return true; }
            current = parent;
        }
        let Some(source) = self.sessions.get(caller) else { return false };
        if source.role != Role::Main { return false; }
        let mut current = target.to_owned();
        let mut seen = HashSet::new();
        while seen.insert(current.clone()) {
            if let Some(session) = self.sessions.get(&current) {
                return self.member_in_scope(&source.member, &session.member);
            }
            let Some(parent) = parent_of(&current) else { return false };
            current = parent;
        }
        false
    }

    /// Evaluate the same rule using only the snapshot's recorded spawners.
    pub fn reaches(&self, caller: &str, target: &str) -> bool {
        self.reaches_with_spawners(caller, target, |key| {
            self.sessions.get(key).and_then(|session| session.spawned_by.clone())
        })
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample() -> Snapshot {
        let mut s = Snapshot::default();
        for (id, parent) in [("lead", None), ("report", Some("lead"))] {
            s.members.insert(id.into(), Member { org_id: "o".into(), manager: parent.map(str::to_owned), active: true });
        }
        for (uid, member, role, parent) in [
            ("lead-main", "lead", Role::Main, None),
            ("a", "lead", Role::Assistant, Some("lead-main")),
            ("b", "lead", Role::Assistant, Some("lead-main")),
            ("report-main", "report", Role::Main, None),
            ("worker", "report", Role::Worker, Some("report-main")),
            ("child", "lead", Role::Worker, Some("a")),
        ] {
            s.sessions.insert(uid.into(), Session { member: member.into(), role, spawned_by: parent.map(str::to_owned) });
        }
        s
    }
    #[test]
    fn lead_controls_reports_worker_assistant_cannot_control_sibling() {
        let s = sample();
        assert!(s.reaches("lead-main", "worker"));
        assert!(!s.reaches("a", "b"));
        assert!(s.reaches("a", "child"));
    }
    #[test]
    fn shared_roster_authority_fixture() {
        let value: Value = serde_json::from_str(include_str!("../tests/fixtures/session-scope-parity.json")).unwrap();
        let scope = Snapshot::from_org_snapshot(&value).unwrap();
        for case in value["cases"].as_array().unwrap() {
            assert_eq!(scope.reaches(case["caller"].as_str().unwrap(), case["target"].as_str().unwrap()),
                case["allowed"].as_bool().unwrap(), "{case}");
        }
    }

    #[test]
    fn session_keys_include_daemon_and_unknown_facts_fail_closed() {
        let mut value: Value = serde_json::from_str(include_str!("../tests/fixtures/session-scope-parity.json")).unwrap();
        value["members"][2]["assistants"].as_array_mut().unwrap().push(serde_json::json!({
            "daemon_id": "d2", "session_uid": "helper", "acting_as": "assistant", "spawned_by": null
        }));
        let scope = Snapshot::from_org_snapshot(&value).unwrap();
        assert!(scope.reaches("agent:d1:lead", "agent:d1:helper"));
        assert!(!scope.reaches("agent:d1:lead", "agent:d2:helper"));
        assert!(!scope.reaches("agent:d1:lead", "agent:d1:unknown"));
        value["members"][0].as_object_mut().unwrap().remove("assistants");
        assert!(Snapshot::from_org_snapshot(&value).is_none());
    }

    #[test]
    fn no_cross_org_or_cycle_widening() {
        let mut s = sample();
        s.members.get_mut("report").unwrap().org_id = "other".into();
        assert!(!s.reaches("lead-main", "worker"));
        s.sessions.get_mut("b").unwrap().spawned_by = Some("b".into());
        assert!(!s.reaches("a", "b"));
    }

    #[test]
    fn live_spawner_chain_precedes_all_cached_membership() {
        let mut s = sample();
        s.members.get_mut("report").unwrap().org_id = "other".into();
        let parents = HashMap::from([
            ("new-child", "new-assistant"),
            ("worker", "lead-main"),
        ]);
        let parent = |key: &str| parents.get(key).map(|s| (*s).to_owned());
        assert!(s.reaches_with_spawners("new-assistant", "new-child", parent));
        assert!(s.reaches_with_spawners("lead-main", "worker", parent));
        assert!(!s.reaches_with_spawners("new-assistant", "worker", parent));
    }
}
