//! Org model S2 (doc/org-model.md §4, §6, §10-§12): daemon proxies to the
//! planning API's `/org` endpoints, the per-session facts the heartbeat
//! reports for the roster, and the member cache `ping` reads.
//!
//! The daemon stamps the actor as for items: Owner is the operator caller
//! (pid `owner`). Roster and adoption are Owner-only; the API re-checks
//! every write. Nothing here enforces anything: holds stay keyed by pid and
//! session scope is unchanged until S6/S7.
//!
//! Lock discipline: the `DaemonState` mutex is never held across an API call.

use super::rpc::{bad, Ctx, RpcResult};
use crate::control::protocol::{ErrorCode, Request, Response};
use crate::session::DaemonSession;
use crate::state::DaemonState;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

pub const METHODS: &[&str] = &[
    "org.snapshot",
    "org.member",
    "org.team",
    "org.roster",
    "org.roster_apply",
    "org.adopt",
    "org.team_create",
    "org.team_update",
    "org.move",
    "org.retire",
    "org.reinstate",
    "org.close_assistant",
    "org.promote",
];

/// The read-only ones (restart-barrier classification).
pub const READ_METHODS: &[&str] = &["org.snapshot", "org.member", "org.team", "org.roster"];

pub fn dispatch(state: &Arc<Mutex<DaemonState>>, req: &Request) -> Response {
    let result = Ctx::capture(state, req).and_then(|ctx| {
        let p = params(&req.params)?;
        match req.method.as_str() {
            "org.snapshot" => {
                let mut query = Vec::new();
                if let Some(v) = p.get("since_version").and_then(Value::as_i64) {
                    query.push(("since_version", v.to_string()));
                }
                api(ctx.api.get("/org/snapshot", &query))
            }
            "org.member" => member(state, &ctx, &p),
            "org.team" => {
                let team = str_param(&p, "team")?.ok_or_else(|| bad("org.team needs team"))?;
                api(ctx.api.get(&format!("/org/teams/{}", encode(&team)), &[]))
            }
            "org.roster" => {
                owner_only(&ctx, "the roster")?;
                api(ctx.api.get("/org/roster", &[]))
            }
            "org.roster_apply" => {
                owner_only(&ctx, "approving the roster")?;
                api(ctx.api.post("/org/roster/apply", &with_actor(&ctx, p)))
            }
            "org.adopt" => adopt(&ctx, p),
            "org.team_create" => api(ctx.api.post("/org/teams", &with_actor(&ctx, p))),
            "org.team_update" => {
                let mut p = p;
                let team = take_str(&mut p, "team")?.ok_or_else(|| bad("org.team_update needs team"))?;
                api(ctx.api.patch(&format!("/org/teams/{}", encode(&team)), &with_actor(&ctx, p)))
            }
            "org.move" | "org.retire" | "org.reinstate" => {
                if req.method == "org.reinstate" {
                    owner_only(&ctx, "reinstating a member")?;
                }
                let verb = &req.method["org.".len()..];
                let mut p = p;
                let member = take_str(&mut p, "member")?.ok_or_else(|| bad(format!("{} needs member", req.method)))?;
                api(ctx.api.post(&format!("/org/members/{}/{verb}", encode(&member)), &with_actor(&ctx, p)))
            }
            "org.close_assistant" => close_assistant(state, &ctx, p),
            "org.promote" => promote(state, &ctx, p),
            other => Err((ErrorCode::UnknownMethod, format!("unknown org method {other}"))),
        }
    });
    match result {
        Ok(value) => Response::ok(req.id.clone(), value),
        Err((code, message)) => Response::err(req.id.clone(), code, message),
    }
}

fn params(raw: &Value) -> RpcResult<Map<String, Value>> {
    match raw {
        Value::Null => Ok(Map::new()),
        Value::Object(map) => Ok(map.clone()),
        _ => Err(bad("expected object parameters")),
    }
}

fn str_param(p: &Map<String, Value>, key: &str) -> RpcResult<Option<String>> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.trim().to_string())),
        Some(_) => Err(bad(format!("{key} must be a non-empty string"))),
    }
}

fn take_str(p: &mut Map<String, Value>, key: &str) -> RpcResult<Option<String>> {
    let value = str_param(p, key)?;
    p.remove(key);
    Ok(value)
}

fn owner_only(ctx: &Ctx, what: &str) -> RpcResult<()> {
    if ctx.owner {
        Ok(())
    } else {
        Err((ErrorCode::Unauthorized, format!("owner_only: {what} is Owner's (the TUI roster screen)")))
    }
}

fn with_actor(ctx: &Ctx, mut p: Map<String, Value>) -> Value {
    p.insert("actor".into(), ctx.actor());
    Value::Object(p)
}

/// An old planning API lacks the route: FastAPI answers a bare `Not Found`,
/// or `Method Not Allowed` when the path matches a read route (a POST to
/// `/members/x/retire` hits `GET /members/{ref:path}`). Callers (the TUI)
/// read either as "feature off".
fn api(result: RpcResult) -> RpcResult {
    result.map_err(|(code, message)| {
        if (matches!(code, ErrorCode::NotFound) && message == "Not Found")
            || message == "Method Not Allowed"
        {
            (ErrorCode::UnknownMethod,
             "unsupported: the planning API has no such org endpoint yet (needs an API deploy)".into())
        } else {
            (code, message)
        }
    })
}

/// Percent-encode a path segment, keeping `/` (qualified handles `main/ep`
/// travel as a path).
fn encode(segment: &str) -> String {
    let mut out = String::new();
    for b in segment.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn member(state: &Arc<Mutex<DaemonState>>, ctx: &Ctx, p: &Map<String, Value>) -> RpcResult {
    if let Some(reference) = str_param(p, "member")? {
        return api(ctx.api.get(&format!("/org/members/{}", encode(&reference)), &[]));
    }
    let pid = match (str_param(p, "pid")?, str_param(p, "session_uid")?) {
        (Some(pid), _) => pid,
        (None, Some(uid)) => format!("agent:{}:{uid}", ctx.daemon_id),
        (None, None) if !ctx.owner => ctx.pid.clone(),
        (None, None) => return api(ctx.api.get("/org/members/owner", &[])),
    };
    // An assistant of a member on this daemon (O5) is known here, from its
    // stamp; a member's main session (the cache) still asks the API.
    let local = pid.strip_prefix(&format!("agent:{}:", ctx.daemon_id)).and_then(|uid| {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        let org = s.sessions.get(uid).map(|sess| sess.org.clone())
            .unwrap_or_else(|| crate::control::methods::persisted_org(&s, uid));
        match crate::assistants::membership(uid, org.as_ref()) {
            Some(m) if m.acting_as == crate::assistants::ASSISTANT => Some(json!({
                "pid": pid, "acting_as": "assistant", "member": {"id": m.member_id},
                "member_via": org.as_ref().and_then(|o| o.get(crate::assistants::MEMBER_VIA)).cloned(),
            })),
            _ => None,
        }
    });
    if let Some(answer) = local {
        return Ok(answer);
    }
    api(ctx.api.get("/org/members/by-pid", &[("pid", pid)]))
}

/// `org.close_assistant` (doc/org-model.md §4.3, O5): deliberately close
/// one of a member's assistants on this daemon. The planning API decides
/// authority (Owner, the member's main session, or a member up its manager
/// chain; the check shared with move and retire) and hands the assistant's
/// board holds back to the member's main session. Membership is checked
/// again after that answer, then the session is closed with that
/// attribution, and a `stream` checkout it was minted for goes back to
/// `ephemeral`, so the reaper's normal protections decide when it goes.
/// Against a planning API without the route, only Owner or the member's main
/// session may close it, and holds stay where they are.
fn close_assistant(state: &Arc<Mutex<DaemonState>>, ctx: &Ctx, mut p: Map<String, Value>) -> RpcResult {
    let uid = take_str(&mut p, "session_uid")?.ok_or_else(|| bad("org.close_assistant needs session_uid"))?;
    let reason = take_str(&mut p, "reason")?.unwrap_or_else(|| "closed by its member".into());
    let member_id = assistant_of(state, &uid)?;
    let pid = format!("agent:{}:{uid}", ctx.daemon_id);
    let mut body = Map::new();
    body.insert("pid".into(), json!(pid));
    body.insert("member_id".into(), json!(member_id));
    body.insert("reason".into(), json!(reason));
    if let Some(id) = p.get("request_id").cloned() {
        body.insert("request_id".into(), id);
    }
    let central = match api(ctx.api.post("/org/assistants/close", &with_actor(ctx, body))) {
        Ok(v) => v,
        Err((ErrorCode::UnknownMethod, _)) => {
            let own_main = ctx.uid.as_deref().and_then(crate::assistants::main_member);
            if !ctx.owner && own_main.as_deref() != Some(member_id.as_str()) {
                return Err((ErrorCode::Unauthorized, "not_manager: the planning API cannot check the \
                    manager chain yet; only Owner or the member's main session may close its assistant".into()));
            }
            json!({"holds": {"handed_back": [], "kept": "planning API predates close_assistant"}})
        }
        Err(e) => return Err(e),
    };
    let attribution = format!("{} (close_assistant)", if ctx.owner { "operator" } else { ctx.uid.as_deref().unwrap_or("?") });
    let released = {
        let mut s = state.lock().unwrap_or_else(|p| p.into_inner());
        // The API call ran unlocked: the session may have exited, been
        // revived as someone else's, or been closed meanwhile. Kill only what
        // is still this member's assistant.
        let still = s.sessions.get(&uid).and_then(crate::assistants::of_session);
        if still.as_ref().map(|m| (m.member_id.as_str(), m.acting_as))
            != Some((member_id.as_str(), crate::assistants::ASSISTANT))
        {
            return Err((ErrorCode::Conflict, format!(
                "changed: '{uid}' is no longer {member_id}'s live assistant; nothing was closed \
                 (holds already handed back: {})", central["holds"])));
        }
        let (_, released) = crate::assistants::end_assistant(&mut s, &uid).unwrap_or_default();
        if let Some(sess) = s.sessions.get_mut(&uid) {
            sess.last_exit.mark_operator_kill_requested();
            sess.set_kill_attribution(&attribution);
            let _ = sess.kill();
        }
        s.record_kill_request(&uid, &attribution, crate::agent_state::unix_now());
        s.persist_sessions_best_effort();
        released
    };
    Ok(json!({
        "closed": uid, "member_id": member_id, "killed_by": attribution,
        "holds": central["holds"], "workspace_released": released,
    }))
}

/// The member `uid` (live on this daemon) is an assistant of, or the
/// refusal that says what it is instead.
fn assistant_of(state: &Arc<Mutex<DaemonState>>, uid: &str) -> RpcResult<String> {
    let s = state.lock().unwrap_or_else(|p| p.into_inner());
    let Some(sess) = s.sessions.get(uid) else {
        return Err((ErrorCode::NotFound, format!("not_found: no live session '{uid}' on this daemon")));
    };
    match crate::assistants::of_session(sess) {
        Some(m) if m.acting_as == crate::assistants::ASSISTANT => Ok(m.member_id),
        Some(m) if m.acting_as == crate::assistants::WORKER => Err((ErrorCode::Conflict, format!(
            "not_an_assistant: '{uid}' is a continuous member's worker; close it with kill_session"))),
        Some(_) => Err((ErrorCode::Conflict, format!(
            "not_an_assistant: '{uid}' is a member's main session; replace or retire the member instead"))),
        None => Err((ErrorCode::Conflict, format!(
            "not_an_assistant: '{uid}' belongs to no member; close it with kill_session"))),
    }
}

/// After an agent's `kill_session` ended a member's assistant: hand its
/// holds back to the member's main session, off the dispatch thread. The
/// kill was already authorized by `kill_session`'s own scope, so the API
/// records it (`ended_by: kill_session`) without the close authority check.
/// Best-effort: a failure is logged and the holds stay (the board flags them
/// as held by an exited session).
pub(crate) fn hand_back_after_kill(state: &Arc<Mutex<DaemonState>>, req: &Request, uid: &str, member_id: &str) {
    let ctx = match Ctx::capture(state, req) {
        Ok(ctx) => ctx,
        Err((_, e)) => {
            eprintln!("cm org: holds of killed assistant {uid} not handed back: {e}");
            return;
        }
    };
    let body = json!({
        "pid": format!("agent:{}:{uid}", ctx.daemon_id), "member_id": member_id,
        "reason": "killed with kill_session", "ended_by": "kill_session", "actor": ctx.actor(),
        "request_id": format!("kill-{uid}"),
    });
    let uid = uid.to_string();
    std::thread::spawn(move || {
        if let Err((_, e)) = api(ctx.api.post("/org/assistants/close", &body)) {
            eprintln!("cm org: holds of killed assistant {uid} not handed back: {e}");
        }
    });
}

/// `org.promote` (doc/org-model.md §4.2): one of a member's assistants on
/// this daemon becomes the main session of a new member managed by that
/// member. The planning API decides who may (the member's main session, a
/// member up its manager chain, or Owner) and creates the member through
/// the adopt/roster path. The session keeps its uid, context, checkout and
/// holds. Then, here: the assistant stamp goes, the member cache learns the
/// new main at once (so the backfill cannot re-stamp it before the next
/// beat), its checkout becomes the member's `home`, and the session gets a
/// native notice.
fn promote(state: &Arc<Mutex<DaemonState>>, ctx: &Ctx, mut p: Map<String, Value>) -> RpcResult {
    let uid = match take_str(&mut p, "session")? {
        Some(uid) => uid,
        None => take_str(&mut p, "session_uid")?.ok_or_else(|| bad("org.promote needs session"))?,
    };
    let handle = take_str(&mut p, "handle")?.ok_or_else(|| bad("org.promote needs handle"))?;
    let reason = take_str(&mut p, "reason")?.ok_or_else(|| bad("org.promote needs reason"))?;
    let boss = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        let Some(sess) = s.sessions.get(&uid) else {
            return Err((ErrorCode::NotFound, format!("not_found: no live session '{uid}' on this daemon")));
        };
        if !matches!(sess.session_type.as_str(), "claude-code" | "codex") {
            return Err((ErrorCode::Conflict, format!(
                "not_an_agent: '{uid}' is a {} session; only agent sessions become members", sess.session_type)));
        }
        if sess.continuous_task_id.is_some() {
            return Err((ErrorCode::Conflict, format!(
                "continuous: '{uid}' is a continuous orchestrator; its scheduler owns it")));
        }
        match crate::assistants::of_session(sess) {
            Some(m) if m.acting_as == crate::assistants::ASSISTANT => m.member_id,
            Some(m) if m.acting_as == crate::assistants::MAIN => {
                return Err((ErrorCode::Conflict, format!("already_member: '{uid}' is already a member's main session")));
            }
            // Member-less, or a member's non-durable worker (a continuous
            // worker): only an assistant is promoted.
            _ => {
                return Err((ErrorCode::Conflict, format!("not_an_assistant: '{uid}' is not an assistant of a member")));
            }
        }
    };
    let pid = format!("agent:{}:{uid}", ctx.daemon_id);
    let mut body = Map::new();
    body.insert("pid".into(), json!(pid));
    body.insert("handle".into(), json!(handle));
    body.insert("reason".into(), json!(reason));
    for key in ["title", "team", "manager", "request_id"] {
        if let Some(v) = p.get(key).filter(|v| !v.is_null()) {
            body.insert(key.into(), v.clone());
        }
    }
    let reply = api(ctx.api.post("/org/assistants/promote", &with_actor(ctx, body)))?;
    let member = reply["member"].clone();
    let member_id = member["id"].as_str().unwrap_or_default().to_string();
    let (root, ws_marked) = {
        let mut s = state.lock().unwrap_or_else(|p| p.into_inner());
        note_main(&uid, &member);
        let ws_id = match s.sessions.get_mut(&uid) {
            Some(sess) => {
                // A main's record: the stamp's keys now name the new member as
                // `main`, so neither the spawn stamp nor the backfill re-stamps
                // it, and the cache (which wins) says the same.
                crate::assistants::stamp(&mut sess.org, &member_id, crate::assistants::MAIN,
                                         &format!("promote:{}", if ctx.owner { "operator" } else { ctx.uid.as_deref().unwrap_or("?") }));
                Some(sess.workspace_id.clone())
            }
            None => None,
        };
        // The checkout is the new member's home now (the central mark says
        // the same and is acked on the next beat).
        let marked = ws_id.and_then(|w| s.workspaces.get_mut(&w)).map(|ws| {
            use cm_holder_proto::org as bridge;
            bridge::set(&mut ws.org, bridge::WORKSPACE_KIND, json!("home"));
            bridge::set(&mut ws.org, OFFICE_OF, json!(member_id));
            if let Some(o) = ws.org.as_mut() {
                o.remove(crate::assistants::STREAM_OF);
                o.remove(crate::assistants::IDLE_SINCE);
            }
        }).is_some();
        s.persist_sessions_best_effort();
        (s.messaging_root.clone(), marked)
    };
    save_cache(&root);
    let manager = reply["member"]["manager_handle"].as_str().unwrap_or("owner").to_string();
    let marker = "[cm-org]";
    let text = format!(
        "{marker} You are now member {handle} (main session), reporting to {manager}. Promoted by {} \
         ({reason}). You keep this conversation, checkout and your board holds; this checkout is \
         your home workspace. Agents you spawn from now on are your assistants. org(view=\"me\") shows it.",
        if ctx.owner { "Owner".to_string() } else { ctx.name.clone() });
    let noticed = crate::notifications::publish(&root, &uid, &format!("org-promoted:{member_id}"), "cm", &text, marker)
        .map_err(|e| eprintln!("cm org: promote notice to {uid}: {e}"))
        .is_ok();
    let mut out = reply;
    out["session_uid"] = json!(uid);
    out["was_assistant_of"] = json!(boss);
    out["home_marked_here"] = json!(ws_marked);
    out["noticed"] = json!(noticed);
    Ok(out)
}

/// Record `uid` as `member`'s main session in the member cache before the
/// next beat confirms it (a promote on this daemon).
pub(crate) fn note_main(uid: &str, member: &Value) {
    let mut entry = member.clone();
    if !entry.is_object() {
        return;
    }
    entry["acting_as"] = json!("main");
    let mut c = cache().lock().unwrap_or_else(|p| p.into_inner());
    c.by_uid.insert(uid.to_string(), entry);
}

/// `org.adopt`: Owner binds a live session to a new member. The TUI may
/// not know a remote host's daemon id, so `session_uid` (resolved against
/// this daemon) can stand in for `pid`; when both are given they must agree.
fn adopt(ctx: &Ctx, mut p: Map<String, Value>) -> RpcResult {
    owner_only(ctx, "adopting a session")?;
    let uid = take_str(&mut p, "session_uid")?;
    let pid = str_param(&p, "pid")?;
    let pid = match (pid, uid) {
        (Some(pid), Some(uid)) if pid != format!("agent:{}:{uid}", ctx.daemon_id) => {
            return Err(bad(format!("pid {pid} and session_uid {uid} name different sessions")));
        }
        (Some(pid), _) => pid,
        (None, Some(uid)) => format!("agent:{}:{uid}", ctx.daemon_id),
        (None, None) => return Err(bad("org.adopt needs pid or session_uid")),
    };
    if ctx.people.iter().any(|s| s["id"] == pid && (s["kind"] == "shell" || s["engine"] == "bash")) {
        return Err(bad("agent_only: adopt requires an agent session; bash is a shell"));
    }
    p.insert("pid".into(), json!(pid));
    api(ctx.api.post("/org/adopt", &with_actor(ctx, p)))
}

/// `GET /org/members/by-pid` for a session on this daemon (the `org` MCP
/// tool's `view="me"`). `Ok(None)` when the planning API predates S2.
pub fn member_for_session(state: &Arc<Mutex<DaemonState>>, uid: &str) -> RpcResult<Option<Value>> {
    let (handle, api_url, api_token) = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        (s.messaging.clone(), s.config.api_url.clone(), s.config.api_token.clone())
    };
    let daemon_id = {
        let slot = handle.lock().unwrap_or_else(|p| p.into_inner());
        slot.as_ref().map(|s| s.daemon_id.clone())
    }
    .ok_or_else(|| (ErrorCode::Internal, "daemon_id_unavailable: messaging store is not open".to_string()))?;
    let client = super::api::Api::from_config(&api_url, &api_token)?;
    match api(client.get("/org/members/by-pid", &[("pid", format!("agent:{daemon_id}:{uid}"))])) {
        Ok(v) => Ok(Some(v)),
        Err((ErrorCode::UnknownMethod, _)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// A team's detail (`GET /org/teams/{ref}`); `Ok(None)` on an old API or an
/// unknown team.
pub fn team_detail(state: &Arc<Mutex<DaemonState>>, reference: &str) -> RpcResult<Option<Value>> {
    let (api_url, api_token) = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        (s.config.api_url.clone(), s.config.api_token.clone())
    };
    let client = super::api::Api::from_config(&api_url, &api_token)?;
    match api(client.get(&format!("/org/teams/{}", encode(reference)), &[])) {
        Ok(v) => Ok(Some(v)),
        Err((ErrorCode::UnknownMethod | ErrorCode::NotFound, _)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// `GET /org/members/{ref}` (the `org` MCP tool's `view="member"`).
pub fn member_detail(state: &Arc<Mutex<DaemonState>>, reference: &str) -> RpcResult {
    let (api_url, api_token) = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        (s.config.api_url.clone(), s.config.api_token.clone())
    };
    let client = super::api::Api::from_config(&api_url, &api_token)?;
    api(client.get(&format!("/org/members/{}", encode(reference)), &[]))
}

// ---- heartbeat: facts out, member cache in ----------------------------------

/// The per-session facts the roster needs, merged into this session's
/// heartbeat row. An old API ignores unknown row keys.
pub(crate) fn session_facts(state: &DaemonState, sess: &DaemonSession, now: f64) -> Map<String, Value> {
    let mut facts = Map::new();
    let worktree = state
        .workspaces
        .get(&sess.workspace_id)
        .and_then(|w| w.worktree_path.as_ref())
        .map(|p| p.display().to_string());
    facts.insert("workspace_id".into(), json!(sess.workspace_id));
    facts.insert("worktree_path".into(), json!(worktree));
    facts.insert("continuous_task_id".into(), json!(sess.continuous_task_id));
    facts.insert("managed_by_uid".into(), json!(sess.managed_by_uid));
    facts.insert("workflow_run_id".into(), json!(sess.workflow_run_id));
    // O5: whom an assistant or worker works for (the API learns mains from
    // incarnations).
    if let Some((member, role)) = crate::assistants::stamped(sess.org.as_ref()) {
        if crate::assistants::main_member(&sess.uid).is_none() {
            facts.insert("member_id".into(), json!(member));
            facts.insert("acting_as".into(), json!(role));
        }
    }
    if let Some(age) = spawned_age_s(&sess.uid, now) {
        facts.insert("spawned_age_s".into(), json!(age));
    }
    facts
}

/// Seconds since the spawn stamped in a `ts-<nanos hex>-<counter>` uid
/// (A-R keeps the uid, so this is the session's original start).
pub(crate) fn spawned_age_s(uid: &str, now: f64) -> Option<f64> {
    let (rank, nanos, _) = crate::org::spawn_order(uid);
    if rank != 0 {
        return None;
    }
    let age = now - (nanos as f64) / 1e9;
    (age >= 0.0 && age < 10.0 * 365.0 * 86400.0).then_some(age)
}

#[derive(Default)]
struct MemberCache {
    /// session uid → member summary (`acting_as: main`), from the last reply.
    by_uid: HashMap<String, Value>,
    at: Option<f64>,
}

fn cache() -> &'static Mutex<MemberCache> {
    static CACHE: OnceLock<Mutex<MemberCache>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

/// Fold a heartbeat reply's `org.members` into the cache. A reply without
/// the key (an API before S2) leaves the cache as it was.
pub(crate) fn absorb_reply(reply: &Value, now: f64) {
    let Some(members) = reply["org"]["members"].as_object() else { return };
    let mut c = cache().lock().unwrap_or_else(|p| p.into_inner());
    c.by_uid = members.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    c.at = Some(now);
}

fn cache_path(root: &std::path::Path) -> std::path::PathBuf {
    root.join("org-members.json")
}

/// Persist the member cache beside `launch-grants.json`. Teardown reads it
/// to spare members' main sessions (lifecycle_guard_v1), so after a brain
/// restart it must not start empty and let `mark_subtask_done` close a
/// member before the first beat (review of 6522623).
pub(crate) fn save_cache(root: &std::path::Path) {
    let snapshot = {
        let c = cache().lock().unwrap_or_else(|p| p.into_inner());
        json!({"at": c.at, "members": c.by_uid})
    };
    let tmp = cache_path(root).with_extension("json.tmp");
    if let Ok(bytes) = serde_json::to_vec(&snapshot) {
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, cache_path(root));
        }
    }
}

/// Load the persisted cache at brain start, unless a beat already filled it.
pub(crate) fn load_cache(root: &std::path::Path) {
    let Some(saved) = std::fs::read(cache_path(root)).ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else { return };
    let mut c = cache().lock().unwrap_or_else(|p| p.into_inner());
    if c.at.is_some() {
        return;
    }
    if let Some(members) = saved["members"].as_object() {
        c.by_uid = members.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        c.at = saved["at"].as_f64();
    }
}

/// Apply the home/stream workspace marks a heartbeat reply hands this
/// daemon (`reply.org.workspace_marks`, doc/org-model.md §5 and §12) and
/// return the ids to ack on the next beat. A mark sets the workspace's
/// `org.workspace_kind` and `org.office_of` (the member); it overrides a
/// local kind, since the central roster and adoption are authoritative. A
/// mark for a workspace this daemon does not hold, matched by id and then
/// by path, stays unacked: the API pushes it again until its attempts run
/// out. Persisted (best effort, like every lifecycle write) before the ids
/// are returned; a re-push of an applied mark is a no-op that acks again.
pub(crate) fn apply_marks(state: &mut DaemonState, reply: &Value) -> Vec<i64> {
    use cm_holder_proto::org as bridge;
    let Some(marks) = reply["org"]["workspace_marks"].as_array() else { return Vec::new() };
    let mut acked = Vec::new();
    let mut changed = false;
    for mark in marks {
        let Some(id) = mark["id"].as_i64() else { continue };
        let kind = match mark["kind"].as_str() {
            Some(k @ ("home" | "stream")) => k,
            other => {
                eprintln!("cm org: workspace mark {id} has unknown kind {other:?}; not applied");
                continue;
            }
        };
        let ws_id = mark["workspace_id"].as_str().filter(|w| state.workspaces.contains_key(*w));
        let ws_id = ws_id.map(str::to_string).or_else(|| {
            let path = std::path::Path::new(mark["path"].as_str()?);
            let mut hits = state
                .workspaces
                .iter()
                .filter(|(_, ws)| ws.worktree_path.as_deref() == Some(path));
            match (hits.next(), hits.next()) {
                (Some((id, _)), None) => Some(id.clone()),
                _ => None,
            }
        });
        let Some(ws) = ws_id.and_then(|w| state.workspaces.get_mut(&w)) else {
            eprintln!("cm org: workspace mark {id} names a workspace this daemon does not hold");
            continue;
        };
        let member = mark["member_id"].clone();
        let org = ws.org.as_ref();
        if bridge::workspace_kind(org) != Some(kind) || org.and_then(|o| o.get(OFFICE_OF)) != Some(&member) {
            bridge::set(&mut ws.org, bridge::WORKSPACE_KIND, Value::String(kind.into()));
            bridge::set(&mut ws.org, OFFICE_OF, member);
            changed = true;
        }
        acked.push(id);
    }
    if changed {
        state.persist_sessions_best_effort();
    }
    acked
}

/// The workspace org-object key naming the member whose home it is.
pub const OFFICE_OF: &str = "office_of";

/// A member's `kind` (`agent`, `continuous`, …) when the cache holds it,
/// i.e. when its main session is on this daemon.
pub fn cached_member_kind(member_id: &str) -> Option<String> {
    let c = cache().lock().unwrap_or_else(|p| p.into_inner());
    c.by_uid.values().find(|m| m["id"] == member_id).and_then(|m| m["kind"].as_str()).map(str::to_string)
}

/// Whether the member cache has heard from an API that serves members (a
/// beat, or the persisted cache at brain start). The assistant backfill
/// waits for it, so a brain that has seen no beat stamps nothing.
pub fn cache_loaded() -> bool {
    cache().lock().unwrap_or_else(|p| p.into_inner()).at.is_some()
}

/// The member a session on this daemon works as, from the last heartbeat
/// (at most ~30 s old). Never blocks on the network: `ping` reads it.
pub fn cached_member(uid: &str) -> Value {
    let c = cache().lock().unwrap_or_else(|p| p.into_inner());
    match (c.by_uid.get(uid), c.at) {
        (Some(m), Some(at)) => {
            let mut m = m.clone();
            m["as_of"] = json!(chrono::DateTime::from_timestamp(at as i64, 0).map(|t| t.to_rfc3339()));
            m
        }
        _ => Value::Null,
    }
}

#[cfg(test)]
pub(crate) fn reset_cache_for_test() {
    *cache().lock().unwrap_or_else(|p| p.into_inner()) = MemberCache::default();
}

/// A stub planning API for org tests: `reply(method, path, body)`; every
/// request is recorded as (method, path, body).
#[cfg(test)]
pub(crate) fn test_stub(
    reply: impl Fn(&str, &str, &Value) -> (u16, String) + Send + 'static,
) -> (String, Arc<Mutex<Vec<(String, String, Value)>>>) {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let seen: Arc<Mutex<Vec<(String, String, Value)>>> = Arc::default();
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            if reader.read_line(&mut first).is_err() {
                continue;
            }
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line.trim().is_empty() {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let parts: Vec<&str> = first.split_whitespace().collect();
            let (method, path) = (parts[0].to_string(), parts[1].to_string());
            let value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let (status, text) = reply(&method, &path, &value);
            log.lock().unwrap().push((method, path, value));
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                text.len()
            );
        }
    });
    (url, seen)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_member_cache_survives_a_brain_restart() {
        // The cache is process-global: every test that writes it holds the lock.
        let _env = crate::test_support::env_lock();
        let root = tempfile::tempdir().unwrap();
        absorb_reply(&json!({"org": {"members": {"persist-uid": {"id": "mem_p", "acting_as": "main"}}}}),
                     1_700_000_000.0);
        save_cache(root.path());
        reset_cache_for_test();
        assert!(cached_member("persist-uid").is_null(), "a fresh brain starts empty");
        load_cache(root.path());
        assert_eq!(cached_member("persist-uid")["acting_as"], "main");
        reset_cache_for_test();
    }

    use super::*;
    use crate::control::protocol::Caller;

    fn state(root: &std::path::Path, url: &str) -> Arc<Mutex<DaemonState>> {
        let mut st = DaemonState::default();
        st.messaging_root = root.into();
        st.config.api_url = url.into();
        st.config.api_token = "tok".into();
        st.tui_sessions.insert(
            "me-uid".into(),
            serde_json::from_value(json!({"uid":"me-uid","label":"me","task_id":"t-1","type":"codex"})).unwrap(),
        );
        let state = Arc::new(Mutex::new(st));
        crate::messaging::startup::open_for_test(&state).unwrap();
        state
    }

    fn daemon_id(state: &Arc<Mutex<DaemonState>>) -> String {
        let handle = state.lock().unwrap().messaging.clone();
        let slot = handle.lock().unwrap();
        slot.as_ref().unwrap().daemon_id.clone()
    }

    fn call(state: &Arc<Mutex<DaemonState>>, caller: Caller, method: &str, params: Value) -> Response {
        dispatch(state, &Request { id: "t".into(), caller, method: method.into(), params })
    }

    #[test]
    fn adopt_and_the_roster_are_owners_and_adopt_fills_in_this_daemons_id() {
        let _env = crate::test_support::env_lock();
        let root = tempfile::tempdir().unwrap();
        let (url, seen) = test_stub(|method, path, body| match (method, path) {
            ("POST", "/org/adopt") => (200, json!({"member": {"handle": body["handle"]}, "home_mark": null}).to_string()),
            ("GET", "/org/roster") => (200, r#"{"proposals":[],"declined":[],"last_applied_at":null}"#.into()),
            _ => (500, "{}".into()),
        });
        let state = state(root.path(), &url);
        let d = daemon_id(&state);
        let me = Caller::session("me-uid");
        for (method, params) in [("org.adopt", json!({"session_uid": "u2", "handle": "rev"})),
                                 ("org.roster", json!({})),
                                 ("org.roster_apply", json!({"rows": []}))] {
            let resp = call(&state, me.clone(), method, params);
            assert!(matches!(resp.error.unwrap().code, ErrorCode::Unauthorized), "{method}");
        }
        let owner = Caller::operator("op");
        let resp = call(&state, owner.clone(), "org.adopt", json!({"session_uid": "u2", "handle": "rev", "request_id": "r1"}));
        assert!(resp.ok, "{:?}", resp.error);
        let resp = call(&state, owner.clone(), "org.adopt",
                        json!({"session_uid": "u2", "pid": "agent:other:u2", "handle": "rev"}));
        assert!(matches!(resp.error.unwrap().code, ErrorCode::InvalidParams));
        assert!(call(&state, owner, "org.roster", json!({})).ok);
        let seen = seen.lock().unwrap();
        let (_, _, body) = seen.iter().find(|(m, p, _)| m == "POST" && p == "/org/adopt").unwrap();
        assert_eq!(body["pid"], format!("agent:{d}:u2"));
        assert_eq!(body["actor"]["pid"], "owner");
        assert!(body.get("session_uid").is_none());
        assert_eq!(seen.iter().filter(|(m, p, _)| m == "POST" && p == "/org/adopt").count(), 1);
    }

    #[test]
    fn reads_and_moves_proxy_with_the_callers_actor() {
        let _env = crate::test_support::env_lock();
        let root = tempfile::tempdir().unwrap();
        let (url, seen) = test_stub(|method, path, _| match (method, path) {
            ("GET", "/org/snapshot?since_version=7") => (200, r#"{"unchanged":true,"version":7}"#.into()),
            ("GET", p) if p.starts_with("/org/members/by-pid?pid=") => (200, r#"{"member":null}"#.into()),
            ("POST", "/org/members/main/ep/move") => (200, r#"{"member":{"handle":"ep"}}"#.into()),
            ("GET", "/org/teams/swarm") => (404, r#"{"detail":"Not Found"}"#.into()),
            _ => (500, "{}".into()),
        });
        let state = state(root.path(), &url);
        let d = daemon_id(&state);
        let me = Caller::session("me-uid");
        let resp = call(&state, me.clone(), "org.snapshot", json!({"since_version": 7}));
        assert_eq!(resp.result.unwrap()["unchanged"], true);
        assert!(call(&state, me.clone(), "org.member", json!({})).ok);
        let resp = call(&state, me.clone(), "org.move",
                        json!({"member": "main/ep", "manager": "owner", "reason": "r"}));
        assert!(resp.ok, "{:?}", resp.error);
        // A planning API before S2: the TUI turns the feature off.
        let err = call(&state, me, "org.team", json!({"team": "swarm"})).error.unwrap();
        assert!(matches!(err.code, ErrorCode::UnknownMethod) && err.message.starts_with("unsupported"));
        let seen = seen.lock().unwrap();
        let (_, path, _) = seen.iter().find(|(_, p, _)| p.starts_with("/org/members/by-pid")).unwrap();
        assert_eq!(path, &format!("/org/members/by-pid?pid=agent%3A{d}%3Ame-uid"));
        let (_, _, body) = seen.iter().find(|(m, _, _)| m == "POST").unwrap();
        assert_eq!(body["actor"]["pid"], format!("agent:{d}:me-uid"));
        assert!(body.get("member").is_none());
    }

    #[test]
    fn org_tool_move_retire_and_reinstate_route_to_the_api_as_the_caller() {
        let _env = crate::test_support::env_lock();
        let root = tempfile::tempdir().unwrap();
        let (url, seen) = test_stub(|method, path, _| match (method, path) {
            ("POST", p) if p.starts_with("/org/members/main/ep/") => (200, r#"{"member":{"handle":"ep"}}"#.into()),
            _ => (500, "{}".into()),
        });
        let state = state(root.path(), &url);
        let d = daemon_id(&state);
        let tool = |caller: Caller, params: Value| {
            crate::control::dispatch::dispatch_org(
                &state, &Request { id: "t".into(), caller, method: "org".into(), params })
        };
        let me = Caller::session("me-uid");
        let resp = tool(me.clone(), json!({"action": "move", "member": "main/ep", "manager": "kestrel",
                                           "team": "swarm", "reason": "r", "request_id": "q1"}));
        assert!(resp.ok, "{:?}", resp.error);
        assert!(tool(me.clone(), json!({"action": "retire", "member": "main/ep", "reason": "done"})).ok);
        assert!(tool(me.clone(), json!({"action": "move", "member": "main/ep", "team": "none", "reason": "r"})).ok);
        // Retire takes no manager; reinstate is Owner's, refused before the API.
        let err = tool(me.clone(), json!({"action": "retire", "member": "main/ep", "manager": "x", "reason": "r"}));
        assert!(matches!(err.error.unwrap().code, ErrorCode::InvalidParams));
        let err = tool(me, json!({"action": "reinstate", "member": "main/ep"})).error.unwrap();
        assert!(matches!(err.code, ErrorCode::Unauthorized) && err.message.starts_with("owner_only"));
        assert!(tool(Caller::operator("op"), json!({"action": "reinstate", "member": "main/ep"})).ok);
        let seen = seen.lock().unwrap();
        let posts: Vec<_> = seen.iter().filter(|(m, _, _)| m == "POST").collect();
        assert_eq!(posts.iter().map(|(_, p, _)| p.as_str()).collect::<Vec<_>>(),
                   ["/org/members/main/ep/move", "/org/members/main/ep/retire", "/org/members/main/ep/move",
                    "/org/members/main/ep/reinstate"]);
        // `team="none"` (or "") clears the primary team: an explicit null.
        assert!(posts[2].2.get("primary_team_id").is_some_and(Value::is_null), "{:?}", posts[2].2);
        let move_body = &posts[0].2;
        assert_eq!(move_body["actor"]["pid"], format!("agent:{d}:me-uid"));
        assert_eq!((&move_body["manager"], &move_body["primary_team_id"], &move_body["request_id"]),
                   (&json!("kestrel"), &json!("swarm"), &json!("q1")));
        assert!(move_body.get("member").is_none() && move_body.get("action").is_none());
        assert_eq!(posts[3].2["actor"]["pid"], "owner");
    }

    #[test]
    fn path_segments_are_encoded_but_qualified_handles_keep_their_slash() {
        assert_eq!(encode("main/ep"), "main/ep");
        assert_eq!(encode("a b?c"), "a%20b%3Fc");
    }

    #[test]
    fn spawn_age_comes_from_the_uid() {
        let now = 2_000_000_000.0;
        let uid = format!("ts-{:x}-1", (now as u128 - 3600) * 1_000_000_000);
        let age = spawned_age_s(&uid, now).unwrap();
        assert!((age - 3600.0).abs() < 1.0, "{age}");
        assert_eq!(spawned_age_s("legacy-uid", now), None);
    }

    #[test]
    fn the_cache_follows_replies_and_ignores_old_apis() {
        let _env = crate::test_support::env_lock();
        reset_cache_for_test();
        assert!(cached_member("u1").is_null());
        absorb_reply(&json!({"org": {"members": {"u1": {"id": "mem_1", "handle": "ep", "acting_as": "main"}}}}), 1_700_000_000.0);
        assert_eq!(cached_member("u1")["handle"], "ep");
        assert!(cached_member("u1")["as_of"].as_str().is_some());
        absorb_reply(&json!({"pushes": []}), 1_700_000_100.0); // pre-S2 API
        assert_eq!(cached_member("u1")["handle"], "ep");
        absorb_reply(&json!({"org": {"members": {}}}), 1_700_000_200.0);
        assert!(cached_member("u1").is_null());
        reset_cache_for_test();
    }

    #[test]
    fn an_old_api_reads_as_unsupported() {
        let err = api(Err((ErrorCode::NotFound, "Not Found".into()))).unwrap_err();
        assert!(matches!(err.0, ErrorCode::UnknownMethod));
        assert!(err.1.starts_with("unsupported"));
        let (code, body) = crate::items::api::map_error(405, r#"{"detail":"Method Not Allowed"}"#);
        let err = api(Err((code, body))).unwrap_err();
        assert!(matches!(err.0, ErrorCode::UnknownMethod) && err.1.contains("needs an API deploy"), "{err:?}");
        let err = api(Err((ErrorCode::NotFound, "not_found: no member 'x'".into()))).unwrap_err();
        assert!(matches!(err.0, ErrorCode::NotFound));
    }

    fn ws(id: &str, path: &str, org: Value) -> crate::manifest::ManifestWorkspace {
        serde_json::from_value(json!({"id": id, "worktree_path": path, "org": org, "sessions": []})).unwrap()
    }

    fn marks(list: Value) -> Value {
        json!({"org": {"workspace_marks": list}})
    }

    /// Org model S2's home push (doc/org-model.md §12): a mark finds its
    /// workspace by id, then by a unique path; overrides a local kind;
    /// keeps the org object's other keys; persists; and is acked. A mark
    /// for a workspace this daemon does not hold, or with an unknown kind,
    /// stays unacked so the API retries it.
    #[test]
    fn workspace_marks_apply_persist_and_ack() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = DaemonState::default();
        st.daemon_sessions_path = Some(dir.path().join("daemon-sessions.json"));
        st.workspaces.insert("ws-a".into(), ws("ws-a", "/w/a", json!({"workspace_kind": "ephemeral", "later": 1})));
        st.workspaces.insert("ws-b".into(), ws("ws-b", "/w/b", Value::Null));
        st.workspaces.insert("ws-c".into(), ws("ws-c", "/w/same", Value::Null));
        st.workspaces.insert("ws-d".into(), ws("ws-d", "/w/same", Value::Null));
        let reply = marks(json!([
            {"id": 1, "workspace_id": "ws-a", "path": "/w/a", "kind": "home", "member_id": "mem_1", "handle": "ep"},
            // Unknown id on this daemon: found by its path.
            {"id": 2, "workspace_id": "elsewhere", "path": "/w/b", "kind": "stream", "member_id": "mem_2"},
            // Two workspaces share the path: no guess.
            {"id": 3, "workspace_id": "nope", "path": "/w/same", "kind": "home", "member_id": "mem_3"},
            {"id": 4, "workspace_id": "nope", "path": "/w/none", "kind": "home", "member_id": "mem_4"},
            {"id": 5, "workspace_id": "ws-c", "kind": "ephemeral", "member_id": "mem_5"},
            {"workspace_id": "ws-d", "kind": "home", "member_id": "mem_6"},
        ]));
        assert_eq!(apply_marks(&mut st, &reply), vec![1, 2]);
        let a = st.workspaces["ws-a"].org.clone().unwrap();
        assert_eq!(a["workspace_kind"], "home", "the central mark overrides a local kind");
        assert_eq!(a[OFFICE_OF], "mem_1");
        assert_eq!(a["later"], 1, "other org keys are kept");
        assert_eq!(crate::lifecycle_guard::workspace_kind(&st.workspaces["ws-a"]),
                   crate::lifecycle_guard::WorkspaceKind::Home);
        assert_eq!(st.workspaces["ws-b"].org.as_ref().unwrap()["workspace_kind"], "stream");
        assert!(st.workspaces["ws-c"].org.is_none() && st.workspaces["ws-d"].org.is_none());
        let saved = std::fs::read_to_string(dir.path().join("daemon-sessions.json")).unwrap();
        let saved: Value = serde_json::from_str(&saved).unwrap();
        assert_eq!(saved["workspaces"]["ws-a"]["org"][OFFICE_OF], "mem_1", "{saved}");
        // The mark outlives the workspace's sessions: a fresh brain loads
        // it, while an unmarked session-less workspace is not persisted.
        let mut fresh = DaemonState::default();
        assert!(fresh.load_manifest_from_disk(&dir.path().join("daemon-sessions.json")).unwrap());
        assert_eq!(fresh.workspaces["ws-a"].org.as_ref().unwrap()[OFFICE_OF], "mem_1");
        assert!(!fresh.workspaces.contains_key("ws-c"));
        // A re-push (the ack was lost) is a no-op that acks again.
        std::fs::remove_file(dir.path().join("daemon-sessions.json")).unwrap();
        let again = marks(json!([{"id": 1, "workspace_id": "ws-a", "kind": "home", "member_id": "mem_1"}]));
        assert_eq!(apply_marks(&mut st, &again), vec![1]);
        assert!(!dir.path().join("daemon-sessions.json").exists(), "nothing changed: no rewrite");
        // A reply without marks (an older API) applies nothing.
        assert!(apply_marks(&mut st, &json!({"pushes": []})).is_empty());
    }
}
