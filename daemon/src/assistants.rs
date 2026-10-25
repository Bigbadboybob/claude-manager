//! Durable assistants (doc/org-model.md §4.2, §4.3, Owner decision O5).
//!
//! Any agent spawn by a member's main session, or by one of its assistants,
//! is an **assistant** of that member: as durable as the main session. It
//! keeps its session, context and checkout until it is deliberately closed
//! (`org.close_assistant`) by its member's main session, the member's
//! manager chain or Owner. Task/item completion, `mark_subtask_done` and the
//! terminal sweep never close it, and its own checkout is a `stream`
//! workspace of the member's office rather than `ephemeral`.
//!
//! The stamp lives in the session's org object (doc/org-model.md §12):
//! `member_id`, `acting_as = "assistant"` and `member_via` (how it was
//! learned: `spawn:<uid>`, `spawner:<uid>`, `task:<id>`). Mains are never
//! stamped here: whether a session is a member's main comes only from the
//! API's member cache ([`crate::items::org::cached_member`]), and it wins
//! over any stamp. A session that belongs to no member keeps today's
//! behaviour.
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
pub const MAIN: &str = "main";
/// Set on an assistant's org object when it is deliberately closed: its
/// tombstone then no longer protects its checkout (the reaper reads it).
pub const CLOSED_AT: &str = "assistant_closed_at";

/// Whom a session works for, and how.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Membership {
    pub member_id: String,
    /// [`MAIN`] or [`ASSISTANT`].
    pub acting_as: &'static str,
}

/// The member `uid` is the main session of, by the member cache.
pub fn main_member(uid: &str) -> Option<String> {
    let m = crate::items::org::cached_member(uid);
    (m["acting_as"] == MAIN).then(|| m["id"].as_str().map(str::to_string)).flatten()
}

/// The assistant stamp in an org object, when there is one.
pub fn stamped_member(org: Option<&OrgObject>) -> Option<String> {
    let org = org?;
    if org.get(ACTING_AS)?.as_str()? != ASSISTANT || org.contains_key(CLOSED_AT) {
        return None;
    }
    org.get(MEMBER_ID)?.as_str().filter(|m| !m.is_empty()).map(str::to_string)
}

/// Whom `uid` (with org object `org`) works for: a main by the cache wins
/// over an assistant stamp.
pub fn membership(uid: &str, org: Option<&OrgObject>) -> Option<Membership> {
    if let Some(member_id) = main_member(uid) {
        return Some(Membership { member_id, acting_as: MAIN });
    }
    stamped_member(org).map(|member_id| Membership { member_id, acting_as: ASSISTANT })
}

pub fn of_session(sess: &DaemonSession) -> Option<Membership> {
    membership(&sess.uid, sess.org.as_ref())
}

/// A member's main session or one of its assistants: never closed by
/// finishing work.
pub fn is_member_session(sess: &DaemonSession) -> bool {
    of_session(sess).is_some()
}

pub fn is_assistant(sess: &DaemonSession) -> bool {
    of_session(sess).is_some_and(|m| m.acting_as == ASSISTANT)
}

/// Stamp `org` as an assistant of `member_id`.
pub fn stamp(org: &mut Option<OrgObject>, member_id: &str, via: &str) {
    bridge::set(org, MEMBER_ID, json!(member_id));
    bridge::set(org, ACTING_AS, json!(ASSISTANT));
    bridge::set(org, MEMBER_VIA, json!(via));
}

/// Remove an assistant stamp (a deliberate close). Other keys stay.
pub fn unstamp(org: &mut Option<OrgObject>) {
    if let Some(o) = org.as_mut() {
        for key in [MEMBER_ID, ACTING_AS, MEMBER_VIA] {
            o.remove(key);
        }
    }
}

/// True when a record's org object already names a member (assistant or a
/// later stage's `main`): the backfill and the spawn stamp leave it alone.
fn has_member_key(org: Option<&OrgObject>) -> bool {
    org.is_some_and(|o| o.get(MEMBER_ID).is_some_and(|v| !v.is_null()))
}

/// The member a spawn by `spawner_uid` works for, if any: the spawner's own
/// membership (main or assistant).
pub fn member_for_spawn(state: &DaemonState, spawner_uid: &str) -> Option<String> {
    let org = state
        .sessions
        .get(spawner_uid)
        .map(|s| s.org.clone())
        .unwrap_or_else(|| crate::control::methods::persisted_org(state, spawner_uid));
    membership(spawner_uid, org.as_ref()).map(|m| m.member_id)
}

/// The spawn-time stamp: a child of a member's session is that member's
/// assistant. Leaves an org object that already names a member (a respawn at
/// the same uid) or the main of a member (cache) untouched.
pub fn stamp_spawn(
    state: &DaemonState,
    uid: &str,
    spawner_uid: Option<&str>,
    org: &mut Option<OrgObject>,
) -> bool {
    if has_member_key(org.as_ref()) || main_member(uid).is_some() {
        return false;
    }
    let Some(spawner) = spawner_uid.filter(|s| *s != uid) else { return false };
    let Some(member_id) = member_for_spawn(state, spawner) else { return false };
    stamp(org, &member_id, &format!("spawn:{spawner}"));
    true
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
    bridge::set(&mut ws.org, "stream_of", json!(ASSISTANT));
    true
}

/// After a deliberate close: a `stream` checkout this module claimed for
/// assistants goes back to `ephemeral` once no member session remains in
/// it, so the reaper's normal protections (no live process, not dirty,
/// evidence released) decide when it goes. A central mark (`home`/`stream`
/// from the roster) is never downgraded.
pub fn release_workspace(state: &mut DaemonState, ws_id: &str, closing_uid: &str) -> bool {
    let still_used = state.sessions.values().any(|s| {
        s.workspace_id == ws_id && s.uid != closing_uid && is_member_session(s)
    });
    let Some(ws) = state.workspaces.get_mut(ws_id) else { return false };
    let claimed = ws.org.as_ref().is_some_and(|o| o.get("stream_of") == Some(&json!(ASSISTANT)));
    if still_used || !claimed || bridge::workspace_kind(ws.org.as_ref()) != Some("stream") {
        return false;
    }
    bridge::set(&mut ws.org, bridge::WORKSPACE_KIND, json!(bridge::KIND_EPHEMERAL));
    if let Some(o) = ws.org.as_mut() {
        o.remove(crate::items::org::OFFICE_OF);
        o.remove("stream_of");
    }
    true
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
            Some(_) => kept = Some("assistant"),
            None => {}
        }
    }
    kept
}

/// Backfill (doc/org-model.md §4.2, S4 pulled forward for O5): stamp every
/// live session that has no member yet and is not a member's main.
///
/// 1. By spawner: walk `managed_by_uid` up to the first session with a
///    membership (main by the cache, or an assistant stamp).
/// 2. Else by task: walk the session's task and its parents (the pushed task
///    tree, then the refresher's planning parents) to the first task with a
///    member's main session on this daemon.
///
/// Runs only once the member cache has heard from the API at least once,
/// so a brain that has not seen a beat yet stamps nothing. Stamps are
/// never changed here, only added; the nearest member wins, so a lane's
/// workers belong to the lane, not to the lane's lead. Returns the stamped
/// uids.
pub fn backfill(state: &mut DaemonState) -> Vec<String> {
    if !crate::items::org::cache_loaded() {
        return Vec::new();
    }
    // Task → the member whose main session works on it (lowest uid first,
    // so the answer is deterministic when two mains share a task).
    let mut uids: Vec<&String> = state.sessions.keys().collect();
    uids.sort();
    let mut task_main: HashMap<String, String> = HashMap::new();
    for uid in &uids {
        let s = &state.sessions[*uid];
        if let (Some(task), Some(member)) = (s.task_id.as_ref(), main_member(uid)) {
            task_main.entry(task.clone()).or_insert(member);
        }
    }
    let mut stamped: Vec<(String, String, String)> = Vec::new();
    for uid in uids {
        let s = &state.sessions[uid];
        if has_member_key(s.org.as_ref()) || main_member(uid).is_some() {
            continue;
        }
        let found = by_spawner(state, s).or_else(|| by_task(state, s, &task_main));
        if let Some((member, via)) = found {
            stamped.push((uid.clone(), member, via));
        }
    }
    let mut out = Vec::new();
    for (uid, member, via) in stamped {
        let ws_id = match state.sessions.get_mut(&uid) {
            Some(s) => {
                stamp(&mut s.org, &member, &via);
                s.workspace_id.clone()
            }
            None => continue,
        };
        claim_workspace(state, &ws_id, &member);
        out.push(uid);
    }
    out
}

fn by_spawner(state: &DaemonState, s: &DaemonSession) -> Option<(String, String)> {
    let mut cur = s.managed_by_uid.clone()?;
    for _ in 0..32 {
        if cur == s.uid {
            return None;
        }
        let Some(spawner) = state.sessions.get(&cur) else {
            // An exited spawner still left its stamp behind.
            let org = crate::control::methods::persisted_org(state, &cur);
            return membership(&cur, org.as_ref()).map(|m| (m.member_id, format!("spawner:{cur}")));
        };
        if let Some(m) = of_session(spawner) {
            return Some((m.member_id, format!("spawner:{cur}")));
        }
        cur = spawner.managed_by_uid.clone()?;
    }
    None
}

fn by_task(
    state: &DaemonState,
    s: &DaemonSession,
    task_main: &HashMap<String, String>,
) -> Option<(String, String)> {
    let mut task = s.task_id.clone()?;
    let mut seen = std::collections::HashSet::new();
    for _ in 0..64 {
        if !seen.insert(task.clone()) {
            return None;
        }
        if let Some(member) = task_main.get(&task) {
            return Some((member.clone(), format!("task:{task}")));
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
