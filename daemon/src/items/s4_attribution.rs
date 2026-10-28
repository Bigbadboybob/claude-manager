//! Non-authoritative, nonblocking S4-min display evidence.
use crate::state::DaemonState;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::os::fd::{AsRawFd, OwnedFd};

#[derive(Clone)]
pub(crate) struct Subject {
    pid: libc::pid_t, incarnation: Option<u64>, generation: u64,
    exit: crate::session::SharedLastExit,
    workflow: bool, continuous: bool, local_origin: bool,
}
impl Subject {
    pub(crate) fn from_session(s: &crate::session::DaemonSession) -> Self {
        Self { pid: s.pid, incarnation: s.holder_incarnation, generation: s.generation, exit: s.last_exit.clone(),
            workflow: s.workflow_run_id.is_some(), continuous: s.continuous_task_id.is_some(),
            local_origin: s.s4_spawn_origin.is_some() }
    }
    fn same(&self, other: &Self) -> bool {
        self.pid == other.pid && self.incarnation == other.incarnation && self.generation == other.generation
            && Arc::ptr_eq(&self.exit, &other.exit) && !self.workflow && !self.continuous && self.local_origin
            && !other.workflow && !other.continuous && other.local_origin
    }
}

#[derive(Clone)]
pub(crate) struct Evidence { pub(crate) claim: Value, subject: Subject, pidfd: Arc<OwnedFd> }
impl Evidence {
    fn matches(&self, current: &Subject) -> bool {
        if !self.subject.same(current) || current.exit.try_kernel_set() != Some(false) { return false; }
        let mut descriptor = libc::pollfd { fd: self.pidfd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // pidfd is bound by the kernel to the original process; timeout zero
        // neither waits nor depends on a reusable numerical PID.
        unsafe { libc::poll(&mut descriptor, 1, 0) == 0 }
    }
}

#[derive(Default, Clone)]
struct Central { version: i64, daemon: String, mains: HashMap<String, String>, ambiguous: std::collections::HashSet<String>, wire: bool }
fn cache() -> &'static Mutex<Option<Arc<Central>>> {
    static CACHE: OnceLock<Mutex<Option<Arc<Central>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}
static LATEST: AtomicI64 = AtomicI64::new(-1);
static INVALID: AtomicBool = AtomicBool::new(false);
static WIRE: AtomicBool = AtomicBool::new(false);
thread_local! { static CURRENT: std::cell::RefCell<(HashMap<String, Evidence>, bool)> = Default::default(); }
pub(crate) struct Scope((HashMap<String, Evidence>, bool));
impl Drop for Scope { fn drop(&mut self) { CURRENT.with(|c| *c.borrow_mut() = std::mem::take(&mut self.0)); } }
pub(crate) fn begin(state: &Arc<Mutex<DaemonState>>, req: &crate::control::protocol::Request) -> Scope {
    let enabled = matches!(req.method.as_str(), "mcp_start_session" | "item.set" | "item.resolve" | "messaging.send");
    let switched_on = std::env::var_os("CM_S4_MIN_WRITE").is_some_and(|v| v == "1");
    let mut refs = Vec::new();
    if let Some(uid) = req.caller.session_uid() { refs.push(uid.to_lowercase()); }
    if matches!(req.method.as_str(), "item.set" | "item.resolve") {
        for key in ["holder", "add_holder", "owner"] {
            match &req.params[key] {
                Value::String(s) => refs.push(s.trim().trim_start_matches('@').to_lowercase()),
                Value::Array(values) if values.len() <= 50 => refs.extend(values.iter().filter_map(Value::as_str)
                    .map(|s| s.trim().trim_start_matches('@').to_lowercase())),
                _ => {}
            }
        }
    }
    let fresh = if enabled && switched_on { capture_for(state, Some(&refs)) } else { Default::default() };
    Scope(CURRENT.with(|c| std::mem::replace(&mut *c.borrow_mut(), fresh)))
}
pub(crate) fn current() -> (HashMap<String, Evidence>, bool) { CURRENT.with(|c| c.borrow().clone()) }
pub(crate) fn matching(evidence: HashMap<String, Evidence>, subjects: &HashMap<String, Subject>) -> HashMap<String, Value> {
    evidence.into_iter().filter_map(|(uid, ev)| {
        subjects.get(&uid).filter(|subject| ev.matches(subject)).map(|_| (uid, ev.claim))
    }).collect()
}

#[cfg(test)]
pub(crate) fn reset_for_test() {
    *cache().lock().unwrap() = None;
    LATEST.store(-1, Ordering::Relaxed);
    INVALID.store(false, Ordering::SeqCst);
    WIRE.store(false, Ordering::SeqCst);
}

pub(crate) fn observe_version(v: &Value) {
    if let Some(version) = v["version"].as_i64().filter(|v| *v >= 0) {
        LATEST.fetch_max(version, Ordering::Relaxed);
    }
}

/// Called by the existing background heartbeat, never by a write path.
pub(crate) fn absorb_snapshot(daemon: &str, value: &Value) {
    INVALID.store(true, Ordering::SeqCst);
    let Some(version) = value["version"].as_i64().filter(|v| *v >= 0) else { return };
    let Some(members) = value["members"].as_array() else { return };
    let mut mains = HashMap::new();
    let mut ambiguous = std::collections::HashSet::new();
    for member in members {
        let main = &member["main"];
        if main.is_null() { continue; }
        let (Some(uid), Some(d)) = (main["session_uid"].as_str(), main["daemon_id"].as_str()) else {
            observe_version(value);
            return;
        };
        if d != daemon { continue; }
        let valid = member["id"].as_str().filter(|id| !id.is_empty())
            .filter(|_| main["pid"].as_str() == Some(format!("agent:{d}:{uid}").as_str()))
            .filter(|_| main["occupancy_generation"].as_i64().is_some_and(|g| g > 0));
        if ambiguous.contains(uid) { continue; }
        if let Some(id) = valid {
            if mains.insert(uid.to_owned(), id.to_owned()).is_none() { continue; }
        }
        mains.remove(uid);
        ambiguous.insert(uid.to_owned());
    }
    observe_version(value);
    if let Ok(mut guard) = cache().try_lock() {
        if guard.as_ref().is_none_or(|old| version >= old.version) {
            *guard = Some(Arc::new(Central { version, daemon: daemon.to_owned(), mains, ambiguous,
                wire: value["s4_min_wire"] == true }));
            WIRE.store(value["s4_min_wire"] == true, Ordering::SeqCst);
        }
        INVALID.store(false, Ordering::SeqCst);
    }
}

/// Snapshot under a short try-lock, then compute outside all locks. A busy or
/// poisoned source is unknown; this never changes operation admission.
#[cfg(test)]
pub(crate) fn capture(state: &Arc<Mutex<DaemonState>>) -> (HashMap<String, Evidence>, bool) {
    capture_for(state, None)
}
fn capture_for(state: &Arc<Mutex<DaemonState>>, refs: Option<&[String]>) -> (HashMap<String, Evidence>, bool) {
    if INVALID.load(Ordering::SeqCst) { return (HashMap::new(), WIRE.load(Ordering::SeqCst)); }
    let locals = match state.try_lock() {
        Ok(s) => s.sessions.iter().filter(|(uid, session)| refs.is_none_or(|refs| refs.iter().any(|r| {
            uid.to_lowercase() == *r || r.rsplit(':').next() == Some(uid.as_str())
                || session.title.to_lowercase() == *r || s.messaging_names.get(*uid).is_some_and(|name|
                    name.name.to_lowercase() == *r || name.aliases.iter().any(|a| a.to_lowercase() == *r))
        }))).filter_map(|(uid, session)| Some((
            uid.clone(), session.s4_spawn_origin.clone()?, session.pid,
            session.holder_incarnation, session.generation, session.workflow_run_id.is_some(),
            session.continuous_task_id.is_some(), session.org.clone(), Subject::from_session(session),
            Arc::new(session.s4_pidfd()?),
        ))).collect::<Vec<_>>(),
        Err(_) => return (HashMap::new(), WIRE.load(Ordering::SeqCst)),
    };
    let central = match cache().try_lock() {
        Ok(c) => c.clone(),
        Err(_) => return (HashMap::new(), WIRE.load(Ordering::SeqCst)),
    };
    let wire = central.as_ref().is_some_and(|c| c.wire);
    if INVALID.load(Ordering::SeqCst) { return (HashMap::new(), wire); }
    let mut claims = HashMap::new();
    for (uid, origin, pid, incarnation, generation, workflow, continuous, org, subject, pidfd) in locals {
        if origin.0 != pid || origin.1 != incarnation || origin.2 != generation || workflow || continuous { continue; }
        if subject.exit.try_kernel_set() != Some(false) { continue; }
        if central.as_ref().is_some_and(|c| c.ambiguous.contains(&uid)) { continue; }
        let legacy = crate::assistants::stamped(origin.3.as_ref()).map(|(m, r)| (m, r.to_string()));
        let current_stamp = crate::assistants::stamped(org.as_ref()).map(|(m, r)| (m, r.to_string()));
        if let Some((c, member)) = central.as_ref().filter(|c| c.version >= LATEST.load(Ordering::Relaxed))
            .and_then(|c| c.mains.get(&uid).map(|m| (c, m))) {
            let claim = json!({"member_id": member, "acting_as": "main", "verified": true,
                "provenance": "central-open-main", "org_version": c.version, "daemon_id": c.daemon.as_str()});
            let ev = Evidence { claim, subject, pidfd };
            if ev.matches(&ev.subject) { claims.insert(uid, ev); }
        } else if let Some((member, role)) = legacy.filter(|old| current_stamp.as_ref() == Some(old)) {
            let claim = json!({"member_id": member, "acting_as": role,
                "provenance": "o5-spawn-stamp", "as_of": "spawn", "verified": false});
            let ev = Evidence { claim, subject, pidfd };
            if ev.matches(&ev.subject) { claims.insert(uid, ev); }
        }
    }
    (claims, wire)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{DaemonSession, SpawnParams};

    #[test]
    fn local_main_snapshot_legacy_stale_and_replaced_are_distinct() {
        let _env = crate::test_support::env_lock();
        reset_for_test();
        let mut params = SpawnParams::new("ts-a1-1", "s4", "/bin/sleep");
        params.args = vec!["60".into()];
        let mut session = DaemonSession::spawn(params).unwrap();
        crate::assistants::stamp(&mut session.org, "mem_old", "assistant", "spawn:caller");
        session.s4_spawn_origin = Some((session.pid, session.holder_incarnation, session.generation,
            session.org.clone()));
        let mut st = DaemonState::default();
        st.sessions.insert("ts-a1-1".into(), session);
        let state = Arc::new(Mutex::new(st));
        let snapshot = json!({"version": 10, "s4_min_wire": true, "members": [
            {"id":"mem_main", "main":{"pid":"agent:d:ts-a1-1", "daemon_id":"d",
             "session_uid":"ts-a1-1", "occupancy_generation":1}}]});
        absorb_snapshot("d", &snapshot);
        let (claims, wire) = capture(&state);
        assert!(wire);
        assert_eq!(claims["ts-a1-1"].claim["member_id"], "mem_main");
        assert_eq!(claims["ts-a1-1"].claim["provenance"], "central-open-main");
        let original = {
            let st = state.lock().unwrap();
            Subject::from_session(&st.sessions["ts-a1-1"])
        };
        assert!(matching(claims.clone(), &HashMap::from([("ts-a1-1".into(), original.clone())])).contains_key("ts-a1-1"));
        let mut params2 = SpawnParams::new("ts-a1-2", "replacement", "/bin/sleep");
        params2.args = vec!["60".into()];
        let replacement = DaemonSession::spawn(params2).unwrap();
        let mut reused = Subject::from_session(&replacement);
        // Even with the same numeric fields (a recycled PID), the distinct
        // process instance cannot consume the earlier claim.
        reused.pid = original.pid;
        reused.incarnation = original.incarnation;
        reused.generation = original.generation;
        reused.local_origin = true;
        assert!(matching(claims.clone(), &HashMap::from([("ts-a1-1".into(), reused.clone())])).is_empty());
        let pair = HashMap::from([("actor".into(), claims["ts-a1-1"].clone()),
                                  ("holder".into(), claims["ts-a1-1"].clone())]);
        let subjects = HashMap::from([("actor".into(), original.clone()), ("holder".into(), reused)]);
        let matched = matching(pair, &subjects);
        assert!(matched.contains_key("actor") && !matched.contains_key("holder"));
        let mut retagged = original;
        retagged.workflow = true;
        assert!(matching(claims, &HashMap::from([("ts-a1-1".into(), retagged)])).is_empty());
        drop(replacement);
        observe_version(&json!({"version":11}));
        assert_eq!(capture(&state).0["ts-a1-1"].claim["provenance"], "o5-spawn-stamp");
        {
            let mut st = state.lock().unwrap();
            let session = st.sessions.get_mut("ts-a1-1").unwrap();
            crate::assistants::stamp(&mut session.org, "mem_main", "main", "promote:test");
        }
        assert!(capture(&state).0.is_empty(), "stale promoted main is unknown");
        absorb_snapshot("d", &json!({"version": 12, "s4_min_wire":true, "members":[]}));
        assert!(capture(&state).0.is_empty(), "missing main cannot borrow old stamp");
        absorb_snapshot("d", &json!({"version":13, "members":[{"id":"mem_x", "main":{}}]}));
        assert!(capture(&state).0.is_empty(), "malformed snapshot cannot retain a stale claim");
        state.lock().unwrap().sessions.remove("ts-a1-1");
        reset_for_test();
    }

    #[test]
    fn contention_is_unknown_and_retirement_writer_is_not_acquired() {
        let _env = crate::test_support::env_lock();
        let state = Arc::new(Mutex::new(DaemonState::default()));
        let guard = state.lock().unwrap();
        assert!(capture(&state).0.is_empty());
        drop(guard);
        let cache_guard = cache().lock().unwrap();
        assert!(capture(&state).0.is_empty(), "busy provenance cache is unknown");
        drop(cache_guard);
        let gate = crate::continuous::retirement::gate("ts-a2-1");
        let writer = gate.write().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let s = state.clone();
        let worker = std::thread::spawn(move || tx.send(capture(&s)).unwrap());
        assert!(rx.recv_timeout(std::time::Duration::from_secs(2)).is_ok());
        drop(writer);
        worker.join().unwrap();
    }
}
