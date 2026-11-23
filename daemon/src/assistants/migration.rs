//! One-time legacy placement. The task and workspace rules in this module are
//! deliberately confined to unstamped sessions restored from before §3.
use super::*;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Destination {
    Member(String, String, &'static str),
    LeadlessTeam(String),
    Owner,
    Pending,
}

fn role(snapshot: &Value, member: &str) -> &'static str {
    if snapshot["members"].as_array().into_iter().flatten()
        .any(|m| m["id"] == member && m["kind"] == "continuous") { WORKER } else { ASSISTANT }
}

/// Resolve exactly once using the former F6 order. Unknown task parents defer
/// the decision; an incomplete startup cache must never become a permanent
/// Owner stamp.
fn legacy_placement(state: &DaemonState, snapshot: &Value, daemon_id: &str, uid: &str) -> Destination {
    let Some(members) = snapshot["members"].as_array() else { return Destination::Pending };
    let Some(teams) = snapshot["teams"].as_array() else { return Destination::Pending };
    let Some(legacy_tasks) = snapshot["legacy_main_tasks"].as_object() else { return Destination::Pending };
    let by_id: HashMap<&str, &Value> = members.iter().filter_map(|m| Some((m["id"].as_str()?, m))).collect();
    let depth = |m: &Value| {
        let mut current = m["manager_member_id"].as_str();
        let mut depth = 0;
        while let Some(parent) = current.and_then(|id| by_id.get(id)).filter(|p| p["kind"] != "owner" && depth < 32) {
            depth += 1;
            current = parent["manager_member_id"].as_str();
        }
        depth
    };
    let mut people: Vec<&Value> = members.iter().filter(|m| m["kind"] != "owner" && m["status"] != "retired").collect();
    people.sort_by_key(|m| (depth(m), m["handle"].as_str().unwrap_or("").to_owned(), m["id"].as_str().unwrap_or("").to_owned()));
    let mut task_to = HashMap::<String, Destination>::new();
    for team in teams.iter().filter(|t| t["status"] != "archived") {
        if let Some(task) = team["coordinator_task_id"].as_str() {
            let dest = match team["lead_member_id"].as_str() {
                Some(id) if by_id.contains_key(id) => Destination::Member(id.into(), format!("task:{task}"), role(snapshot, id)),
                _ => Destination::LeadlessTeam(team["name"].as_str().or_else(|| team["id"].as_str()).unwrap_or("unknown").into()),
            };
            task_to.entry(task.into()).or_insert(dest);
        }
    }
    let mut member_tasks = HashMap::<String, Destination>::new();
    let mut workspace_to = HashMap::<String, String>::new();
    for m in &people {
        let Some(id) = m["id"].as_str() else { continue };
        let home = &m["home_workspace"];
        if home["daemon_id"].as_str() == Some(daemon_id) {
            let home_ws = state.workspaces.iter().find(|(ws_id, ws)| !ws.is_closed &&
                (home["workspace_id"].as_str() == Some(ws_id.as_str()) ||
                 home["path"].as_str().is_some_and(|p| ws.worktree_path.as_deref() == Some(std::path::Path::new(p)))));
            if let Some((ws, _)) = home_ws { workspace_to.entry(ws.clone()).or_insert(id.into()); }
        }
        if let Some(task) = legacy_tasks.get(id).and_then(Value::as_str) {
            member_tasks.entry(task.into()).or_insert(Destination::Member(id.into(), format!("task:{task}"), role(snapshot, id)));
        }
    }
    // F6 applies member tasks over coordinator tasks.
    task_to.extend(member_tasks);
    for m in &people {
        let Some(id) = m["id"].as_str() else { continue };
        let main = &m["main"];
        if main["daemon_id"].as_str() != Some(daemon_id) { continue }
        if let Some(session) = main["session_uid"].as_str().and_then(|u| state.sessions.get(u)) {
            if state.workspaces.get(&session.workspace_id).is_some_and(|w| !w.is_closed) {
                workspace_to.entry(session.workspace_id.clone()).or_insert(id.into());
            }
        }
    }
    let Some(start) = state.sessions.get(uid) else { return Destination::Pending };
    if let Some((id, child_role, via)) = by_spawner(state, start) {
        return Destination::Member(id, via, child_role);
    }
    let mut chain = vec![start];
    let mut seen = HashSet::from([uid.to_owned()]);
    while let Some(next) = chain.last().and_then(|s| s.managed_by_uid.as_deref()) {
        if !seen.insert(next.to_owned()) || chain.len() >= 32 { break }
        let Some(session) = state.sessions.get(next) else { break };
        chain.push(session);
    }
    for session in chain.into_iter().rev() {
        let mut task = session.task_id.clone();
        let mut tasks = HashSet::new();
        while let Some(t) = task.filter(|t| tasks.insert(t.clone()) && tasks.len() <= 64) {
            if let Some(dest) = task_to.get(&t) { return dest.clone() }
            match crate::control::auth::known_parent(state, &t) {
                Some(parent) => task = parent,
                None => return Destination::Pending,
            }
        }
        if let Some(member) = workspace_to.get(&session.workspace_id) {
            return Destination::Member(member.clone(), "workspace:legacy".into(), role(snapshot, member));
        }
    }
    Destination::Owner
}

pub(crate) fn needed(state: &DaemonState) -> bool {
    state.sessions.values().any(|s| matches!(s.session_type.as_str(), "claude" | "claude-code" | "codex")
        && s.org.as_ref().is_none_or(|o| o.get(PLACEMENT_V1).and_then(Value::as_bool) != Some(true)
            && o.get(ACTING_AS).and_then(Value::as_str).is_none()))
}

pub(crate) fn run(state: &mut DaemonState, snapshot: &Value, daemon_id: &str) -> Vec<String> {
    let mut uids: Vec<String> = state.sessions.keys().cloned().collect();
    uids.sort_by_key(|uid| {
        let mut depth = 0usize;
        let mut current = state.sessions.get(uid).and_then(|s| s.managed_by_uid.as_deref());
        let mut seen = HashSet::new();
        while let Some(parent) = current.filter(|p| seen.insert(*p) && depth < 32) {
            depth += 1;
            current = state.sessions.get(parent).and_then(|s| s.managed_by_uid.as_deref());
        }
        (depth, uid.clone())
    });
    let mut changed = Vec::new();
    for uid in uids {
        let s = &state.sessions[&uid];
        if !matches!(s.session_type.as_str(), "claude" | "claude-code" | "codex")
            || s.org.as_ref().is_some_and(|o| o.get(PLACEMENT_V1).and_then(Value::as_bool) == Some(true)
                || o.get(ACTING_AS).and_then(Value::as_str).is_some()) { continue }
        let fact = snapshot["members"].as_array().into_iter().flatten().find_map(|member| {
            let id = member["id"].as_str()?;
            let fact = member["assistants"].as_array()?.iter().find(|row|
                row["daemon_id"].as_str() == Some(daemon_id) && row["session_uid"].as_str() == Some(&uid))?;
            let acting = match fact["acting_as"].as_str()? {
                "main" => MAIN, "assistant" => ASSISTANT, "worker" => WORKER, _ => return None,
            };
            Some(Destination::Member(id.to_owned(), "fact:legacy".into(), acting))
        });
        let destination = if let Some(member) = main_member(&uid) {
            Destination::Member(member, "main:legacy".into(), MAIN)
        } else if let Some(fact) = fact { fact }
        else { legacy_placement(state, snapshot, daemon_id, &uid) };
        if destination == Destination::Pending { continue }
        if matches!(destination, Destination::Owner | Destination::LeadlessTeam(_))
            && spawner_is_awaiting_owner(state, s) {
            if let Destination::LeadlessTeam(team) = &destination {
                let session = state.sessions.get_mut(&uid).unwrap();
                if session.org.as_ref().and_then(|o| o.get("legacy_leadless_logged")).and_then(Value::as_bool) != Some(true) {
                    eprintln!("cm org: leadless-team migration uid={uid} label={:?} team={team:?}", session.title);
                    bridge::set(&mut session.org, "legacy_leadless_logged", json!(true));
                    changed.push(uid.clone());
                }
            }
            continue;
        }
        let s = state.sessions.get_mut(&uid).unwrap();
        let mut claim = None;
        match destination {
            Destination::Member(member, via, acting) => {
                stamp(&mut s.org, &member, acting, &via);
                if acting == ASSISTANT { claim = Some((s.workspace_id.clone(), member)); }
            }
            Destination::LeadlessTeam(team) => {
                eprintln!("cm org: leadless-team migration uid={uid} label={:?} team={team:?}", s.title);
                bridge::set(&mut s.org, AUTO_PLACE_OWNER, json!(true));
                bridge::set(&mut s.org, AUTO_PLACE_UNRESOLVED, json!(true));
            }
            Destination::Owner => {
                bridge::set(&mut s.org, AUTO_PLACE_OWNER, json!(true));
                bridge::set(&mut s.org, AUTO_PLACE_UNRESOLVED, json!(true));
            }
            Destination::Pending => unreachable!(),
        }
        bridge::set(&mut s.org, PLACEMENT_V1, json!(true));
        if let Some((workspace, member)) = claim { claim_workspace(state, &workspace, &member); }
        changed.push(uid);
    }
    changed
}
