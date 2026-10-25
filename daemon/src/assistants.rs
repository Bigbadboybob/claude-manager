//! Durable assistants (doc/org-model.md §4.2, §4.3, Owner decision O5).
//!
//! An agent spawned by a member's main session, or by one of its assistants,
//! works for that member. For an ordinary member it is an **assistant**: as
//! durable as the main session. It keeps its session, context and checkout
//! until it is deliberately closed (`org.close_assistant`, Owner's A-w) by its
//! member's main session, the member's manager chain or Owner, or killed by an
//! agent's `kill_session`. Task/item completion, `mark_subtask_done` and the
//! terminal sweep never close it, and its own checkout is a `stream`
//! workspace of the member's office rather than `ephemeral`.
//!
//! A continuous member's spawns are **workers** (Swarm-Coord, 2026-10-25):
//! stamped for attribution only. They are torn down exactly as today and
//! their checkouts are never promoted, because triage close-out reclaims them
//! through `mark_subtask_done` and continuous hosts run near their worktree
//! cap.
//!
//! Only spawned sessions are stamped. A session with no spawner
//! (`managed_by_uid` null) was started by Owner or an operator and is nobody's
//! assistant, and shells (`bash`) are never stamped: only Owner closes,
//! kills or restarts a shell.
//!
//! The stamp lives in the session's org object (doc/org-model.md §12):
//! `member_id`, `acting_as` (`assistant` / `worker`) and `member_via` (how it
//! was learned: `spawn:<uid>`, `spawner:<uid>`, `fork:<uid>`, `task:<id>`).
//! Mains are never stamped here: whether a session is a member's main comes
//! only from the API's member cache ([`crate::items::org::cached_member`]),
//! and it wins over any stamp.
//!
//! A stream checkout is not protected forever by a dead assistant: once no
//! assistant has been live in it for [`STREAM_RELEASE_SECS`] it goes back to
//! `ephemeral` ([`release_idle_streams`]), and a revive inside that window
//! keeps it.
//!
//! Advertised as [`CAPABILITY`] (brain-only): the API mirrors a task's close
//! only when every daemon holding its sessions keeps assistants alive, so an
//! older brain never tears down an assistant that this one would keep.

use crate::session::DaemonSession;
use crate::state::DaemonState;
use cm_holder_proto::org::{self as bridge, OrgObject};
use serde_json::{json, Value};
use std::collections::HashMap;

/// Advertised in every board heartbeat (brain-only, [`crate::capabilities::BRAIN_ONLY`]).
pub const CAPABILITY: &str = "assistant_guard_v1";

pub const MEMBER_ID: &str = "member_id";
pub const ACTING_AS: &str = "acting_as";
pub const MEMBER_VIA: &str = "member_via";
pub const ASSISTANT: &str = "assistant";
/// A continuous member's spawn: attributed, not durable.
pub const WORKER: &str = "worker";
pub const MAIN: &str = "main";
/// Set on an assistant's org object when it is closed (`close_assistant`, or
/// an agent's `kill_session`): its tombstone then no longer protects its
/// checkout (the reaper reads it).
pub const CLOSED_AT: &str = "assistant_closed_at";
/// Workspace org keys: who claimed the stream, and since when (unix seconds)
/// it has had no live assistant.
pub const STREAM_OF: &str = "stream_of";
pub const IDLE_SINCE: &str = "assistant_idle_since";
/// A stream checkout whose assistant has had no live session this long goes
/// back to normal reaper eligibility (7 days).
pub const STREAM_RELEASE_SECS: f64 = 7.0 * 86400.0;

/// Whom a session works for, and how.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Membership {
    pub member_id: String,
    /// [`MAIN`], [`ASSISTANT`] or [`WORKER`].
    pub acting_as: &'static str,
}

/// The member `uid` is the main session of, by the member cache.
pub fn main_member(uid: &str) -> Option<String> {
    let m = crate::items::org::cached_member(uid);
    (m["acting_as"] == MAIN).then(|| m["id"].as_str().map(str::to_string)).flatten()
}

/// Whether `member_id` is a continuous member, by the member cache (which
/// knows the members whose main session is on this daemon).
pub fn is_continuous_member(member_id: &str) -> bool {
    crate::items::org::cached_member_kind(member_id).as_deref() == Some("continuous")
}

fn acting(text: &str) -> Option<&'static str> {
    match text {
        ASSISTANT => Some(ASSISTANT),
        WORKER => Some(WORKER),
        _ => None,
    }
}

/// The open stamp in an org object (assistant or worker), when there is one.
pub fn stamped(org: Option<&OrgObject>) -> Option<(String, &'static str)> {
    let org = org?;
    if org.contains_key(CLOSED_AT) {
        return None;
    }
    let how = acting(org.get(ACTING_AS)?.as_str()?)?;
    let member = org.get(MEMBER_ID)?.as_str().filter(|m| !m.is_empty())?;
    Some((member.to_string(), how))
}

/// The member of a durable assistant stamp (not a worker's).
pub fn stamped_member(org: Option<&OrgObject>) -> Option<String> {
    stamped(org).filter(|(_, how)| *how == ASSISTANT).map(|(m, _)| m)
}

/// Whom `uid` (with org object `org`) works for: a main by the cache wins
/// over any stamp.
pub fn membership(uid: &str, org: Option<&OrgObject>) -> Option<Membership> {
    if let Some(member_id) = main_member(uid) {
        return Some(Membership { member_id, acting_as: MAIN });
    }
    stamped(org).map(|(member_id, acting_as)| Membership { member_id, acting_as })
}

pub fn of_session(sess: &DaemonSession) -> Option<Membership> {
    membership(&sess.uid, sess.org.as_ref())
}

/// A member's main session or one of its (durable) assistants: never closed
/// by finishing work. Workers are not.
pub fn is_member_session(sess: &DaemonSession) -> bool {
    of_session(sess).is_some_and(|m| m.acting_as != WORKER)
}

pub fn is_assistant(sess: &DaemonSession) -> bool {
    of_session(sess).is_some_and(|m| m.acting_as == ASSISTANT)
}

fn is_shell(session_type: &str) -> bool {
    session_type == "bash"
}

/// Stamp `org` as `acting_as` ([`ASSISTANT`] / [`WORKER`]) of `member_id`.
pub fn stamp(org: &mut Option<OrgObject>, member_id: &str, acting_as: &str, via: &str) {
    bridge::set(org, MEMBER_ID, json!(member_id));
    bridge::set(org, ACTING_AS, json!(acting_as));
    bridge::set(org, MEMBER_VIA, json!(via));
}

/// True when a record's org object already names a member (a stamp, or a
/// later stage's `main`): the backfill and the spawn stamp leave it alone.
fn has_member_key(org: Option<&OrgObject>) -> bool {
    org.is_some_and(|o| o.get(MEMBER_ID).is_some_and(|v| !v.is_null()))
}

/// How a spawn by a session with membership `m` works for its member: a
/// continuous member's are workers; otherwise it inherits the spawner's
/// kind (a main's spawns are assistants, a worker's spawns stay workers).
fn child_role(m: &Membership) -> &'static str {
    match m.acting_as {
        MAIN if is_continuous_member(&m.member_id) => WORKER,
        MAIN => ASSISTANT,
        other => other,
    }
}

fn org_of(state: &DaemonState, uid: &str) -> Option<OrgObject> {
    state
        .sessions
        .get(uid)
        .map(|s| s.org.clone())
        .unwrap_or_else(|| crate::control::methods::persisted_org(state, uid))
}

/// Whom a spawn by `spawner_uid` works for, and how: `(member, acting_as)`.
pub fn member_for_spawn(state: &DaemonState, spawner_uid: &str) -> Option<(String, &'static str)> {
    let m = membership(spawner_uid, org_of(state, spawner_uid).as_ref())?;
    let role = child_role(&m);
    Some((m.member_id, role))
}

/// The spawn-time stamp: a child of a member's session works for that
/// member. Leaves an org object that already names a member (a respawn at
/// the same uid, or a fork stamped from its source), the main of a member, a
/// shell and a session with no spawner untouched. Returns the role stamped.
pub fn stamp_spawn(
    state: &DaemonState,
    uid: &str,
    spawner_uid: Option<&str>,
    session_type: &str,
    org: &mut Option<OrgObject>,
) -> Option<&'static str> {
    if has_member_key(org.as_ref()) || main_member(uid).is_some() || is_shell(session_type) {
        return None;
    }
    let spawner = spawner_uid.filter(|s| *s != uid)?;
    let (member_id, role) = member_for_spawn(state, spawner)?;
    stamp(org, &member_id, role, &format!("spawn:{spawner}"));
    Some(role)
}

/// A fork's stamp (design: the fork works for the SOURCE's member, not the
/// caller's): `Some(org)` with the stamp when the source works for a member.
pub fn fork_org(state: &DaemonState, source_uid: &str, session_type: &str) -> Option<OrgObject> {
    if is_shell(session_type) {
        return None;
    }
    let (member_id, role) = member_for_spawn(state, source_uid)?;
    let mut org = None;
    stamp(&mut org, &member_id, role, &format!("fork:{source_uid}"));
    org
}

/// Record an ephemeral workspace as a `stream` of `member_id`'s office when
/// every member session in it works for that member: the checkout of a
/// member's assistant is durable (O5). Home, stream, pinned and unclassified
/// workspaces are left as they are. Returns whether it changed.
pub fn claim_workspace(state: &mut DaemonState, ws_id: &str, member_id: &str) -> bool {
    let others = state.sessions.values().any(|s| {
        s.workspace_id == ws_id && of_session(s).is_some_and(|m| m.member_id != member_id)
    });
    let Some(ws) = state.workspaces.get_mut(ws_id) else { return false };
    if others || bridge::workspace_kind(ws.org.as_ref()) != Some(bridge::KIND_EPHEMERAL) {
        return false;
    }
    bridge::set(&mut ws.org, bridge::WORKSPACE_KIND, json!("stream"));
    bridge::set(&mut ws.org, crate::items::org::OFFICE_OF, json!(member_id));
    bridge::set(&mut ws.org, STREAM_OF, json!(ASSISTANT));
    true
}

fn claimed_stream(ws: &crate::manifest::ManifestWorkspace) -> bool {
    ws.org.as_ref().is_some_and(|o| o.get(STREAM_OF) == Some(&json!(ASSISTANT)))
        && bridge::workspace_kind(ws.org.as_ref()) == Some("stream")
}

fn unclaim(ws: &mut crate::manifest::ManifestWorkspace) {
    bridge::set(&mut ws.org, bridge::WORKSPACE_KIND, json!(bridge::KIND_EPHEMERAL));
    if let Some(o) = ws.org.as_mut() {
        for key in [crate::items::org::OFFICE_OF, STREAM_OF, IDLE_SINCE] {
            o.remove(key);
        }
    }
}

/// After a close: a `stream` checkout this module claimed for assistants
/// goes back to `ephemeral` once no member session remains in it, so the
/// reaper's normal protections (no live process, not dirty, evidence
/// released) decide when it goes. A central mark (`home`/`stream` from the
/// roster) is never downgraded.
pub fn release_workspace(state: &mut DaemonState, ws_id: &str, closing_uid: &str) -> bool {
    let still_used = state.sessions.values().any(|s| {
        s.workspace_id == ws_id && s.uid != closing_uid && is_member_session(s)
    });
    let Some(ws) = state.workspaces.get_mut(ws_id) else { return false };
    if still_used || !claimed_stream(ws) {
        return false;
    }
    unclaim(ws);
    true
}

/// Mark `uid` closed (its tombstone stops protecting its checkout) and
/// release a stream it was the last assistant in. Returns the member and
/// whether the workspace was released; `None` when `uid` is no live
/// assistant. The caller kills the session and hands its holds back.
pub fn end_assistant(state: &mut DaemonState, uid: &str) -> Option<(String, bool)> {
    let (member_id, ws_id) = {
        let sess = state.sessions.get_mut(uid)?;
        let m = of_session(sess).filter(|m| m.acting_as == ASSISTANT)?;
        bridge::set(&mut sess.org, CLOSED_AT, json!(chrono::Utc::now().to_rfc3339()));
        (m.member_id, sess.workspace_id.clone())
    };
    let released = release_workspace(state, &ws_id, uid);
    Some((member_id, released))
}

/// O5 blocker 2: a stream claimed for assistants is not protected forever by
/// an assistant that died some other way (a plain kill, a crash or memory
/// cap, a natural exit). Each pass notes when such a stream first had no
/// live assistant, clears that on a revive, and releases it to `ephemeral`
/// once [`STREAM_RELEASE_SECS`] have passed. Returns the released ids.
pub fn release_idle_streams(state: &mut DaemonState, now: f64) -> (Vec<String>, bool) {
    let live: std::collections::HashSet<String> = state
        .sessions
        .values()
        .filter(|s| is_member_session(s))
        .map(|s| s.workspace_id.clone())
        .collect();
    let mut released = Vec::new();
    let mut changed = false;
    for (id, ws) in state.workspaces.iter_mut() {
        if !claimed_stream(ws) {
            continue;
        }
        let idle = ws.org.as_ref().and_then(|o| o.get(IDLE_SINCE)).and_then(Value::as_f64);
        match (live.contains(id), idle) {
            (true, Some(_)) => {
                ws.org.as_mut().map(|o| o.remove(IDLE_SINCE));
                changed = true;
            }
            (true, None) => {}
            (false, None) => {
                bridge::set(&mut ws.org, IDLE_SINCE, json!(now));
                changed = true;
            }
            (false, Some(since)) if now - since >= STREAM_RELEASE_SECS => {
                unclaim(ws);
                released.push(id.clone());
                changed = true;
            }
            (false, Some(_)) => {}
        }
    }
    (released, changed)
}

/// Whether a teardown must keep this workspace: its kind (home, stream,
/// pinned) or a member's session (main or assistant) still in it.
pub fn workspace_kept(state: &DaemonState, ws_id: &str) -> Option<&'static str> {
    if let Some(why) = state.workspaces.get(ws_id).and_then(crate::lifecycle_guard::protected_workspace) {
        return Some(why);
    }
    let mut kept = None;
    for s in state.sessions.values().filter(|s| s.workspace_id == ws_id) {
        match of_session(s).map(|m| m.acting_as) {
            Some(MAIN) => return Some("member"),
            Some(ASSISTANT) => kept = Some("assistant"),
            _ => {}
        }
    }
    kept
}

/// Idle assistants of the member `caller_uid` works for (main or
/// assistant), for the worktree-cap error: reuse them before spawning.
pub fn idle_assistants_for(state: &DaemonState, caller_uid: &str) -> Vec<(String, String)> {
    let Some(member) = membership(caller_uid, org_of(state, caller_uid).as_ref())
        .filter(|m| m.acting_as != WORKER)
        .map(|m| m.member_id)
    else {
        return Vec::new();
    };
    let mut out: Vec<(String, String)> = state
        .sessions
        .values()
        .filter(|s| s.uid != caller_uid && stamped_member(s.org.as_ref()).as_deref() == Some(&member))
        .filter(|s| is_idle(s))
        .map(|s| (s.uid.clone(), s.title.clone()))
        .collect();
    out.sort();
    out
}

/// Idle by the engine's report, else PTY-quiet (the terminal sweep's rule).
fn is_idle(s: &DaemonSession) -> bool {
    let agent = crate::agent_state::current(s);
    if agent.source.engine_reported() && agent.state != crate::agent_state::State::Unknown {
        return matches!(agent.state, crate::agent_state::State::Idle | crate::agent_state::State::WaitingOnHuman);
    }
    let quiet = *s.last_activity_at.lock().unwrap_or_else(|p| p.into_inner());
    quiet.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(2))
}

/// The remediation appended to a worktree-cap refusal for `caller_uid`.
pub fn cap_hint(state: &DaemonState, caller_uid: &str) -> Option<String> {
    let idle = idle_assistants_for(state, caller_uid);
    if idle.is_empty() {
        return None;
    }
    let names: Vec<String> = idle.iter().map(|(uid, label)| format!("{label} ({uid})")).collect();
    let first = &idle[0].0;
    Some(format!(
        ". Your member has {} idle assistant(s): {}. Reuse one with \
         send_input(session_uid=\"{first}\", text=...), or free its checkout with \
         org(action=\"close_assistant\", session_uid=\"{first}\", reason=...)",
        idle.len(),
        names.join(", "),
    ))
}

/// Backfill (doc/org-model.md §4.2, S4 pulled forward for O5): stamp every
/// live, spawned, non-shell session that has no member yet and is not a
/// member's main.
///
/// 1. By spawner: walk `managed_by_uid` up to the first session with a
///    membership (main by the cache, or a stamp); the role follows
///    [`child_role`] at that link and is inherited below it.
/// 2. Only when that chain is broken (a spawner gone with no record; a chain
///    ending at a live non-member stays unstamped): walk the session's task
///    and its parents (the pushed task tree, then the refresher's planning
///    parents) to the first task with a member's main session on this daemon.
///
/// A session with no spawner (Owner- or operator-started) and a shell are
/// never stamped. Runs only once the member cache has heard from the API at
/// least once. Stamps are never changed here, only added; the nearest
/// member wins. Returns the stamped uids.
pub fn backfill(state: &mut DaemonState) -> Vec<String> {
    if !crate::items::org::cache_loaded() {
        return Vec::new();
    }
    let mut uids: Vec<&String> = state.sessions.keys().collect();
    uids.sort();
    // Task → the member whose main session works on it (lowest uid first,
    // so the answer is deterministic when two mains share a task).
    let mut task_main: HashMap<String, String> = HashMap::new();
    for uid in &uids {
        let s = &state.sessions[*uid];
        if let (Some(task), Some(member)) = (s.task_id.as_ref(), main_member(uid)) {
            task_main.entry(task.clone()).or_insert(member);
        }
    }
    let mut stamped: Vec<(String, String, &'static str, String)> = Vec::new();
    for uid in uids {
        let s = &state.sessions[uid];
        if has_member_key(s.org.as_ref())
            || main_member(uid).is_some()
            || is_shell(&s.session_type)
            || s.managed_by_uid.is_none()
        {
            continue;
        }
        let found = match by_spawner(state, s) {
            Chain::Member(found) => Some(found),
            // The chain reached a live (or recorded) session that belongs
            // to no member: so does this one. Live 2026-10-25 on cm-sessions
            // the task chain would have made Swarm-Coord's lanes
            // initiative-bridge's assistants through the hub task.
            Chain::NoMember => None,
            Chain::Broken => by_task(state, s, &task_main),
        };
        if let Some((member, role, via)) = found {
            stamped.push((uid.clone(), member, role, via));
        }
    }
    let mut out = Vec::new();
    for (uid, member, role, via) in stamped {
        let ws_id = match state.sessions.get_mut(&uid) {
            Some(s) => {
                stamp(&mut s.org, &member, role, &via);
                s.workspace_id.clone()
            }
            None => continue,
        };
        if role == ASSISTANT {
            claim_workspace(state, &ws_id, &member);
        }
        out.push(uid);
    }
    out
}

enum Chain {
    Member((String, &'static str, String)),
    /// The chain ends at a session (live or recorded) with no member.
    NoMember,
    /// A spawner on the chain is gone with no record: unknown.
    Broken,
}

fn by_spawner(state: &DaemonState, s: &DaemonSession) -> Chain {
    let Some(mut cur) = s.managed_by_uid.clone() else { return Chain::NoMember };
    for _ in 0..32 {
        if cur == s.uid {
            return Chain::NoMember;
        }
        let Some(spawner) = state.sessions.get(&cur) else {
            // An exited spawner still left its record (and stamp) behind.
            let org = crate::control::methods::persisted_org(state, &cur);
            return match membership(&cur, org.as_ref()) {
                Some(m) => Chain::Member((m.member_id.clone(), child_role(&m), format!("spawner:{cur}"))),
                None if org.is_some() || state.exited_tombstone(&cur).is_some() => Chain::NoMember,
                None => Chain::Broken,
            };
        };
        if let Some(m) = of_session(spawner) {
            return Chain::Member((m.member_id.clone(), child_role(&m), format!("spawner:{cur}")));
        }
        match spawner.managed_by_uid.clone() {
            Some(next) => cur = next,
            None => return Chain::NoMember,
        }
    }
    Chain::NoMember
}

fn by_task(
    state: &DaemonState,
    s: &DaemonSession,
    task_main: &HashMap<String, String>,
) -> Option<(String, &'static str, String)> {
    let mut task = s.task_id.clone()?;
    let mut seen = std::collections::HashSet::new();
    for _ in 0..64 {
        if !seen.insert(task.clone()) {
            return None;
        }
        if let Some(member) = task_main.get(&task) {
            let role = if is_continuous_member(member) { WORKER } else { ASSISTANT };
            return Some((member.clone(), role, format!("task:{task}")));
        }
        task = crate::control::auth::known_parent(state, &task).flatten()?;
    }
    None
}

/// The wire form `list_sessions` and the manifest show: `{member_id,
/// acting_as}` or null.
pub fn wire(uid: &str, org: Option<&OrgObject>) -> Value {
    match membership(uid, org) {
        Some(m) => json!({"member_id": m.member_id, "acting_as": m.acting_as}),
        None => Value::Null,
    }
}

#[cfg(test)]
mod tests;
