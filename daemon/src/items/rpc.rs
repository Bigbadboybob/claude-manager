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

pub const METHODS: &[&str] = &["item.create", "item.set", "item.resolve", "board.read"];

type RpcResult<T = Value> = Result<T, (ErrorCode, String)>;

fn bad(message: impl Into<String>) -> (ErrorCode, String) {
    (ErrorCode::InvalidParams, message.into())
}

pub fn dispatch(state: &Arc<Mutex<DaemonState>>, req: &Request) -> Response {
    let result = Ctx::capture(state, req).and_then(|ctx| match req.method.as_str() {
        "item.create" => item_create(&ctx, &req.params),
        "item.set" => item_set(&ctx, &req.params),
        "item.resolve" => item_resolve(state, &ctx, &req.params),
        "board.read" => board_read(&ctx, &req.params),
        other => Err((ErrorCode::UnknownMethod, format!("unknown items method {other}"))),
    });
    match result {
        Ok(value) => Response::ok(req.id.clone(), value),
        Err((code, message)) => Response::err(req.id.clone(), code, message),
    }
}

/// Who is calling, resolved on this daemon.
pub(crate) struct Ctx {
    pub pid: String,
    pub name: String,
    pub uid: Option<String>,
    pub task_id: Option<String>,
    pub owner: bool,
    pub global: bool,
    pub engine: Option<String>,
    pub daemon_id: String,
    /// Every known participant: `{id, name, session_uid, aliases?, released?, present}`.
    pub people: Vec<Value>,
    pub api: Api,
}

impl Ctx {
    fn capture(state: &Arc<Mutex<DaemonState>>, req: &Request) -> RpcResult<Self> {
        if matches!(req.caller, Caller::Operator(_)) {
            crate::control::operator::validate_operator(&req.caller)
                .map_err(|e| (ErrorCode::Unauthorized, e.to_string()))?;
        }
        if !req.params.is_object() && !req.params.is_null() {
            return Err(bad("expected object parameters"));
        }
        let (me, live, handle, api_url, api_token) = {
            let s = state.lock().unwrap_or_else(|p| p.into_inner());
            let me = match &req.caller {
                Caller::Operator(_) => None,
                Caller::Session(c) => {
                    let uid = c.session_uid.clone();
                    if let Some(sess) = s.sessions.get(&uid) {
                        Some((uid, sess.title.clone(), sess.task_id.clone(), sess.global_perms,
                              Some(sess.session_type.clone())))
                    } else if let Some(t) = s.tui_sessions.get(&uid) {
                        Some((uid.clone(), t.label.clone().unwrap_or(uid), t.task_id.clone(),
                              t.global_perms, t.session_type.clone()))
                    } else {
                        return Err((
                            ErrorCode::Unauthorized,
                            "item tools need a registered CM session".into(),
                        ));
                    }
                }
            };
            let live: Vec<(String, String, Option<String>)> = s
                .sessions
                .values()
                .map(|v| (v.uid.clone(), v.title.clone(), v.task_id.clone()))
                .chain(s.tui_sessions.values().filter(|t| !s.sessions.contains_key(&t.uid)).map(
                    |t| (t.uid.clone(), t.label.clone().unwrap_or_else(|| t.uid.clone()), t.task_id.clone()),
                ))
                .collect();
            (me, live, s.messaging.clone(), s.config.api_url.clone(), s.config.api_token.clone())
        };
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
                .iter()
                .map(|(uid, name, task)| Person {
                    id: store.participant_id(uid),
                    name: name.clone(),
                    session_uid: uid.clone(),
                    task: task.clone(),
                    present: true,
                    kind: "agent".into(),
                })
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
        Ok(match me {
            None => Ctx {
                pid: "owner".into(),
                name: "Owner".into(),
                uid: None,
                task_id: None,
                owner: true,
                global: true,
                engine: None,
                daemon_id,
                people,
                api,
            },
            Some((uid, title, task_id, global, engine)) => Ctx {
                pid: format!("agent:{daemon_id}:{uid}"),
                name: my_name.unwrap_or(title),
                uid: Some(uid),
                task_id,
                owner: false,
                global,
                engine,
                daemon_id,
                people,
                api,
            },
        })
    }

    fn actor(&self) -> Value {
        json!({
            "pid": self.pid,
            "name": self.name,
            "session_uid": self.uid,
            "daemon_id": if self.owner { Value::Null } else { json!(self.daemon_id) },
            "task_id": self.task_id,
        })
    }

    fn me(&self) -> Value {
        holder_value(&self.pid, Some(&self.name))
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
        return Ok(holder_value(r, name.as_deref()));
    }
    if let Some(p) = ctx.people.iter().find(|p| p["session_uid"] == r && p["id"].as_str().is_some()) {
        return Ok(holder_value(p["id"].as_str().unwrap(), name_of(p).as_deref()));
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
        [one] => Ok(holder_value(one["id"].as_str().unwrap_or_default(), name_of(one).as_deref())),
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

fn present(params: &Value, key: &str) -> Option<Value> {
    params.get(key).filter(|v| !v.is_null()).cloned()
}

fn int_list(value: &Value, name: &str) -> RpcResult<Vec<i64>> {
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
fn resolve_board(ctx: &Ctx, params: &Value) -> RpcResult {
    if let Some(reference) = params.get("board").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
        return ctx.api.post("/boards/resolve", &json!({"ref": reference.trim()}));
    }
    match &ctx.task_id {
        Some(task_id) => ctx.api.post("/boards/resolve", &json!({"task_id": task_id})),
        None => Err(bad("no board: this session has no task; pass board=<slug>")),
    }
}

/// Editing rights (doc §2 rule 9): Owner, a global session, a session whose
/// task resolves to the board, or a current holder of any item on it.
fn authorize_edit(ctx: &Ctx, params: &Value, board: &Value) -> RpcResult<()> {
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
    Err((
        ErrorCode::Unauthorized,
        format!(
            "board {} is not your task's board and you hold no item on it; reading it with board() is allowed",
            board["slug"].as_str().unwrap_or("?")
        ),
    ))
}

fn slug(board: &Value) -> RpcResult<String> {
    board["slug"].as_str().map(str::to_string).ok_or_else(|| {
        (ErrorCode::Internal, "planning API returned a board without a slug".into())
    })
}

/// Fields passed straight through to the API; "" clears a text field.
const PASS_FIELDS: &[&str] = &[
    "status", "note", "group", "blocked_on", "check_back", "eta", "title", "links", "blocked_by",
];
const CLEARABLE: &[&str] = &["note", "group", "blocked_on", "check_back", "eta"];

fn item_fields(ctx: &Ctx, params: &Value, skip_title: bool) -> RpcResult<Map<String, Value>> {
    let mut fields = Map::new();
    for key in PASS_FIELDS {
        if skip_title && *key == "title" {
            continue;
        }
        let Some(mut value) = present(params, key) else { continue };
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
    })
}

fn holder_label(h: &Value) -> Value {
    h["name"].as_str().map(|n| json!(n)).unwrap_or_else(|| h["pid"].clone())
}

fn write_reply(board_slug: &str, reply: &Value, items_key: &str) -> Value {
    let mut out = json!({"board": board_slug});
    if let Some(items) = reply[items_key].as_array() {
        out["items"] = json!(items.iter().map(compact).collect::<Vec<_>>());
    }
    if reply["unblocked"].as_array().is_some_and(|u| !u.is_empty()) {
        out["unblocked"] = reply["unblocked"].clone();
    }
    if reply["warnings"].as_array().is_some_and(|w| !w.is_empty()) {
        out["warnings"] = reply["warnings"].clone();
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
    let reply = ctx.api.post(&format!("/boards/{slug}/items"), &json!({"actor": ctx.actor(), "items": specs}))?;
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
    if let Some(remove) = present(params, "remove_holder") {
        let pids: Vec<Value> = holders_arg(ctx, &remove)?.into_iter().map(|h| h["pid"].clone()).collect();
        body["remove_holders"] = json!(pids);
    }
    if let Some(reason) = present(params, "reason") {
        body["reason"] = reason;
    }
    if body["set"].as_object().is_some_and(Map::is_empty)
        && body.get("add_holders").is_none()
        && body.get("remove_holders").is_none()
    {
        return Err(bad("nothing to change: pass status, note, holder or another field"));
    }
    let board = resolve_board(ctx, params)?;
    authorize_edit(ctx, params, &board)?;
    let slug = slug(&board)?;
    let reply = ctx.api.patch(&format!("/boards/{slug}/items"), &body)?;
    Ok(write_reply(&slug, &reply, "items"))
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
    for key in ["kind", "blocked_on", "check_back", "reason", "message"] {
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
        let Some(uid) = ctx.uid.as_deref() else {
            return Err(bad("launch needs a session caller; Owner launches from the TUI"));
        };
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
        let pid = format!("agent:{}:{new_uid}", ctx.daemon_id);
        body["action"] = json!("reassign");
        body["holders"] = json!([holder_value(&pid, Some(&format!("item-{n}")))]);
        launched = json!({"uid": new_uid, "engine": engine, "pid": pid});
    }
    let reply = ctx.api.post(&format!("/boards/{slug}/items/{n}/resolve"), &body)?;
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

// ---- board() --------------------------------------------------------------

fn short_duration(seconds: i64) -> String {
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
    let st = &h["state"];
    let state = st["state"].as_str().unwrap_or("unknown");
    let mut out = format!("@{name}[{state}");
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
    if item["status"] == "waiting" {
        if let Some(eta) = parse_ts(&item["eta_at"]) {
            let left = (eta - now).num_seconds();
            if left >= 0 {
                line.push_str(&format!(" eta {}", short_duration(left)));
            } else {
                line.push_str(&format!(" eta passed {} ago", short_duration(-left)));
            }
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
    })
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
    let free: Vec<Value> = read["free_capacity"]
        .as_array()
        .map(|v| v.iter().map(|s| s["name"].as_str().map(|n| json!(n)).unwrap_or_else(|| s["pid"].clone())).collect())
        .unwrap_or_default();
    let mut out = json!({"board": board_header(&read["board"]), "free_capacity": free});
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
            pid: "agent:d1:me-uid".into(),
            name: "lane".into(),
            uid: Some("me-uid".into()),
            task_id: Some("t-1".into()),
            owner: false,
            global: false,
            engine: Some("codex".into()),
            daemon_id: "d1".into(),
            people,
            api: Api::from_config("http://127.0.0.1:9", "tok").unwrap(),
        }
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
            "free_capacity": [{"pid":"agent:d1:f","name":"idle-lane","state":"idle"}]
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
        assert_eq!(out["free_capacity"], json!(["idle-lane"]));
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
                                              "task_id":task,"session_type":"codex"}))
                .unwrap(),
            );
        }
        let state = Arc::new(Mutex::new(state));
        crate::messaging::rpc::initialize(&state).unwrap();
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
        assert_eq!(err.message, "cycle: cycle: 1→2→1");
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
    fn unreachable_api_fails_fast_with_a_clear_error() {
        let _env = crate::test_support::env_lock();
        let root = tempfile::tempdir().unwrap();
        let state = state_with_session(root.path(), "http://127.0.0.1:9");
        let resp = call(&state, "me-uid", "board.read", json!({}));
        assert!(resp.error.unwrap().message.starts_with("planning_api_unavailable"));
    }
}
