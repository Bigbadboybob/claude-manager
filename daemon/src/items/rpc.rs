//! Daemon methods `item.create`, `item.set`, `item.resolve`, `board.read`
//! (doc/items-board.md §7).
//!
//! Lock discipline: the `DaemonState` mutex is never held across a planning
//! API call. The caller's context is snapshotted under the lock first.

use super::api::Api;
use crate::control::protocol::{Caller, ErrorCode, Request, Response};
use crate::messaging::Person;
use crate::state::DaemonState;
use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use std::sync::{Arc, Mutex};

pub const METHODS: &[&str] = &[
    "item.create", "item.set", "item.resolve", "board.read", "board.settings", "board.owner_blocked",
    // Org model S3 (doc/org-model.md §7): placement, groups, the on-boards list.
    "item.place", "item.unplace", "group.create", "group.set", "boards.on_boards",
    "epic.set_poc", "epic.staff", "epic.close_plan", "epic.close_apply",
    // #28: board membership (spawn-time setup), and ending it.
    "board.join", "board.leave", "board.remove_member",
];

pub(crate) type RpcResult<T = Value> = Result<T, (ErrorCode, String)>;

pub(crate) fn bad(message: impl Into<String>) -> (ErrorCode, String) {
    (ErrorCode::InvalidParams, message.into())
}

pub fn dispatch(state: &Arc<Mutex<DaemonState>>, req: &Request) -> Response {
    let result = Ctx::capture(state, req).and_then(|ctx| match req.method.as_str() {
        "item.create" => item_create(&ctx, &req.params),
        "item.set" => item_set(&ctx, &req.params),
        "item.resolve" => item_resolve(state, &ctx, &req.params),
        "board.read" => board_read(&ctx, &req.params),
        "board.settings" => board_settings(&ctx, &req.params),
        "item.place" => item_place(state, &ctx, &req.params),
        "item.unplace" => item_unplace(state, &ctx, &req.params),
        "group.create" => group_create(&ctx, &req.params),
        "group.set" => group_set(&ctx, &req.params),
        "board.join" => board_join(&ctx, &req.params),
        "board.leave" => board_leave(&ctx, &req.params, None),
        "board.remove_member" => {
            let reference = req.params.get("session").and_then(Value::as_str).filter(|s| !s.trim().is_empty())
                .ok_or_else(|| bad("session (a chat name, uid or participant id) is required"));
            reference.and_then(|r| board_leave(&ctx, &req.params, Some(r)))
        }
        "epic.set_poc" | "epic.staff" | "epic.close_plan" | "epic.close_apply" => {
            epic_call(&ctx, req.method.as_str(), &req.params)
        }
        // Read-only, open to every session: epics and board-native items.
        "boards.on_boards" => {
            let since: Vec<(&str, String)> = req.params.get("since_version").and_then(Value::as_i64)
                .map(|v| vec![("since_version", v.to_string())]).unwrap_or_default();
            ctx.api.get("/items/on-boards", &since)
        }
        // Every item blocked on Owner, across boards (the TUI's sidebar marker).
        "board.owner_blocked" => ctx.api.get("/items/owner-blocked", &[]),
        other => Err((ErrorCode::UnknownMethod, format!("unknown items method {other}"))),
    });
    match result {
        Ok(value) => Response::ok(req.id.clone(), value),
        Err((code, message)) => Response::err(req.id.clone(), code, message),
    }
}

/// The actor a task write is attributed to (doc/org-model.md §7.1): the
/// session's participant id and name, so a translated legacy status write on
/// a placed task is the session's, not the daemon's API token. None when the
/// messaging store is not open (no participant identity) or the uid is
/// unknown. Takes the state lock, releases it, then the store slot (the
/// usual order).
pub fn session_actor(state: &Arc<Mutex<DaemonState>>, uid: &str) -> Option<Value> {
    let (handle, title, task_id) = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        let sess = s.sessions.get(uid)?;
        (s.messaging.clone(), sess.title.clone(), sess.task_id.clone())
    };
    let slot = handle.lock().unwrap_or_else(|p| p.into_inner());
    let store = slot.as_ref()?;
    let name = store
        .names
        .get(&store.participant_id(uid))
        .map(|n| n.name.clone())
        .unwrap_or(title);
    Some(json!({
        "pid": format!("agent:{}:{uid}", store.daemon_id),
        "name": name,
        "session_uid": uid,
        "daemon_id": store.daemon_id,
        "task_id": task_id,
    }))
}

/// Who is calling, resolved on this daemon.
pub(crate) struct Ctx {
    pub(crate) s4: std::collections::HashMap<String, Value>,
    pub(crate) s4_wire: bool,
    pub pid: String,
    pub name: String,
    pub uid: Option<String>,
    pub task_id: Option<String>,
    pub owner: bool,
    pub global: bool,
    pub engine: Option<String>,
    /// Owned by this daemon's registry (spawnable-from), not a TUI-only row.
    pub daemon_owned: bool,
    pub daemon_id: String,
    /// Every known participant: `{id, name, session_uid, aliases?, released?, present}`.
    pub people: Vec<Value>,
    pub api: Api,
}

impl Ctx {
    pub(crate) fn capture(state: &Arc<Mutex<DaemonState>>, req: &Request) -> RpcResult<Self> {
        if matches!(req.caller, Caller::Operator(_)) {
            crate::control::operator::validate_operator(&req.caller)
                .map_err(|e| (ErrorCode::Unauthorized, e.to_string()))?;
        }
        if !req.params.is_object() && !req.params.is_null() {
            return Err(bad("expected object parameters"));
        }
        let (s4_snapshot, s4_wire) = super::s4_attribution::current();
        let (me, live, handle, api_url, api_token, subjects) = {
            let s = state.lock().unwrap_or_else(|p| p.into_inner());
            let subjects = s4_snapshot.keys().filter_map(|uid| s.sessions.get(uid)
                .map(|session| (uid.clone(), super::s4_attribution::Subject::from_session(session))))
                .collect::<std::collections::HashMap<_, _>>();
            let me = match &req.caller {
                Caller::Operator(_) => None,
                Caller::Session(c) => {
                    let uid = c.session_uid.clone();
                    if let Some(sess) = s.sessions.get(&uid) {
                        Some((uid, sess.title.clone(), sess.task_id.clone(), sess.global_perms,
                              Some(sess.session_type.clone()), true))
                    } else if let Some(t) = s.tui_sessions.get(&uid) {
                        Some((uid.clone(), t.label.clone().unwrap_or(uid), t.task_id.clone(),
                              t.global_perms, t.session_type.clone(), false))
                    } else {
                        return Err((
                            ErrorCode::Unauthorized,
                            "item tools need a registered CM session".into(),
                        ));
                    }
                }
            };
            let live = crate::messaging::live_sessions(&s);
            (me, live, s.messaging.clone(), s.config.api_url.clone(), s.config.api_token.clone(), subjects)
        };
        if me.as_ref().is_some_and(|(_, _, _, _, engine, _)| engine.as_deref() == Some("bash")) {
            return Err(bad("agent_only: item and org tools require an agent; bash is a shell"));
        }
        let (daemon_id, people, my_name) = {
            let slot = handle.lock().unwrap_or_else(|p| p.into_inner());
            let store = slot.as_ref().ok_or_else(|| {
                (
                    ErrorCode::Internal,
                    "daemon_id_unavailable: this daemon's messaging store is not open, so it \
                     has no participant identity for item holders"
                        .to_string(),
                )
            })?;
            let mut persons: Vec<Person> = live
                .into_iter()
                .map(|live| Person::present_agent(store.participant_id(&live.uid), live))
                .collect();
            persons.extend(store.remote_people());
            let people = store.people(&persons).as_array().cloned().unwrap_or_default();
            let my_name = me
                .as_ref()
                .and_then(|(uid, ..)| store.names.get(&store.participant_id(uid)))
                .map(|n| n.name.clone());
            (store.daemon_id.clone(), people, my_name)
        };
        let api = Api::from_config(&api_url, &api_token)?;
        let s4 = super::s4_attribution::matching(s4_snapshot, &subjects);
        Ok(match me {
            None => Ctx {
                s4, s4_wire,
                pid: "owner".into(),
                name: "Owner".into(),
                uid: None,
                task_id: None,
                owner: true,
                global: true,
                engine: None,
                daemon_owned: false,
                daemon_id,
                people,
                api,
            },
            Some((uid, title, task_id, global, engine, daemon_owned)) => Ctx {
                s4, s4_wire,
                pid: format!("agent:{daemon_id}:{uid}"),
                name: my_name.unwrap_or(title),
                uid: Some(uid),
                task_id,
                owner: false,
                global,
                engine,
                daemon_owned,
                daemon_id,
                people,
                api,
            },
        })
    }

    pub(crate) fn actor(&self) -> Value {
        let mut actor = json!({
            "pid": self.pid,
            "name": self.name,
            "session_uid": self.uid,
            "daemon_id": if self.owner { Value::Null } else { json!(self.daemon_id) },
            "task_id": self.task_id,
        });
        if self.s4_wire {
            if let Some(uid) = self.uid.as_ref() {
                actor["attribution"] = self.s4.get(uid).cloned().unwrap_or(json!({"provenance":"unknown"}));
            }
        }
        actor
    }

    fn me(&self) -> Value {
        self.holder(&self.pid, Some(&self.name))
    }

    fn holder(&self, pid: &str, name: Option<&str>) -> Value {
        let mut value = holder_value(pid, name);
        if self.s4_wire && value["session_uid"].is_string() {
            let claim = (value["daemon_id"].as_str() == Some(self.daemon_id.as_str()))
                .then(|| value["session_uid"].as_str().and_then(|u| self.s4.get(u))).flatten();
            value["attribution"] = claim.cloned().unwrap_or(json!({"provenance":"unknown"}));
        }
        value
    }
}

/// `{pid, name, session_uid, daemon_id}` for a participant id.
fn holder_value(pid: &str, name: Option<&str>) -> Value {
    let mut parts = pid.splitn(3, ':');
    let (daemon_id, uid) = match (parts.next(), parts.next(), parts.next()) {
        (Some("agent"), Some(d), Some(u)) => (json!(d), json!(u)),
        _ => (Value::Null, Value::Null),
    };
    json!({"pid": pid, "name": name, "session_uid": uid, "daemon_id": daemon_id})
}

/// Resolve one holder reference: `me`, a participant id, a session uid, or a
/// chat name (or alias), case-insensitive, with or without a leading `@`.
pub(crate) fn resolve_holder(ctx: &Ctx, reference: &str) -> RpcResult {
    let r = reference.trim().trim_start_matches('@');
    if r.is_empty() {
        return Err(bad("empty holder"));
    }
    if ctx.people.iter().any(|p| {
        (p["kind"] == "shell" || p["engine"] == "bash")
            && (p["id"] == r || p["session_uid"] == r
                || p["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(r))
                || p["aliases"].as_array().is_some_and(|a| a.iter().any(|n| n.as_str().is_some_and(|n| n.eq_ignore_ascii_case(r)))))
    }) {
        return Err(bad("agent_only: bash is a shell and cannot hold work items"));
    }
    if r.eq_ignore_ascii_case("me") && ctx.engine.as_deref() == Some("bash") {
        return Err(bad("agent_only: bash is a shell and cannot hold work items"));
    }
    if r.eq_ignore_ascii_case("me") {
        return Ok(ctx.me());
    }
    if r.eq_ignore_ascii_case("owner") {
        return Ok(holder_value("owner", Some("Owner")));
    }
    let name_of = |p: &Value| p["name"].as_str().map(str::to_string);
    if r.starts_with("agent:") {
        if r.splitn(3, ':').count() != 3 {
            return Err(bad(format!("malformed participant id {r}")));
        }
        let name = ctx.people.iter().find(|p| p["id"] == r).and_then(name_of);
        return Ok(ctx.holder(r, name.as_deref()));
    }
    if let Some(p) = ctx.people.iter().find(|p| p["session_uid"] == r && p["id"].as_str().is_some()) {
        return Ok(ctx.holder(p["id"].as_str().unwrap(), name_of(p).as_deref()));
    }
    let key = r.to_lowercase();
    let matches_name = |p: &&Value| {
        p["name"].as_str().is_some_and(|n| n.to_lowercase() == key)
            || p["aliases"].as_array().is_some_and(|a| {
                a.iter().any(|x| x.as_str().is_some_and(|n| n.to_lowercase() == key))
            })
    };
    let mut found: Vec<&Value> = ctx
        .people
        .iter()
        .filter(|p| p["kind"] != "owner")
        .filter(matches_name)
        .collect();
    if found.len() > 1 {
        // A current name beats an old alias or a released record.
        let current: Vec<&Value> = found
            .iter()
            .copied()
            .filter(|p| p["released"] != true)
            .filter(|p| p["name"].as_str().is_some_and(|n| n.to_lowercase() == key))
            .collect();
        if !current.is_empty() {
            found = current;
        }
    }
    found.dedup_by(|a, b| a["id"] == b["id"]);
    match found.as_slice() {
        [one] => Ok(ctx.holder(one["id"].as_str().unwrap_or_default(), name_of(one).as_deref())),
        [] => Err((
            ErrorCode::NotFound,
            format!("no session named {r}: use a chat name, session uid or participant id (chat_people lists them)"),
        )),
        many => Err(bad(format!(
            "{r} is ambiguous: {}; pass a participant id",
            many.iter()
                .map(|p| format!("{} ({})", p["name"].as_str().unwrap_or("?"), p["id"].as_str().unwrap_or("?")))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// A holder argument: one reference or a list; `none` means no holders.
fn holders_arg(ctx: &Ctx, value: &Value) -> RpcResult<Vec<Value>> {
    let refs: Vec<&str> = match value {
        Value::String(s) => vec![s.as_str()],
        Value::Array(items) => items
            .iter()
            .map(|v| v.as_str().ok_or_else(|| bad("holder entries must be strings")))
            .collect::<RpcResult<_>>()?,
        _ => return Err(bad("holder must be a name, uid, participant id, `me` or `none`")),
    };
    if refs.len() == 1 && refs[0].trim().eq_ignore_ascii_case("none") {
        return Ok(vec![]);
    }
    if refs.iter().any(|r| r.trim().eq_ignore_ascii_case("none")) {
        return Err(bad("`none` cannot be combined with other holders"));
    }
    refs.into_iter().map(|r| resolve_holder(ctx, r)).collect()
}

pub(crate) fn present(params: &Value, key: &str) -> Option<Value> {
    params.get(key).filter(|v| !v.is_null()).cloned()
}

pub(crate) fn int_list(value: &Value, name: &str) -> RpcResult<Vec<i64>> {
    match value {
        Value::Number(n) => n.as_i64().map(|n| vec![n]).ok_or_else(|| bad(format!("{name} must be integers"))),
        Value::Array(items) => items
            .iter()
            .map(|v| v.as_i64().ok_or_else(|| bad(format!("{name} must be integers"))))
            .collect(),
        _ => Err(bad(format!("{name} must be an integer or a list of integers"))),
    }
}

/// The caller's board: `board` if given, else its task's board.
pub(crate) fn resolve_board(ctx: &Ctx, params: &Value) -> RpcResult {
    if let Some(reference) = params.get("board").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
        return ctx.api.post("/boards/resolve", &json!({"ref": reference.trim()}));
    }
    match &ctx.task_id {
        Some(task_id) => ctx.api.post("/boards/resolve", &json!({"task_id": task_id})),
        None => Err(bad("no board: this session has no task; pass board=<slug>")),
    }
}

/// Editing rights (doc §2 rule 9): Owner, a global session, a session whose
/// task resolves to the board, a current holder of any item on it, or a
/// session added to it (`board.join`, #28).
pub(crate) fn authorize_edit(ctx: &Ctx, params: &Value, board: &Value) -> RpcResult<()> {
    if ctx.owner || ctx.global {
        return Ok(());
    }
    let explicit = params.get("board").and_then(Value::as_str).is_some_and(|s| !s.trim().is_empty());
    if let Some(task_id) = &ctx.task_id {
        if !explicit {
            return Ok(());
        }
        let mine = ctx.api.post("/boards/resolve", &json!({"task_id": task_id}))?;
        if mine["id"] == board["id"] {
            return Ok(());
        }
    }
    let held = ctx.api.get("/items", &[("holder_pid", ctx.pid.clone()), ("open", "true".into())])?;
    if held.as_array().is_some_and(|rows| rows.iter().any(|r| r["board"] == board["slug"])) {
        return Ok(());
    }
    // Added to the board by someone with authority over it (#28).
    if let Ok(slug) = slug(board) {
        let joined = ctx.api.get(&format!("/boards/{slug}/members"), &[("pid", ctx.pid.clone())]);
        if joined.is_ok_and(|v| v["members"].as_array().is_some_and(|m| !m.is_empty())) {
            return Ok(());
        }
    }
    Err((
        ErrorCode::Unauthorized,
        format!(
            "board {} is not your task's board and you hold no item on it; reading it with board() is allowed",
            board["slug"].as_str().unwrap_or("?")
        ),
    ))
}

pub(crate) fn slug(board: &Value) -> RpcResult<String> {
    board["slug"].as_str().map(str::to_string).ok_or_else(|| {
        (ErrorCode::Internal, "planning API returned a board without a slug".into())
    })
}

/// Fields passed straight through to the API; "" clears a text field.
const PASS_FIELDS: &[&str] = &[
    "status", "note", "group", "blocked_on", "check_back", "eta", "job", "title", "links", "blocked_by",
    // Org model S3: a group by id, a board-native item's description, its team.
    "group_id", "description", "team_id",
    // Gate bindings (doc/gate-receipts.md §8).
    "head", "mf", "ruling", "gate_class",
];
const CLEARABLE: &[&str] = &["note", "group", "group_id", "description", "team_id", "blocked_on", "check_back", "eta", "job", "head", "mf", "ruling", "gate_class"];

fn item_fields(ctx: &Ctx, params: &Value, skip_title: bool) -> RpcResult<Map<String, Value>> {
    let mut fields = Map::new();
    for key in PASS_FIELDS {
        if skip_title && *key == "title" {
            continue;
        }
        // Item updates distinguish omitted scheduling fields from explicit
        // null; the API validates whether clearing each field is allowed.
        let value = if matches!(*key, "check_back" | "eta" | "job") && !skip_title {
            params.get(*key).cloned()
        } else {
            present(params, key)
        };
        let Some(mut value) = value else { continue };
        if CLEARABLE.contains(key) && value.as_str().is_some_and(|s| s.trim().is_empty()) {
            value = Value::Null;
        }
        if *key == "blocked_by" {
            value = json!(int_list(&value, "blocked_by")?);
        }
        if *key == "links" {
            if let Value::String(s) = &value {
                value = json!([s]);
            }
        }
        fields.insert((*key).into(), value);
    }
    if let Some(holder) = present(params, "holder") {
        fields.insert("holders".into(), json!(holders_arg(ctx, &holder)?));
    }
    Ok(fields)
}

fn compact(item: &Value) -> Value {
    json!({
        "n": item["n"],
        "title": item["title"],
        "status": item["status"],
        "holders": item["holders"].as_array().map(|hs| hs.iter().map(holder_label).collect::<Vec<_>>()),
        "note": item["note"],
        "blocked_by": item["blocked_by"],
        "blocked_on": item["blocked_on"],
        "eta_at": item["eta_at"],
        "job": item["job"],
    })
}

fn holder_label(h: &Value) -> Value {
    h["name"].as_str().map(|n| json!(n)).unwrap_or_else(|| h["pid"].clone())
}

fn write_reply(board_slug: &str, reply: &Value, items_key: &str) -> Value {
    let mut out = json!({"board": board_slug});
    if reply["replayed"] == true {
        out["replayed"] = json!(true);
    }
    if let Some(items) = reply[items_key].as_array() {
        out["items"] = json!(items.iter().map(compact).collect::<Vec<_>>());
    }
    if reply["unblocked"].as_array().is_some_and(|u| !u.is_empty()) {
        out["unblocked"] = reply["unblocked"].clone();
    }
    if reply["warnings"].as_array().is_some_and(|w| !w.is_empty()) {
        out["warnings"] = reply["warnings"].clone();
    }
    // Similar open items for a new one (doc/org-model.md §7.4): a hint.
    if reply.get("possible_duplicate").is_some_and(|d| !d.is_null()) {
        out["possible_duplicate"] = reply["possible_duplicate"].clone();
    }
    out
}

fn item_create(ctx: &Ctx, params: &Value) -> RpcResult {
    let titles: Vec<String> = match params.get("title") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| v.as_str().map(str::to_string).ok_or_else(|| bad("title entries must be strings")))
            .collect::<RpcResult<_>>()?,
        _ => return Err(bad("title is required (a string or a list of strings)")),
    };
    if titles.is_empty() {
        return Err(bad("title must not be empty"));
    }
    let fields = item_fields(ctx, params, true)?;
    let board = resolve_board(ctx, params)?;
    authorize_edit(ctx, params, &board)?;
    let slug = slug(&board)?;
    let specs: Vec<Value> = titles
        .into_iter()
        .map(|title| {
            let mut spec = fields.clone();
            spec.insert("title".into(), json!(title));
            Value::Object(spec)
        })
        .collect();
    let mut body = json!({"actor": ctx.actor(), "items": specs});
    if let Some(request_id) = params.get("request_id").and_then(Value::as_str).filter(|r| !r.trim().is_empty()) {
        body["request_id"] = json!(request_id.trim());
    }
    let reply = ctx.api.post(&format!("/boards/{slug}/items"), &body)?;
    Ok(write_reply(&slug, &reply, "items"))
}

fn item_set(ctx: &Ctx, params: &Value) -> RpcResult {
    let ns = int_list(params.get("n").unwrap_or(&Value::Null), "n")?;
    if ns.is_empty() {
        return Err(bad("n is required"));
    }
    let fields = item_fields(ctx, params, false)?;
    let mut body = json!({"actor": ctx.actor(), "ns": ns, "set": fields});
    if let Some(add) = present(params, "add_holder") {
        body["add_holders"] = json!(holders_arg(ctx, &add)?);
    }
    // One atomic ownership transfer; the role for added holders (§7.2).
    if let Some(owner) = present(params, "owner") {
        let mut owners = holders_arg(ctx, &owner)?;
        if owners.len() != 1 {
            return Err(bad("owner names exactly one holder"));
        }
        body["owner"] = owners.remove(0);
    }
    if let Some(role) = params.get("role").and_then(Value::as_str).filter(|r| !r.trim().is_empty()) {
        body["role"] = json!(role.trim());
    }
    if let Some(remove) = present(params, "remove_holder") {
        let pids: Vec<Value> = holders_arg(ctx, &remove)?.into_iter().map(|h| h["pid"].clone()).collect();
        body["remove_holders"] = json!(pids);
    }
    if let Some(reason) = present(params, "reason") {
        body["reason"] = reason;
    }
    // Bulk edits: each item on its own, with a result per item.
    if params["each"] == true {
        body["each"] = json!(true);
    }
    // A done item reopens only on purpose; expect_status is the
    // compare-and-set, checked by the API under the board-row lock.
    if params["reopen"] == true {
        body["reopen"] = json!(true);
    }
    if let Some(expect) = params.get("expect_status").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
        body["expect_status"] = json!(expect.trim());
    }
    if body["set"].as_object().is_some_and(Map::is_empty)
        && body.get("add_holders").is_none()
        && body.get("remove_holders").is_none()
        && body.get("owner").is_none()
    {
        return Err(bad("nothing to change: pass status, note, holder or another field"));
    }
    let board = resolve_board(ctx, params)?;
    authorize_edit(ctx, params, &board)?;
    let slug = slug(&board)?;
    let reply = ctx.api.patch(&format!("/boards/{slug}/items"), &body)?;
    let mut out = write_reply(&slug, &reply, "items");
    if let Some(results) = reply["results"].as_array() {
        out["results"] = json!(results);
    }
    Ok(out)
}

fn normalize_engine(engine: &str) -> RpcResult<&'static str> {
    match engine.trim().to_lowercase().as_str() {
        "codex" => Ok("codex"),
        "claude" | "claude-code" => Ok("claude-code"),
        other => Err(bad(format!("engine must be codex or claude, not {other}"))),
    }
}

fn item_resolve(state: &Arc<Mutex<DaemonState>>, ctx: &Ctx, params: &Value) -> RpcResult {
    let n = params.get("n").and_then(Value::as_i64).ok_or_else(|| bad("n (an item number) is required"))?;
    let action = params.get("action").and_then(Value::as_str).ok_or_else(|| bad("action is required"))?;
    let board = resolve_board(ctx, params)?;
    authorize_edit(ctx, params, &board)?;
    let slug = slug(&board)?;
    let mut body = json!({"actor": ctx.actor(), "action": action});
    for key in ["kind", "blocked_on", "check_back", "reason", "message", "to"] {
        if let Some(v) = present(params, key) {
            body[key] = v;
        }
    }
    if let Some(v) = present(params, "blocked_by") {
        body["blocked_by"] = json!(int_list(&v, "blocked_by")?);
    }
    if let Some(v) = present(params, "holder") {
        body["holders"] = json!(holders_arg(ctx, &v)?);
    }
    let mut launched = Value::Null;
    if action == "launch" {
        launched = launch(state, ctx, params, &slug, n)?;
        body["action"] = json!("reassign");
        body["holders"] = json!([ctx.holder(launched["pid"].as_str().unwrap_or_default(), Some(&format!("item-{n}")))]);
    }
    let reply = ctx.api.post(&format!("/boards/{slug}/items/{n}/resolve"), &body).map_err(|(code, message)| {
        if launched.is_null() {
            (code, message)
        } else {
            // The worker exists; say so, and let a retry with the same
            // request_id reassign instead of spawning again.
            (code, format!(
                "{message} — the launched session {} is running but is not yet the holder; retry \
                 with the same request_id, or item_resolve({n}, \"reassign\", holder=\"{}\")",
                launched["uid"].as_str().unwrap_or("?"),
                launched["uid"].as_str().unwrap_or("?"),
            ))
        }
    })?;
    let mut out = json!({
        "board": slug,
        "item": compact(&reply["item"]),
        "flag_resolved": reply["flags_resolved"],
    });
    if !launched.is_null() {
        out["launched"] = launched;
    }
    if reply["warnings"].as_array().is_some_and(|w| !w.is_empty()) {
        out["warnings"] = reply["warnings"].clone();
    }
    Ok(out)
}

/// Sessions spawned by `launch`, keyed by (caller pid, board, item, request_id), so a
/// retried launch whose final reassign failed reassigns instead of spawning
/// a second worker. In memory: a brain restart forgets it, and the error
/// message names the uid for a manual reassign.
type LaunchKey = (String, String, i64, String);

fn launches() -> &'static Mutex<std::collections::VecDeque<(LaunchKey, Value)>> {
    static LAUNCHES: std::sync::OnceLock<Mutex<std::collections::VecDeque<(LaunchKey, Value)>>> =
        std::sync::OnceLock::new();
    LAUNCHES.get_or_init(Mutex::default)
}
const MAX_REMEMBERED_LAUNCHES: usize = 256;

fn launch(state: &Arc<Mutex<DaemonState>>, ctx: &Ctx, params: &Value, slug: &str, n: i64) -> RpcResult {
    let Some(uid) = ctx.uid.as_deref() else {
        return Err(bad("launch needs a session caller; Owner launches from the TUI"));
    };
    if !ctx.daemon_owned {
        return Err(bad(
            "launch spawns from a daemon-owned session, and this one is TUI-owned: start the \
             worker with start_session, then item_resolve(n, \"reassign\", holder=<its uid>)",
        ));
    }
    let request_id = params.get("request_id").and_then(Value::as_str).map(str::trim).filter(|r| !r.is_empty());
    let key = request_id.map(|r| (ctx.pid.clone(), slug.to_string(), n, r.to_string()));
    if let Some(key) = &key {
        let remembered = launches().lock().unwrap_or_else(|p| p.into_inner());
        if let Some((_, prior)) = remembered.iter().find(|(k, _)| k == key) {
            let mut prior = prior.clone();
            prior["replayed"] = json!(true);
            return Ok(prior);
        }
    }
    let engine = match params.get("engine").and_then(Value::as_str) {
        Some(e) if !e.trim().is_empty() => normalize_engine(e)?,
        _ => match ctx.engine.as_deref() {
            Some("codex") => "codex",
            Some("claude-code") | Some("claude") => "claude-code",
            _ => return Err(bad("pass engine=codex or engine=claude (the caller is not an agent session)")),
        },
    };
    let read = ctx.api.get(&format!("/boards/{slug}"), &[])?;
    let item = read["items"]
        .as_array()
        .and_then(|items| items.iter().find(|i| i["n"] == n))
        .ok_or_else(|| (ErrorCode::NotFound, format!("no open item #{n} on board {slug}")))?;
    let title = item["title"].as_str().unwrap_or_default();
    let mut prompt = format!("item #{n} ({slug}): {title}");
    if let Some(message) = params.get("message").and_then(Value::as_str).filter(|m| !m.trim().is_empty()) {
        prompt.push_str("\n\n");
        prompt.push_str(message);
    }
    prompt.push_str(&format!(
        "\n\nYou hold this item. Keep it current with item_set({n}, ..., board=\"{slug}\"), \
         mark it waiting with an eta for long jobs, and close it with item_set({n}, \"done\", board=\"{slug}\")."
    ));
    // Without task_id the worker joins the caller's task and checkout, as
    // start_session does; a task_id gives it that task's own worktree.
    let mut spawn_params = json!({"type": engine, "label": format!("item-{n}"), "prompt": prompt});
    if let Some(task_id) = params.get("task_id").and_then(Value::as_str).filter(|t| !t.trim().is_empty()) {
        spawn_params["task_id"] = json!(task_id.trim());
    }
    let spawn = crate::control::methods::mcp_start_session(state, &spawn_params, Some(uid))?;
    let new_uid = spawn["uid"]
        .as_str()
        .or_else(|| spawn["session_uid"].as_str())
        .ok_or_else(|| (ErrorCode::Internal, format!("launch: spawn returned no uid: {spawn}")))?
        .to_string();
    let mut launched = json!({
        "uid": new_uid,
        "engine": engine,
        "pid": format!("agent:{}:{new_uid}", ctx.daemon_id),
    });
    for key in ["worktree_path", "task_id", "shared_workspace", "workspace_shared_with", "warning", "worktree_warning", "worktree_recreated"] {
        if let Some(v) = spawn.get(key).filter(|v| !v.is_null()) {
            launched[key] = v.clone();
        }
    }
    if let Some(key) = key {
        let mut remembered = launches().lock().unwrap_or_else(|p| p.into_inner());
        remembered.push_back((key, launched.clone()));
        while remembered.len() > MAX_REMEMBERED_LAUNCHES {
            remembered.pop_front();
        }
    }
    Ok(launched)
}

// ---- board() --------------------------------------------------------------

pub(crate) fn short_duration(seconds: i64) -> String {
    let s = seconds.max(0);
    match s {
        0..=89 => format!("{s}s"),
        90..=5399 => format!("{}m", (s + 30) / 60),
        5400..=172_799 => {
            let (h, m) = (s / 3600, (s % 3600) / 60);
            if m == 0 { format!("{h}h") } else { format!("{h}h{m}m") }
        }
        _ => format!("{}d", s / 86_400),
    }
}

fn parse_ts(value: &Value) -> Option<DateTime<Utc>> {
    value.as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|d| d.with_timezone(&Utc))
}

fn holder_slim(h: &Value) -> String {
    let name = h["name"].as_str().or_else(|| h["pid"].as_str()).unwrap_or("?");
    if h["kind"] == "shell" {
        return format!("@{name}[shell]");
    }
    let st = &h["state"];
    let state = st["state"].as_str().unwrap_or("unknown");
    // A role other than owner shows: `@ep(reviewer)[idle 3m]` (§7.2).
    let role = h["role"].as_str().filter(|r| !matches!(*r, "owner" | "holder"))
        .map(|r| format!("({r})")).unwrap_or_default();
    let mut out = format!("@{name}{role}[{state}");
    if let Some(for_s) = st["for_s"].as_i64() {
        out.push_str(&format!(" {}", short_duration(for_s)));
    }
    if st["reported_done"] == true {
        out.push_str(", reported done");
    }
    out.push(']');
    out
}

/// One line per item: `#14 active "fuse SEJD" [RL] @lane[idle 24m] ⚑holder_idle · note`.
pub(crate) fn slim_item(item: &Value, now: DateTime<Utc>) -> String {
    let mut line = format!(
        "#{} {} \"{}\"",
        item["n"],
        item["status"].as_str().unwrap_or("?"),
        item["title"].as_str().unwrap_or("")
    );
    if let Some(group) = item["group"].as_str() {
        line.push_str(&format!(" [{group}]"));
    }
    match item["holders"].as_array() {
        Some(hs) if !hs.is_empty() => {
            for h in hs {
                line.push(' ');
                line.push_str(&holder_slim(h));
            }
        }
        _ => line.push_str(" (no holder)"),
    }
    if let Some(blockers) = item["blocked_by"].as_array().filter(|b| !b.is_empty()) {
        let list: Vec<String> = blockers.iter().map(|b| format!("#{b}")).collect();
        line.push_str(&format!(" ⊸{}", list.join(",")));
    }
    if let Some(on) = item["blocked_on"].as_str() {
        line.push_str(&format!(" on \"{on}\""));
    }
    // The job beside the holder's session state, never merged: an idle
    // session with a job in flight is both (doc/strict-boards.md §2.3).
    if let Some(job) = item["job"].as_str() {
        line.push_str(&format!(" job: {job}"));
    }
    if item["status"] == "waiting" || item["status"] == "active" {
        if let Some(eta) = parse_ts(&item["eta_at"]) {
            let left = (eta - now).num_seconds();
            let eta = if left >= 0 {
                format!("eta {}", short_duration(left))
            } else {
                format!("eta passed {} ago", short_duration(-left))
            };
            if item["job"].is_string() {
                line.push_str(&format!(" ({eta})"));
            } else {
                line.push_str(&format!(" {eta}"));
            }
        }
    }
    if let Some(gates) = item["gate"].as_object() {
        for (name, g) in gates {
            line.push_str(&super::gate::slim_cell(name, g));
        }
    }
    if let Some(flags) = item["flags"].as_array() {
        for f in flags {
            line.push_str(&format!(" ⚑{}", f.as_str().unwrap_or("?")));
        }
    }
    if let Some(note) = item["note"].as_str() {
        line.push_str(&format!(" · {note}"));
    }
    line
}

fn slim_closed(item: &Value, now: DateTime<Utc>) -> String {
    let ago = parse_ts(&item["closed_at"])
        .map(|t| format!(" {} ago", short_duration((now - t).num_seconds())))
        .unwrap_or_default();
    let mut line = format!(
        "#{} {} \"{}\"{ago}",
        item["n"],
        item["status"].as_str().unwrap_or("?"),
        item["title"].as_str().unwrap_or("")
    );
    if let Some(note) = item["note"].as_str() {
        line.push_str(&format!(" · {note}"));
    }
    line
}

fn board_header(header: &Value) -> Value {
    let orchestrator = &header["orchestrator"];
    json!({
        "slug": header["slug"],
        "name": header["name"],
        "version": header["version"],
        "orchestrator": if orchestrator.is_null() {
            json!("none")
        } else {
            json!(format!(
                "{} [{}]{}",
                orchestrator["name"].as_str().or_else(|| orchestrator["pid"].as_str()).unwrap_or("?"),
                orchestrator["state"].as_str().unwrap_or("unknown"),
                if orchestrator["explicit"] == true { " (set on the board)" } else { "" },
            ))
        },
        "health": header["health"],
        "backlog_allowed": header["settings"]["backlog_allowed"],
    })
}

/// Session names, `rev1 (for ep)` for a member's assistant (O5).
fn names(rows: &Value) -> Vec<Value> {
    rows.as_array()
        .map(|v| {
            v.iter()
                .map(|s| {
                    let name = s["name"].as_str().map(str::to_string)
                        .unwrap_or_else(|| s["pid"].as_str().unwrap_or("?").to_string());
                    match s["assistant_of"].as_str() {
                        Some(member) => json!(format!("{name} (for {member})")),
                        None => json!(name),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// [`launch_line`] at the current time (for the TUI's Alt+B banner).
pub fn launch_line_now(launch: &Value) -> Option<String> {
    launch_line(launch, Utc::now())
}

/// `2 unlaunched (#1496 18m, #1501 4m) · free: Batch-Valid, P1-Ingest`, or
/// None when nothing is unlaunched and nobody is free.
pub fn launch_line(launch: &Value, now: DateTime<Utc>) -> Option<String> {
    let unlaunched = launch["unlaunched"].as_array().cloned().unwrap_or_default();
    let free: Vec<&str> = launch["free"].as_array().map(|v| v.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    if unlaunched.is_empty() {
        return None;
    }
    let list: Vec<String> = unlaunched
        .iter()
        .map(|u| match parse_ts(&u["since"]) {
            Some(t) => format!("#{} {}", u["n"], short_duration((now - t).num_seconds())),
            None => format!("#{}", u["n"]),
        })
        .collect();
    let free = if free.is_empty() { "nobody".to_string() } else { free.join(", ") };
    Some(format!("{} unlaunched ({}) · free: {free}", unlaunched.len(), list.join(", ")))
}

/// Shape a `GET /boards/{ref}` reply for `board()`; pure, for tests.
pub(crate) fn shape_board(read: &Value, pid: &str, params: &Value, now: DateTime<Utc>) -> Value {
    let mine = params["mine"] == true;
    let group = params["group"].as_str().map(str::to_lowercase);
    let full = params["view"] == "full";
    let keep = |item: &&Value| {
        (!mine || item["holders"].as_array().is_some_and(|hs| hs.iter().any(|h| h["pid"] == pid)))
            && group.as_ref().is_none_or(|g| item["group"].as_str().is_some_and(|x| x.to_lowercase() == *g))
    };
    let mut items: Vec<&Value> = read["items"].as_array().map(|v| v.iter().filter(keep).collect()).unwrap_or_default();
    // Flagged first (the API already orders by group, then number).
    items.sort_by_key(|i| i["flags"].as_array().is_none_or(Vec::is_empty));
    let kept: std::collections::BTreeSet<i64> = items.iter().filter_map(|i| i["n"].as_i64()).collect();
    let flags: Vec<&Value> = read["flags"]
        .as_array()
        .map(|v| v.iter().filter(|f| f["n"].as_i64().is_some_and(|n| kept.contains(&n))).collect())
        .unwrap_or_default();
    let closed: Vec<&Value> = if params["include_closed"] == false {
        vec![]
    } else {
        read["recently_closed"].as_array().map(|v| v.iter().filter(keep).collect()).unwrap_or_default()
    };
    let mut out = json!({"board": board_header(&read["board"]), "free_capacity": names(&read["free_capacity"])});
    // Headings and epics (doc/org-model.md §7.3), for the Alt+B tree.
    if let Some(groups) = read["groups"].as_array().filter(|g| !g.is_empty()) {
        out["groups"] = json!(groups);
    }
    // Working sessions on the board that hold no item: work it can't see.
    if read["untracked"].as_array().is_some_and(|u| !u.is_empty()) {
        out["untracked"] = json!(names(&read["untracked"]));
    }
    if full {
        if read["launch"].is_object() {
            out["launch"] = read["launch"].clone();
        }
    } else if let Some(line) = launch_line(&read["launch"], now) {
        out["launch"] = json!(line);
    }
    if full {
        out["flags"] = json!(flags);
        out["items"] = json!(items);
        out["recently_closed"] = json!(closed);
    } else {
        out["flags"] = json!(flags
            .iter()
            .map(|f| format!(
                "#{} {} {}{}",
                f["n"],
                f["kind"].as_str().unwrap_or("?"),
                short_duration(f["age_s"].as_i64().unwrap_or(0)),
                if f["detail"].is_null() { String::new() } else { format!(" {}", f["detail"]) }
            ))
            .collect::<Vec<_>>());
        out["items"] = json!(items.iter().map(|i| slim_item(i, now)).collect::<Vec<_>>());
        out["recently_closed"] = json!(closed.iter().map(|i| slim_closed(i, now)).collect::<Vec<_>>());
    }
    if let Some(archived) = read["archived"].as_array() {
        out["archived"] = if full {
            json!(archived)
        } else {
            json!(archived.iter().map(|i| slim_closed(i, now)).collect::<Vec<_>>())
        };
    }
    out
}

/// The settings `board.settings` forwards; anything else is refused here
/// rather than passed to `PATCH /boards`. Gate profile settings have their
/// own tool (`gate.profile`). The API still decides who may change the
/// governed ones (orchestrator_pid, steward_pid, backlog_allowed, the caps).
const BOARD_SETTING_KEYS: &[&str] = &[
    "name", "idle_s", "stale_s", "unassigned_s", "repush_s", "escalate_s", "digest_s",
    "stale_background_s", "nudge_grace_s", "holder_idle_enabled", "orchestrator_pid", "steward_pid",
    "channel", "backlog_allowed", "launch_s", "max_active_per_holder", "check_back_max_s",
];

/// `board(settings={...})`: change a board's settings (board members only;
/// the API further leaves backlog_allowed and steward_pid to Owner and the
/// orchestrator) and return the new header. A write, so not `board.read`,
/// which the restart barrier treats as read-only.
fn board_settings(ctx: &Ctx, params: &Value) -> RpcResult {
    let Some(settings) = params.get("settings").and_then(Value::as_object).filter(|s| !s.is_empty()) else {
        return Err(bad("settings must be a non-empty object, e.g. {\"backlog_allowed\": false}"));
    };
    let board = resolve_board(ctx, params)?;
    authorize_edit(ctx, params, &board)?;
    let slug = slug(&board)?;
    let mut body = json!({"actor": ctx.actor()});
    for (k, v) in settings {
        if !BOARD_SETTING_KEYS.contains(&k.as_str()) {
            return Err(bad(format!(
                "unknown board setting {k}; settings are {}",
                BOARD_SETTING_KEYS.join(", ")
            )));
        }
        body[k] = v.clone();
    }
    let header = ctx.api.patch(&format!("/boards/{slug}"), &body)?;
    Ok(json!({"board": slug, "settings": header["settings"]}))
}

/// `place(task, board, group=…, holder=…)` (doc/org-model.md §7.1). Editing
/// the target board needs the usual board rights; the API enforces what
/// placement itself refuses.
/// Placing or unplacing a task writes that task (its status, its owner's
/// launch grant), so it takes the same scope as any task write: the
/// caller's own task or a descendant, unless Owner or global (review of
/// 6522623: placing someone else's task, then owning it, must not widen the
/// caller's launch scope).
fn require_task_scope(state: &Arc<Mutex<DaemonState>>, ctx: &Ctx, task: &str) -> RpcResult<()> {
    if ctx.owner || ctx.global {
        return Ok(());
    }
    let own = ctx.task_id.as_deref().ok_or_else(|| {
        (ErrorCode::Unauthorized, format!("a taskless session cannot place or unplace task {task}"))
    })?;
    let s = state.lock().unwrap_or_else(|p| p.into_inner());
    if crate::control::auth::task_in_scope(&s, task, own) {
        Ok(())
    } else {
        Err((ErrorCode::Unauthorized, format!("task {task} is not the caller's task or a descendant")))
    }
}

fn item_place(state: &Arc<Mutex<DaemonState>>, ctx: &Ctx, params: &Value) -> RpcResult {
    let task = params.get("task_id").or_else(|| params.get("task")).and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| bad("task_id is required"))?;
    require_task_scope(state, ctx, task.trim())?;
    let board = resolve_board(ctx, params)?;
    authorize_edit(ctx, params, &board)?;
    let slug = slug(&board)?;
    let mut body = json!({"actor": ctx.actor(), "board": slug});
    for key in ["group", "group_id", "role"] {
        if let Some(v) = params.get(key).and_then(Value::as_str).filter(|v| !v.trim().is_empty()) {
            body[key] = json!(v.trim());
        }
    }
    if let Some(holder) = present(params, "holder") {
        let mut hs = holders_arg(ctx, &holder)?;
        if hs.len() > 1 {
            return Err(bad("place takes one holder; add others with item_set(add_holder=...)"));
        }
        if let Some(h) = hs.pop() {
            body["holder"] = h;
        }
    }
    if params["replace"] == true {
        body["replace"] = json!(true);
    }
    ctx.api.post(&format!("/tasks/{}/place", task.trim()), &body)
}

fn item_unplace(state: &Arc<Mutex<DaemonState>>, ctx: &Ctx, params: &Value) -> RpcResult {
    let task = params.get("task_id").or_else(|| params.get("task")).and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| bad("task_id is required"))?;
    require_task_scope(state, ctx, task.trim())?;
    // Unplacing edits the link's board: the usual board rights there.
    let row = ctx.api.get(&format!("/tasks/{}", task.trim()), &[])?;
    let board_ref = row["placement"]["board"].as_str()
        .ok_or_else(|| bad(format!("task {} is not on a board", task.trim())))?;
    let board = resolve_board(ctx, &json!({"board": board_ref}))?;
    authorize_edit(ctx, &json!({"board": board_ref}), &board)?;
    let mut body = json!({"actor": ctx.actor()});
    if let Some(reason) = params.get("reason").and_then(Value::as_str) {
        body["reason"] = json!(reason);
    }
    ctx.api.post(&format!("/tasks/{}/unplace", task.trim()), &body)
}

const GROUP_FIELDS: &[&str] = &["name", "kind", "parent_group_id", "description", "goal", "ord"];

fn group_create(ctx: &Ctx, params: &Value) -> RpcResult {
    let board = resolve_board(ctx, params)?;
    authorize_edit(ctx, params, &board)?;
    let slug = slug(&board)?;
    let mut body = json!({"actor": ctx.actor()});
    for key in GROUP_FIELDS {
        if let Some(v) = present(params, key) {
            body[*key] = v;
        }
    }
    ctx.api.post(&format!("/boards/{slug}/groups"), &body)
}

fn group_set(ctx: &Ctx, params: &Value) -> RpcResult {
    let id = params.get("group_id").and_then(Value::as_str).filter(|g| !g.trim().is_empty())
        .ok_or_else(|| bad("group_id is required"))?;
    let board = resolve_board(ctx, params)?;
    authorize_edit(ctx, params, &board)?;
    let slug = slug(&board)?;
    let mut body = json!({"actor": ctx.actor()});
    for key in GROUP_FIELDS {
        if let Some(v) = present(params, key) {
            body[*key] = v;
        }
    }
    if let Some(rev) = params.get("expected_revision").and_then(Value::as_i64) {
        body["expected_revision"] = json!(rev);
    }
    ctx.api.patch(&format!("/boards/{slug}/groups/{}", id.trim()), &body)
}

/// The epic writes (doc/org-model.md §7.3, §7.6). The API decides who may:
/// Owner or the board's lead for a PoC and a close; the org core checks a
/// staffing move's manager chain.
/// `board.join` (#28): add a session to a board the caller may edit (the
/// creator setting up a session it spawned, or anyone with editing rights
/// bringing a session in). `session` is a chat name, uid or participant id;
/// `group` / `group_id` names the group (an epic) it joins for.
fn board_join(ctx: &Ctx, params: &Value) -> RpcResult {
    let board = resolve_board(ctx, params)?;
    authorize_edit(ctx, params, &board)?;
    let reference = params.get("session").and_then(Value::as_str).filter(|s| !s.trim().is_empty())
        .ok_or_else(|| bad("session (a chat name, uid or participant id) is required"))?;
    let target = resolve_holder(ctx, reference)?;
    let mut body = json!({"actor": ctx.actor(), "pid": target["pid"], "name": target["name"]});
    for key in ["group", "group_id", "reason"] {
        if let Some(v) = present(params, key) {
            body[key] = v;
        }
    }
    ctx.api.post(&format!("/boards/{}/members", slug(&board)?), &body)
}

/// `board.leave` (the caller leaves) and `board.remove_member` (#28): end
/// an explicit join. The API decides who may remove whom; the daemon tells it
/// whether the caller can still edit the board (the adder's case).
fn board_leave(ctx: &Ctx, params: &Value, target: Option<&str>) -> RpcResult {
    let board = resolve_board(ctx, params)?;
    let pid = match target {
        Some(reference) => resolve_holder(ctx, reference)?["pid"].as_str().unwrap_or_default().to_string(),
        None => ctx.pid.clone(),
    };
    let editor = target.is_some() && authorize_edit(ctx, params, &board).is_ok();
    let mut body = json!({"actor": ctx.actor(), "pid": pid, "editor": editor});
    if let Some(reason) = present(params, "reason") {
        body["reason"] = reason;
    }
    ctx.api.post(&format!("/boards/{}/members/remove", slug(&board)?), &body)
}

fn epic_call(ctx: &Ctx, method: &str, params: &Value) -> RpcResult {
    let board = resolve_board(ctx, params)?;
    let slug = slug(&board)?;
    let mut body = json!({"actor": ctx.actor()});
    let group = || {
        params.get("group_id").and_then(Value::as_str).filter(|g| !g.trim().is_empty())
            .map(|g| g.trim().to_string())
            .ok_or_else(|| bad("group_id (the epic) is required"))
    };
    let path = match method {
        "epic.set_poc" | "epic.staff" => {
            // `session=` names the PoC by its session (a chat name, uid or
            // participant id); the API takes the member whose main it is.
            let session = params.get("session").and_then(Value::as_str).filter(|m| !m.trim().is_empty());
            let member = match session {
                Some(reference) => resolve_holder(ctx, reference)?["pid"].as_str().unwrap_or_default().to_string(),
                None => params.get("member").and_then(Value::as_str).filter(|m| !m.trim().is_empty())
                    .ok_or_else(|| bad("member (a member id, handle or owner) or session is required"))?
                    .trim().to_string(),
            };
            body["member"] = json!(member);
            for key in ["team_id", "as", "reason", "request_id"] {
                if let Some(v) = params.get(key).filter(|v| !v.is_null()) {
                    body[key] = v.clone();
                }
            }
            let tail = if method == "epic.set_poc" { "poc" } else { "staff" };
            format!("/boards/{slug}/epics/{}/{tail}", group()?)
        }
        "epic.close_plan" => {
            body["outcome"] = params.get("outcome").cloned()
                .ok_or_else(|| bad("outcome (done or cancelled) is required"))?;
            format!("/boards/{slug}/epics/{}/close-plan", group()?)
        }
        _ => {
            let plan = params.get("plan_id").and_then(Value::as_i64)
                .ok_or_else(|| bad("plan_id is required"))?;
            body["revision"] = params.get("revision").cloned()
                .ok_or_else(|| bad("revision (from epic.close_plan) is required"))?;
            body["choices"] = params.get("choices").cloned().unwrap_or_else(|| json!({}));
            if let Some(r) = params.get("request_id").filter(|v| !v.is_null()) {
                body["request_id"] = r.clone();
            }
            format!("/boards/{slug}/close-plans/{plan}/apply")
        }
    };
    ctx.api.post(&path, &body)
}

fn board_read(ctx: &Ctx, params: &Value) -> RpcResult {
    let board = resolve_board(ctx, params)?;
    let slug = slug(&board)?;
    let mut query: Vec<(&str, String)> = vec![];
    if params["archived"] == true {
        query.push(("archived", "true".into()));
    }
    if let Some(q) = params["query"].as_str().filter(|q| !q.trim().is_empty()) {
        query.push(("q", q.trim().into()));
    }
    if params["view"] == "full" {
        query.push(("history", "5".into()));
    }
    if let Some(v) = params["since_version"].as_i64() {
        query.push(("since_version", v.to_string()));
    }
    let read = ctx.api.get(&format!("/boards/{slug}"), &query)?;
    if read["unchanged"] == true {
        return Ok(read);
    }
    Ok(shape_board(&read, &ctx.pid, params, Utc::now()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    fn ctx_with(people: Vec<Value>) -> Ctx {
        Ctx {
            s4: Default::default(), s4_wire: false,
            pid: "agent:d1:me-uid".into(),
            name: "lane".into(),
            uid: Some("me-uid".into()),
            task_id: Some("t-1".into()),
            owner: false,
            global: false,
            engine: Some("codex".into()),
            daemon_owned: true,
            daemon_id: "d1".into(),
            people,
            api: Api::from_config("http://127.0.0.1:9", "tok").unwrap(),
        }
    }

    #[test]
    fn s4_actor_and_target_are_independent_and_old_api_shape_is_unchanged() {
        let mut ctx = ctx_with(people());
        let actor = json!({"member_id":"mem_a", "acting_as":"main", "provenance":"central-open-main", "org_version":7});
        let target = json!({"member_id":"mem_b", "acting_as":"assistant", "provenance":"o5-spawn-stamp", "verified":false});
        ctx.s4.insert("me-uid".into(), actor.clone());
        ctx.s4.insert("old-uid".into(), target.clone());
        assert!(ctx.actor().get("attribution").is_none());
        assert!(ctx.holder("agent:d1:old-uid", None).get("attribution").is_none());
        ctx.s4_wire = true;
        assert_eq!(ctx.actor()["attribution"], actor);
        assert_eq!(ctx.holder("agent:d1:old-uid", None)["attribution"], target);
        assert_eq!(ctx.holder("agent:d2:old-uid", None)["attribution"]["provenance"], "unknown");
    }

    fn people() -> Vec<Value> {
        vec![
            json!({"id":"agent:d1:me-uid","name":"lane","session_uid":"me-uid","present":true,"kind":"agent"}),
            json!({"id":"agent:d2:rl-uid","name":"rl-scale-out","session_uid":"rl-uid","present":true,"kind":"agent","aliases":["rl"]}),
            json!({"id":"agent:d1:old-uid","name":"scout","session_uid":"old-uid","present":false,"kind":"agent","released":true}),
            json!({"id":"agent:d2:new-uid","name":"scout","session_uid":"new-uid","present":true,"kind":"agent"}),
            json!({"id":"agent:d1:x-uid","name":"twin","session_uid":"x-uid","kind":"agent"}),
            json!({"id":"agent:d2:y-uid","name":"Twin","session_uid":"y-uid","kind":"agent"}),
            json!({"id":"owner","name":"Owner","session_uid":"","kind":"owner"}),
        ]
    }

    #[test]
    fn shell_holders_refuse_names_aliases_uids_and_participant_ids() {
        let mut ctx = ctx_with(vec![]);
        ctx.people.push(json!({"id":"agent:d1:sh", "session_uid":"sh", "name":"terminal", "aliases":["old-shell"], "kind":"shell", "engine":"bash"}));
        for reference in ["terminal", "old-shell", "sh", "agent:d1:sh"] {
            let e = resolve_holder(&ctx, reference).unwrap_err();
            assert!(e.1.contains("bash is a shell"), "{reference}: {e:?}");
        }
        ctx.engine = Some("bash".into());
        assert!(resolve_holder(&ctx, "me").is_err());
        assert_eq!(holder_slim(&json!({"name":"terminal", "kind":"shell", "state":{"state":"idle"}})), "@terminal[shell]");
    }

    #[test]
    fn holders_resolve_by_name_alias_uid_and_pid() {
        let ctx = ctx_with(people());
        let h = resolve_holder(&ctx, "@RL-Scale-Out").unwrap();
        assert_eq!(h, json!({"pid":"agent:d2:rl-uid","name":"rl-scale-out","session_uid":"rl-uid","daemon_id":"d2"}));
        assert_eq!(resolve_holder(&ctx, "rl").unwrap()["pid"], "agent:d2:rl-uid");
        assert_eq!(resolve_holder(&ctx, "rl-uid").unwrap()["pid"], "agent:d2:rl-uid");
        assert_eq!(resolve_holder(&ctx, "me").unwrap()["pid"], "agent:d1:me-uid");
        let foreign = resolve_holder(&ctx, "agent:d9:zz").unwrap();
        assert_eq!((foreign["daemon_id"].as_str(), foreign["session_uid"].as_str()), (Some("d9"), Some("zz")));
        // The released record loses to the current holder of the name.
        assert_eq!(resolve_holder(&ctx, "scout").unwrap()["pid"], "agent:d2:new-uid");
        let (code, msg) = resolve_holder(&ctx, "twin").unwrap_err();
        assert!(matches!(code, ErrorCode::InvalidParams));
        assert!(msg.contains("ambiguous") && msg.contains("agent:d1:x-uid") && msg.contains("agent:d2:y-uid"));
        assert!(matches!(resolve_holder(&ctx, "nobody").unwrap_err().0, ErrorCode::NotFound));
    }

    #[test]
    fn holder_lists_and_none() {
        let ctx = ctx_with(people());
        assert_eq!(holders_arg(&ctx, &json!("none")).unwrap(), Vec::<Value>::new());
        assert_eq!(holders_arg(&ctx, &json!(["me", "rl"])).unwrap().len(), 2);
        assert!(holders_arg(&ctx, &json!(["none", "rl"])).is_err());
    }

    #[test]
    fn fields_pass_through_and_blank_text_clears() {
        let ctx = ctx_with(people());
        let fields = item_fields(
            &ctx,
            &json!({"status":"blocked","note":"","blocked_by":14,"links":"abc123","holder":"rl","group":null}),
            false,
        )
        .unwrap();
        assert_eq!(Value::Object(fields), json!({
            "status":"blocked","note":null,"blocked_by":[14],"links":["abc123"],
            "holders":[{"pid":"agent:d2:rl-uid","name":"rl-scale-out","session_uid":"rl-uid","daemon_id":"d2"}]
        }));
    }

    #[test]
    fn durations_are_short() {
        assert_eq!(short_duration(45), "45s");
        assert_eq!(short_duration(24 * 60), "24m");
        assert_eq!(short_duration(2 * 3600 + 600), "2h10m");
        assert_eq!(short_duration(3 * 86_400), "3d");
    }

    fn sample_read() -> Value {
        json!({
            "board": {"slug":"sfd","name":"Swarm","version":12,
                      "orchestrator":{"pid":"agent:d1:o","name":"Swarm-Coord","state":"idle","explicit":false},
                      "health":{"unresolved":1,"oldest_s":600}},
            "flags": [{"n":14,"kind":"holder_idle","detail":null,"age_s":600}],
            "items": [
                {"n":3,"status":"waiting","title":"C2 run","group":"RL","eta_at":"2026-10-06T12:40:00Z",
                 "holders":[{"pid":"agent:d1:me-uid","name":"lane","state":{"state":"idle","for_s":60}}],
                 "blocked_by":[],"flags":[],"note":"full C2 run"},
                {"n":14,"status":"active","title":"fuse SEJD","group":"RL",
                 "holders":[{"pid":"agent:d2:rl-uid","name":"rl-scale-out","state":{"state":"idle","for_s":1440}}],
                 "blocked_by":[],"flags":["holder_idle"],"note":null},
                {"n":15,"status":"blocked","title":"bench","group":null,"holders":[],
                 "blocked_by":[14],"blocked_on":null,"flags":[],"note":null}
            ],
            "recently_closed": [{"n":9,"status":"done","title":"x","closed_at":"2026-10-06T10:00:00Z","note":"abc123","holders":[]}],
            "free_capacity": [{"pid":"agent:d1:f","name":"idle-lane","state":"idle"},
                              {"pid":"agent:d1:r","name":"rev1","state":"idle","assistant_of":"ep"}],
            "untracked": [{"pid":"agent:d1:u","name":"busy-lane","state":"working"}],
            "launch": {"unlaunched": [{"n":15,"title":"bench","since":"2026-10-06T12:02:00Z"}], "free": ["idle-lane"]}
        })
    }

    #[test]
    fn slim_board_puts_flags_first_and_reads_compactly() {
        let now = DateTime::parse_from_rfc3339("2026-10-06T12:20:00Z").unwrap().with_timezone(&Utc);
        let out = shape_board(&sample_read(), "agent:d1:me-uid", &json!({}), now);
        assert_eq!(out["board"]["orchestrator"], "Swarm-Coord [idle]");
        assert_eq!(out["flags"], json!(["#14 holder_idle 10m"]));
        assert_eq!(out["items"], json!([
            "#14 active \"fuse SEJD\" [RL] @rl-scale-out[idle 24m] ⚑holder_idle",
            "#3 waiting \"C2 run\" [RL] @lane[idle 60s] eta 20m · full C2 run",
            "#15 blocked \"bench\" (no holder) ⊸#14",
        ]));
        assert_eq!(out["recently_closed"], json!(["#9 done \"x\" 2h20m ago · abc123"]));
        assert_eq!(out["free_capacity"], json!(["idle-lane", "rev1 (for ep)"]));
        assert_eq!(out["untracked"], json!(["busy-lane"]));
        assert_eq!(out["launch"], "1 unlaunched (#15 18m) · free: idle-lane");
    }

    #[test]
    fn slim_item_shows_the_job_beside_the_holders_state() {
        let now = DateTime::parse_from_rfc3339("2026-10-06T12:20:00Z").unwrap().with_timezone(&Utc);
        let item = json!({"n":1216,"status":"active","title":"pins gate","eta_at":"2026-10-06T12:45:00Z",
            "job":"cm-gate pins L3","holders":[{"pid":"p","name":"release-4","state":{"state":"idle","for_s":720}}],
            "flags":[]});
        assert_eq!(slim_item(&item, now), "#1216 active \"pins gate\" @release-4[idle 12m] job: cm-gate pins L3 (eta 25m)");
        let roles = json!({"n":3,"status":"active","title":"t","flags":[],"holders":[
            {"pid":"a","name":"lane","role":"owner","state":{"state":"working"}},
            {"pid":"b","name":"ep","role":"reviewer","state":{"state":"idle","for_s":180}}]});
        assert_eq!(slim_item(&roles, now), "#3 active \"t\" @lane[working] @ep(reviewer)[idle 3m]");
        let no_job = json!({"n":2,"status":"active","title":"t","eta_at":"2026-10-06T12:00:00Z","holders":[],"flags":[]});
        assert_eq!(slim_item(&no_job, now), "#2 active \"t\" (no holder) eta passed 20m ago");
        assert_eq!(launch_line(&json!({"unlaunched":[],"free":["a"]}), now), None);
        assert_eq!(launch_line(&json!({"unlaunched":[{"n":3}],"free":[]}), now).unwrap(), "1 unlaunched (#3) · free: nobody");
    }

    #[test]
    fn board_filters_mine_group_and_closed() {
        let now = Utc::now();
        let out = shape_board(&sample_read(), "agent:d1:me-uid", &json!({"mine":true,"include_closed":false}), now);
        assert_eq!(out["items"].as_array().unwrap().len(), 1);
        assert_eq!(out["flags"], json!([]));
        assert_eq!(out["recently_closed"], json!([]));
        let out = shape_board(&sample_read(), "x", &json!({"group":"rl","view":"full"}), now);
        assert_eq!(out["items"].as_array().unwrap().len(), 2);
        assert_eq!(out["items"][0]["n"], 14);
    }

    // ---- against a stub planning API ----------------------------------------

    type Seen = Arc<Mutex<Vec<(String, String, Value)>>>;

    /// Serve requests with `reply(method, path) -> (status, body)` and record them.
    fn stub(reply: impl Fn(&str, &str, &Value) -> (u16, String) + Send + 'static) -> (String, Seen) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let seen: Seen = Arc::default();
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
                log.lock().unwrap().push((method.clone(), path.clone(), value));
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                );
            }
        });
        (url, seen)
    }

    fn state_with_session(root: &std::path::Path, api_url: &str) -> Arc<Mutex<DaemonState>> {
        let mut state = DaemonState::default();
        state.messaging_root = root.into();
        state.config.api_url = api_url.into();
        state.config.api_token = "tok".into();
        for (uid, task) in [("me-uid", "task-a"), ("rl-uid", "task-b")] {
            state.tui_sessions.insert(
                uid.into(),
                serde_json::from_value(json!({"uid":uid,"label":uid.trim_end_matches("-uid"),
                                              "task_id":task,"type":"codex"}))
                .unwrap(),
            );
        }
        let state = Arc::new(Mutex::new(state));
        crate::messaging::startup::open_for_test(&state).unwrap();
        state
    }

    fn call(state: &Arc<Mutex<DaemonState>>, uid: &str, method: &str, params: Value) -> Response {
        dispatch(state, &Request {
            id: "t".into(),
            caller: Caller::session(uid),
            method: method.into(),
            params,
        })
    }

    fn daemon_id(state: &Arc<Mutex<DaemonState>>) -> String {
        let handle = state.lock().unwrap().messaging.clone();
        let slot = handle.lock().unwrap();
        slot.as_ref().unwrap().daemon_id.clone()
    }

    const BOARD: &str = r#"{"id":"b-1","slug":"sfd","name":"Swarm"}"#;

    fn call_gate(state: &Arc<Mutex<DaemonState>>, uid: &str, method: &str, params: Value) -> Response {
        crate::items::gate::dispatch(state, &Request {
            id: "t".into(),
            caller: Caller::session(uid),
            method: method.into(),
            params,
        })
    }

    #[test]
    fn gate_report_stamps_the_actor_and_returns_the_computed_verdict() {
        let _env = crate::test_support::env_lock();
        let root = tempfile::tempdir().unwrap();
        let (url, seen) = stub(|method, path, _| match (method, path) {
            ("POST", "/boards/resolve") => (200, BOARD.into()),
            ("GET", p) if p.starts_with("/boards/sfd/gate-profiles") => (200, r#"{"profile":{"inputs":[]}}"#.into()),
            ("POST", "/boards/sfd/items/14/receipts") => (200, r#"{"receipt":{"id":"r-sfd-14-1","gate":"gate","verdict":"VOID","headline":"VOID: fetch: void (FETCH_FAIL)","head":"aaaa","runs":{"VOID":1},"reasons":[{"level":"void","text":"fetch: void (FETCH_FAIL)"},{"level":"note","text":"x"}],"inputs":[{"name":"h","state":"missing","verification":"self_supplied_unverified"}]}}"#.into()),
            _ => (500, "{}".into()),
        });
        let state = state_with_session(root.path(), &url);
        let d = daemon_id(&state);
        let resp = call_gate(&state, "me-uid", "gate.report", json!({
            "n": 14, "head": "a".repeat(40), "klass": "py", "steps": [], "request_id": "rq",
            "attestation": "cm-gate", "verdict": "PASS",
            "inputs": [{"name": "t", "kind": "db", "content_id": "snapshot:x",
                        "verification": "hashed_at_run", "hashed_by": "cm-gate",
                        "daemon_hash": {"content_id": "snapshot:x"}}]}));
        assert!(resp.ok, "{:?}", resp.error);
        let out = resp.result.unwrap();
        assert_eq!(out["receipt"]["verdict"], "VOID", "the API's verdict, not the poster's");
        assert_eq!(out["receipt"]["reasons"], json!(["fetch: void (FETCH_FAIL)"]));
        let seen = seen.lock().unwrap();
        let (_, _, body) = seen.iter().find(|(_, p, _)| p.ends_with("/receipts")).unwrap();
        assert_eq!(body["actor"]["pid"], format!("agent:{d}:me-uid"));
        assert_eq!(body["receipt"]["class"], "py");
        assert!(body["receipt"].get("verdict").is_none(), "a posted verdict is never forwarded");
        assert!(body["receipt"].get("attestation").is_none(), "nor a posted attestation");
        let input = &body["receipt"]["inputs"][0];
        for key in ["verification", "hashed_by", "daemon_hash"] {
            assert!(input.get(key).is_none(), "poster's {key} dropped: {input}");
        }
        assert_eq!(body["request_id"], "rq");
        let missing = call_gate(&state, "me-uid", "gate.report", json!({"n": 14, "steps": []}));
        assert!(!missing.ok && missing.error.unwrap().message.contains("head is required"));
    }

    #[test]
    fn gate_report_derives_input_paths_and_refuses_unauthorized_callers_first() {
        let _env = crate::test_support::env_lock();
        let root = tempfile::tempdir().unwrap();
        let wt = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let (url, seen) = stub(|method, path, _| match (method, path) {
            ("POST", "/boards/resolve") => (200, BOARD.into()),
            ("GET", p) if p.starts_with("/boards/sfd/gate-profiles") => (200, r#"{"profile":{"inputs":[{"name":"harvest","kind":"dir","path":"harvest"}]}}"#.into()),
            ("POST", "/boards/sfd/items/14/receipts") => (200, r#"{"receipt":{"id":"r-sfd-14-1","verdict":"VOID"}}"#.into()),
            _ => (500, "{}".into()),
        });
        let state = state_with_session(root.path(), &url);
        state.lock().unwrap().tui_sessions.get_mut("me-uid").unwrap().worktree_path = Some(wt.path().display().to_string());
        let resp = call_gate(&state, "me-uid", "gate.report", json!({
            "n": 14, "head": "a".repeat(40), "steps": [],
            "worktree": {"path": "/somewhere/else"},
            "inputs": [{"name": "harvest", "kind": "dir", "resolved_path": elsewhere.path(),
                        "content_id": format!("tree:{}", "0".repeat(64))}]}));
        assert!(resp.ok, "{:?}", resp.error);
        let seen_now = seen.lock().unwrap().clone();
        let (_, _, body) = seen_now.iter().find(|(_, p, _)| p.ends_with("/receipts")).unwrap();
        let wt_c = std::fs::canonicalize(wt.path()).unwrap();
        assert_eq!(body["receipt"]["worktree"]["path"], json!(wt_c.display().to_string()));
        let input = &body["receipt"]["inputs"][0];
        assert_eq!(input["resolved_path"], json!(wt_c.join("harvest").display().to_string()));
        assert_eq!(input["daemon_hash"], json!({"missing": true}));

        // Another board, holding nothing there: refused before any profile
        // read or hashing, so nothing about this host's files leaks.
        let (url, seen) = stub(two_boards(r#"[{"board":"mine","n":1}]"#));
        state.lock().unwrap().config.api_url = url;
        let resp = call_gate(&state, "me-uid", "gate.report", json!({
            "n": 1, "head": "a".repeat(40), "steps": [], "board": "other",
            "inputs": [{"name": "harvest", "kind": "dir"}]}));
        let err = resp.error.unwrap();
        assert!(matches!(err.code, ErrorCode::Unauthorized), "{err:?}");
        assert!(!err.message.contains("harvest") && !err.message.contains("missing"));
        let seen = seen.lock().unwrap();
        assert!(!seen.iter().any(|(_, p, _)| p.contains("gate-profiles") || p.contains("/receipts")), "{seen:?}");
    }

    #[test]
    fn gate_waive_checks_the_message_in_this_daemons_store() {
        let _env = crate::test_support::env_lock();
        let root = tempfile::tempdir().unwrap();
        let (url, seen) = stub(|method, path, _| match (method, path) {
            ("POST", "/boards/resolve") => (200, BOARD.into()),
            ("POST", "/boards/sfd/items/14/grants") => (200, r#"{"grant":{"id":"g-sfd-14-1","verified":false}}"#.into()),
            _ => (500, "{}".into()),
        });
        let state = state_with_session(root.path(), &url);
        let resp = call_gate(&state, "me-uid", "gate.waive", json!({
            "n": 14, "head": "a".repeat(40), "step": "mypy", "reason": "r", "risk": "k",
            "message_id": "no-such-message",
            "message_check": {"found": true, "sender": "forged", "contains_head": true}}));
        assert!(resp.ok, "{:?}", resp.error);
        let seen = seen.lock().unwrap();
        let (_, _, body) = seen.iter().find(|(_, p, _)| p.ends_with("/grants")).unwrap();
        assert_eq!(body["message_check"], json!({"found": false}), "the caller cannot supply the check");
    }

    #[test]
    fn item_set_each_forwards_and_returns_per_item_results() {
        let _env = crate::test_support::env_lock();
        let root = tempfile::tempdir().unwrap();
        let (url, seen) = stub(|method, path, _| match (method, path) {
            ("POST", "/boards/resolve") => (200, BOARD.into()),
            ("PATCH", "/boards/sfd/items") => (200, r#"{"items":[{"n":1,"status":"done"}],"results":[{"n":1,"ok":true},{"n":9,"ok":false,"error":{"code":"not_found"}}],"unblocked":[],"warnings":[]}"#.into()),
            _ => (500, "{}".into()),
        });
        let state = state_with_session(root.path(), &url);
        let resp = call(&state, "me-uid", "item.set", json!({"n":[1,9],"status":"done","each":true}));
        assert!(resp.ok, "{:?}", resp.error);
        let out = resp.result.unwrap();
        assert_eq!(out["results"][1], json!({"n":9,"ok":false,"error":{"code":"not_found"}}));
        let seen = seen.lock().unwrap();
        let (_, _, body) = seen.iter().find(|(m, _, _)| m == "PATCH").unwrap();
        assert_eq!(body["each"], true);
    }

    #[test]
    fn item_set_preserves_scheduling_nulls_and_omissions_on_the_api_wire() {
        let _env = crate::test_support::env_lock();
        let root = tempfile::tempdir().unwrap();
        let (url, seen) = stub(|method, path, _| match (method, path) {
            ("POST", "/boards/resolve") => (200, BOARD.into()),
            ("PATCH", "/boards/sfd/items") => (200, r#"{"items":[],"unblocked":[],"warnings":[]}"#.into()),
            _ => (500, "{}".into()),
        });
        let state = state_with_session(root.path(), &url);
        for field in ["check_back", "eta", "job"] {
            for value in [Value::Null, json!(""), json!("2h")] {
                let mut params = json!({"n": 14, "note": "update"});
                params[field] = value.clone();
                let resp = call(&state, "me-uid", "item.set", params);
                assert!(resp.ok, "{field}: {:?}", resp.error);
                let log = seen.lock().unwrap();
                let body = &log.iter().rev().find(|(m, _, _)| m == "PATCH").unwrap().2;
                assert_eq!(body["set"].get(field), Some(if value == json!("") { &Value::Null } else { &value }));
            }
        }
        let resp = call(&state, "me-uid", "item.set", json!({"n": 14, "note": "update"}));
        assert!(resp.ok, "{:?}", resp.error);
        let log = seen.lock().unwrap();
        let body = &log.iter().rev().find(|(m, _, _)| m == "PATCH").unwrap().2;
        for field in ["check_back", "eta", "job"] { assert!(body["set"].get(field).is_none()); }
        // Other nullable fields retain their existing omission behavior.
        let ctx = ctx_with(people());
        let fields = item_fields(&ctx, &json!({"note": null}), false).unwrap();
        assert!(!fields.contains_key("note"));
    }

    #[test]
    fn item_set_forwards_gate_bindings_and_blank_clears() {
        let _env = crate::test_support::env_lock();
        let root = tempfile::tempdir().unwrap();
        let (url, seen) = stub(|method, path, _| match (method, path) {
            ("POST", "/boards/resolve") => (200, BOARD.into()),
            ("PATCH", "/boards/sfd/items") => (200, r#"{"items":[],"unblocked":[],"warnings":[]}"#.into()),
            _ => (500, "{}".into()),
        });
        let state = state_with_session(root.path(), &url);
        let resp = call(&state, "me-uid", "item.set", json!({"n":14,"mf":"c".repeat(40),"ruling":"EP-A1","head":""}));
        assert!(resp.ok, "{:?}", resp.error);
        let seen = seen.lock().unwrap();
        let (_, _, body) = seen.iter().find(|(m, _, _)| m == "PATCH").unwrap();
        assert_eq!(body["set"]["mf"], json!("c".repeat(40)));
        assert_eq!(body["set"]["ruling"], "EP-A1");
        assert!(body["set"]["head"].is_null() && body["set"].get("head").is_some());
    }

    #[test]
    fn board_settings_patches_the_board_and_job_passes_through() {
        let _env = crate::test_support::env_lock();
        let root = tempfile::tempdir().unwrap();
        let (url, seen) = stub(|method, path, _| match (method, path) {
            ("POST", "/boards/resolve") => (200, BOARD.into()),
            ("PATCH", "/boards/sfd") => (200, r#"{"slug":"sfd","settings":{"backlog_allowed":false,"launch_s":900}}"#.into()),
            ("PATCH", "/boards/sfd/items") => (200, r#"{"items":[],"unblocked":[],"warnings":[]}"#.into()),
            _ => (500, "{}".into()),
        });
        let state = state_with_session(root.path(), &url);
        let resp = call(&state, "me-uid", "board.settings", json!({"settings":{"backlog_allowed":false,"idle_s":1800}}));
        assert!(resp.ok, "{:?}", resp.error);
        assert_eq!(resp.result.as_ref().unwrap()["settings"]["backlog_allowed"], false);
        let resp = call(&state, "me-uid", "board.settings", json!({"settings":{}}));
        assert!(!resp.ok);
        let resp = call(&state, "me-uid", "board.settings", json!({"settings":{"actor":{"pid":"owner"}}}));
        assert!(!resp.ok, "an agent cannot pose as Owner");
        for key in ["gate_editors", "waiver_grantors", "next_number", "id"] {
            let resp = call(&state, "me-uid", "board.settings", json!({"settings":{key: []}}));
            assert!(!resp.ok && resp.error.as_ref().unwrap().message.contains("unknown board setting"), "{key}");
        }
        let resp = call(&state, "me-uid", "item.set", json!({"n":3,"eta":"40m","job":"pins L3"}));
        assert!(resp.ok, "{:?}", resp.error);
        let resp = call(&state, "me-uid", "item.set", json!({"n":3,"job":""}));
        assert!(resp.ok, "{:?}", resp.error);
        let seen = seen.lock().unwrap();
        let (_, _, body) = seen.iter().find(|(m, p, _)| m == "PATCH" && p == "/boards/sfd").unwrap();
        assert_eq!(body["backlog_allowed"], false);
        assert_eq!(body["idle_s"], 1800);
        let pid = body["actor"]["pid"].as_str().unwrap();
        assert!(pid.starts_with("agent:") && pid.ends_with(":me-uid"), "stamped by the daemon: {pid}");
        let sets: Vec<&Value> = seen.iter().filter(|(m, p, _)| m == "PATCH" && p == "/boards/sfd/items").map(|(_, _, b)| &b["set"]).collect();
        assert_eq!(sets[0]["job"], "pins L3");
        assert!(sets[1]["job"].is_null() && sets[1].get("job").is_some(), "\"\" clears the job");
    }

    #[test]
    fn item_set_forwards_reopen_and_expect_status() {
        let _env = crate::test_support::env_lock();
        let root = tempfile::tempdir().unwrap();
        let (url, seen) = stub(|method, path, _| match (method, path) {
            ("POST", "/boards/resolve") => (200, BOARD.into()),
            ("PATCH", "/boards/sfd/items") => (200, r#"{"items":[],"unblocked":[],"warnings":[]}"#.into()),
            _ => (500, "{}".into()),
        });
        let state = state_with_session(root.path(), &url);
        assert!(call(&state, "me-uid", "item.set", json!({"n":3,"status":"blocked","reopen":true,"expect_status":"done"})).ok);
        assert!(call(&state, "me-uid", "item.set", json!({"n":3,"note":"x","reopen":false})).ok);
        let seen = seen.lock().unwrap();
        let bodies: Vec<&Value> = seen.iter().filter(|(m, _, _)| m == "PATCH").map(|(_, _, b)| b).collect();
        assert_eq!((bodies[0]["reopen"].clone(), bodies[0]["expect_status"].clone()), (json!(true), json!("done")));
        assert!(bodies[1].get("reopen").is_none() && bodies[1].get("expect_status").is_none());
    }

    #[test]
    fn place_groups_and_owner_forward_to_the_api() {
        let _env = crate::test_support::env_lock();
        let root = tempfile::tempdir().unwrap();
        let (url, seen) = stub(|method, path, _| match (method, path) {
            ("POST", "/boards/resolve") => (200, BOARD.into()),
            ("POST", "/tasks/task-a/place") => (200, r#"{"board":"sfd","n":7,"item_status":"active","task_status":"running"}"#.into()),
            ("POST", "/tasks/task-a/unplace") => (200, r#"{"unplaced":true}"#.into()),
            ("GET", "/tasks/task-a") => (200, r#"{"id":"task-a","placement":{"board":"sfd","n":7}}"#.into()),
            ("POST", "/boards/sfd/groups") => (200, r#"{"id":"g-1","name":"Latent","kind":"epic"}"#.into()),
            ("PATCH", "/boards/sfd/groups/g-1") => (200, r#"{"id":"g-1"}"#.into()),
            ("PATCH", "/boards/sfd/items") => (200, r#"{"items":[],"unblocked":[],"warnings":[]}"#.into()),
            ("POST", "/boards/sfd/epics/g-1/poc") => (200, r#"{"team_id":"tm-1","staffing":[]}"#.into()),
            ("POST", "/boards/sfd/epics/g-1/close-plan") => (200, r#"{"plan_id":3,"revision":0,"items":[]}"#.into()),
            ("POST", "/boards/sfd/close-plans/3/apply") => (200, r#"{"applied":true,"pending":[]}"#.into()),
            _ => (500, "{}".into()),
        });
        let state = state_with_session(root.path(), &url);
        assert!(call(&state, "me-uid", "epic.set_poc", json!({"group_id":"g-1","member":"lfl"})).ok);
        assert!(!call(&state, "me-uid", "epic.set_poc", json!({"group_id":"g-1"})).ok, "member is required");
        assert!(call(&state, "me-uid", "epic.close_plan", json!({"group_id":"g-1","outcome":"done"})).ok);
        let applied = call(&state, "me-uid", "epic.close_apply", json!({"plan_id":3,"revision":0,"choices":{"items":{}}}));
        assert_eq!(applied.result.as_ref().unwrap()["applied"], true);
        let resp = call(&state, "me-uid", "item.place", json!({"task_id":"task-a","group":"Latent","holder":"me","replace":true}));
        assert!(resp.ok, "{:?}", resp.error);
        assert_eq!(resp.result.as_ref().unwrap()["n"], 7);
        assert!(call(&state, "me-uid", "item.unplace", json!({"task_id":"task-a","reason":"later"})).ok);
        // Another tree's task: placing or unplacing it is a task write the
        // caller has no scope for (review of 6522623).
        let resp = call(&state, "me-uid", "item.place", json!({"task_id":"t-elsewhere"}));
        assert_eq!(resp.error.as_ref().unwrap().code, ErrorCode::Unauthorized);
        let resp = call(&state, "me-uid", "item.unplace", json!({"task_id":"t-elsewhere"}));
        assert_eq!(resp.error.as_ref().unwrap().code, ErrorCode::Unauthorized);
        assert!(call(&state, "me-uid", "group.create", json!({"name":"Latent","kind":"epic","goal":"ship"})).ok);
        assert!(call(&state, "me-uid", "group.set", json!({"group_id":"g-1","kind":"epic","expected_revision":2})).ok);
        assert!(!call(&state, "me-uid", "group.set", json!({"kind":"epic"})).ok, "group_id is required");
        assert!(call(&state, "me-uid", "item.set", json!({"n":3,"owner":"me","role":"reviewer"})).ok);
        let seen = seen.lock().unwrap();
        let body = |m: &str, p: &str| seen.iter().find(|(mm, pp, _)| mm == m && pp == p).map(|(_, _, b)| b.clone()).unwrap();
        let place = body("POST", "/tasks/task-a/place");
        assert_eq!((place["board"].clone(), place["group"].clone(), place["replace"].clone()), (json!("sfd"), json!("Latent"), json!(true)));
        assert!(place["holder"]["pid"].as_str().unwrap().ends_with(":me-uid"));
        assert!(place["actor"]["pid"].as_str().unwrap().ends_with(":me-uid"));
        assert_eq!(body("POST", "/tasks/task-a/unplace")["reason"], "later");
        assert_eq!(body("POST", "/boards/sfd/groups")["kind"], "epic");
        assert_eq!(body("PATCH", "/boards/sfd/groups/g-1")["expected_revision"], 2);
        let set = body("PATCH", "/boards/sfd/items");
        assert!(set["owner"]["pid"].as_str().unwrap().ends_with(":me-uid"));
        assert_eq!(set["role"], "reviewer");
    }

    #[test]
    fn create_stamps_actor_and_resolves_holders() {
        let _env = crate::test_support::env_lock();
        let root = tempfile::tempdir().unwrap();
        let (url, seen) = stub(|method, path, _| match (method, path) {
            ("POST", "/boards/resolve") => (200, BOARD.into()),
            ("POST", "/boards/sfd/items") => (200, r#"{"items":[{"n":1,"title":"a","status":"active","holders":[{"pid":"p","name":"rl"}]}],"unblocked":[],"warnings":[]}"#.into()),
            _ => (500, "{}".into()),
        });
        let state = state_with_session(root.path(), &url);
        let d = daemon_id(&state);
        let resp = call(&state, "me-uid", "item.create", json!({"title":"a","holder":"rl","note":"n"}));
        assert!(resp.ok, "{:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["board"], "sfd");
        assert_eq!(result["items"][0]["holders"], json!(["rl"]));
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].2, json!({"task_id":"task-a"}));
        let body = &seen[1].2;
        assert_eq!(body["actor"]["pid"], format!("agent:{d}:me-uid"));
        assert_eq!(body["actor"]["daemon_id"], d);
        assert_eq!(body["actor"]["task_id"], "task-a");
        assert_eq!(body["items"][0]["holders"][0]["pid"], format!("agent:{d}:rl-uid"));
        assert_eq!(body["items"][0]["note"], "n");
    }

    /// Boards: the caller's task resolves to `mine`; `board="other"` to `other`.
    fn two_boards(held: &'static str) -> impl Fn(&str, &str, &Value) -> (u16, String) {
        move |method, path, body| match (method, path) {
            ("POST", "/boards/resolve") if body["ref"] == "other" => {
                (200, r#"{"id":"b-other","slug":"other"}"#.into())
            }
            ("POST", "/boards/resolve") => (200, r#"{"id":"b-mine","slug":"mine"}"#.into()),
            ("GET", p) if p.starts_with("/items?") => (200, held.into()),
            ("PATCH", "/boards/other/items") => (200, r#"{"items":[]}"#.into()),
            _ => (200, "{}".into()),
        }
    }

    #[test]
    fn editing_another_board_needs_a_held_item_there() {
        let _env = crate::test_support::env_lock();
        let root = tempfile::tempdir().unwrap();
        let (url, seen) = stub(two_boards(r#"[{"board":"mine","n":1}]"#));
        let state = state_with_session(root.path(), &url);
        let resp = call(&state, "me-uid", "item.set", json!({"n":1,"status":"done","board":"other"}));
        let err = resp.error.unwrap();
        assert!(matches!(err.code, ErrorCode::Unauthorized));
        assert!(err.message.contains("not your task's board"));
        assert!(!seen.lock().unwrap().iter().any(|(m, ..)| m == "PATCH"));
        // Reading it is allowed.
        assert!(call(&state, "me-uid", "board.read", json!({"board":"other"})).ok);

        let (url, seen) = stub(two_boards(r#"[{"board":"other","n":4}]"#));
        state.lock().unwrap().config.api_url = url;
        let resp = call(&state, "me-uid", "item.set", json!({"n":1,"status":"done","board":"other"}));
        assert!(resp.ok, "{:?}", resp.error);
        assert!(seen.lock().unwrap().iter().any(|(m, p, _)| m == "PATCH" && p == "/boards/other/items"));
    }

    #[test]
    fn global_sessions_edit_any_board() {
        let _env = crate::test_support::env_lock();
        let root = tempfile::tempdir().unwrap();
        let (url, seen) = stub(two_boards("[]"));
        let state = state_with_session(root.path(), &url);
        state.lock().unwrap().tui_sessions.get_mut("me-uid").unwrap().global_perms = true;
        let resp = call(&state, "me-uid", "item.set", json!({"n":1,"status":"done","board":"other"}));
        assert!(resp.ok, "{:?}", resp.error);
        assert!(!seen.lock().unwrap().iter().any(|(m, ..)| m == "GET"));
    }

    #[test]
    fn api_errors_surface_with_their_codes() {
        let _env = crate::test_support::env_lock();
        let root = tempfile::tempdir().unwrap();
        let (url, _) = stub(|method, _, _| match method {
            "POST" => (200, BOARD.into()),
            _ => (409, r#"{"detail":{"code":"cycle","message":"cycle: 1→2→1"}}"#.into()),
        });
        let state = state_with_session(root.path(), &url);
        let resp = call(&state, "me-uid", "item.set", json!({"n":1,"blocked_by":[2]}));
        let err = resp.error.unwrap();
        assert!(matches!(err.code, ErrorCode::Conflict));
        assert_eq!(err.message, "cycle: 1→2→1");
    }

    #[test]
    fn unregistered_callers_and_missing_identity_are_refused() {
        let _env = crate::test_support::env_lock();
        let root = tempfile::tempdir().unwrap();
        let state = state_with_session(root.path(), "http://127.0.0.1:9");
        let resp = call(&state, "ghost", "board.read", json!({}));
        assert!(matches!(resp.error.unwrap().code, ErrorCode::Unauthorized));
        let mut bare = DaemonState::default();
        bare.tui_sessions.insert("me-uid".into(), serde_json::from_value(json!({"uid":"me-uid"})).unwrap());
        let bare = Arc::new(Mutex::new(bare));
        let resp = call(&bare, "me-uid", "board.read", json!({}));
        assert!(resp.error.unwrap().message.starts_with("daemon_id_unavailable"));
    }

    #[test]
    fn tui_owned_callers_get_a_clear_launch_error() {
        let _env = crate::test_support::env_lock();
        let root = tempfile::tempdir().unwrap();
        let (url, seen) = stub(|_, _, _| (200, BOARD.into()));
        let state = state_with_session(root.path(), &url);
        let resp = call(&state, "me-uid", "item.resolve", json!({"n":1,"action":"launch"}));
        let err = resp.error.unwrap();
        assert!(matches!(err.code, ErrorCode::InvalidParams));
        assert!(err.message.contains("TUI-owned"), "{}", err.message);
        assert!(!seen.lock().unwrap().iter().any(|(_, p, _)| p.ends_with("/resolve") && p != "/boards/resolve"));
    }

    #[test]
    fn create_passes_the_request_id_and_reports_replays() {
        let _env = crate::test_support::env_lock();
        let root = tempfile::tempdir().unwrap();
        let (url, seen) = stub(|_, path, _| match path {
            "/boards/resolve" => (200, BOARD.into()),
            _ => (200, r#"{"items":[{"n":1,"title":"a","status":"active","holders":[]}],"replayed":true}"#.into()),
        });
        let state = state_with_session(root.path(), &url);
        let resp = call(&state, "me-uid", "item.create", json!({"title":"a","request_id":"rq-9"}));
        assert_eq!(resp.result.unwrap()["replayed"], true);
        assert_eq!(seen.lock().unwrap()[1].2["request_id"], "rq-9");
    }

    #[test]
    fn unreachable_api_fails_fast_with_a_clear_error() {
        let _env = crate::test_support::env_lock();
        let root = tempfile::tempdir().unwrap();
        let state = state_with_session(root.path(), "http://127.0.0.1:9");
        let resp = call(&state, "me-uid", "board.read", json!({}));
        assert!(resp.error.unwrap().message.starts_with("planning_api_unavailable"));
    }
}
