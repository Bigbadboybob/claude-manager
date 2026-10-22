//! Daemon methods `gate.report`, `gate.receipts`, `gate.waive`, `gate.profile`
//! (doc/gate-receipts.md §9), and the refresh that watches this host's
//! receipts for base, runner and branch moves (§8).
//!
//! The verdict is computed by the planning API; the daemon stamps the
//! caller as actor and, for waivers, checks the linked message in its own
//! messaging store: who sent it and whether it names the SHA.
//!
//! Lock discipline: the `DaemonState` mutex and the messaging-store slot are
//! taken one after the other, never nested, and neither is held across a
//! planning API call or a git command.

use super::rpc::{authorize_edit, bad, int_list, present, resolve_board, slug, Ctx, RpcResult};
use crate::control::protocol::{ErrorCode, Request, Response};
use crate::state::DaemonState;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const METHODS: &[&str] = &["gate.report", "gate.receipts", "gate.waive", "gate.profile"];

/// Receipt fields `gate.report` forwards as the receipt body.
const RECEIPT_FIELDS: &[&str] = &[
    "gate", "klass", "head", "mf", "runner", "ruling", "base", "closure", "worktree", "inputs",
    "steps", "waivers", "log_path", "attestation", "repo", "branch",
];

pub fn dispatch(state: &Arc<Mutex<DaemonState>>, req: &Request) -> Response {
    let result = Ctx::capture(state, req).and_then(|ctx| match req.method.as_str() {
        "gate.report" => report(&ctx, &req.params),
        "gate.receipts" => receipts(state, &ctx, &req.params),
        "gate.waive" => waive(state, &ctx, &req.params),
        "gate.profile" => profile(&ctx, &req.params),
        other => Err((ErrorCode::UnknownMethod, format!("unknown gate method {other}"))),
    });
    match result {
        Ok(value) => Response::ok(req.id.clone(), value),
        Err((code, message)) => Response::err(req.id.clone(), code, message),
    }
}

fn item_n(params: &Value) -> RpcResult<i64> {
    match int_list(params.get("n").unwrap_or(&Value::Null), "n")?.as_slice() {
        [n] => Ok(*n),
        _ => Err(bad("n is one item number")),
    }
}

fn str_param<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

/// The compact reply an agent reads: verdict, headline, blocking reasons.
fn compact_receipt(r: &Value) -> Value {
    let blocking: Vec<&Value> = r["reasons"]
        .as_array()
        .map(|rs| {
            rs.iter()
                .filter(|x| matches!(x["level"].as_str(), Some("fail" | "void" | "incomplete")))
                .collect()
        })
        .unwrap_or_default();
    let mut out = json!({
        "id": r["id"],
        "gate": r["gate"],
        "verdict": r["verdict"],
        "headline": r["headline"],
        "head": r["head"],
        "reasons": blocking.iter().map(|x| x["text"].clone()).collect::<Vec<_>>(),
    });
    for key in ["was", "stale", "runs", "unchecked_s"] {
        if !r[key].is_null() {
            out[key] = r[key].clone();
        }
    }
    if let Some(inputs) = r["inputs"].as_array().filter(|i| !i.is_empty()) {
        out["inputs"] = json!(inputs
            .iter()
            .map(|i| json!({"name": i["name"], "state": i["state"], "verification": i["verification"]}))
            .collect::<Vec<_>>());
    }
    out
}

fn report(ctx: &Ctx, params: &Value) -> RpcResult {
    let n = item_n(params)?;
    if str_param(params, "head").is_none() {
        return Err(bad("head is required (the full commit SHA the gate ran on)"));
    }
    if !params["steps"].is_array() {
        return Err(bad("steps is required: every planned step, including failed, skipped and not run"));
    }
    let mut receipt = Map::new();
    for key in RECEIPT_FIELDS {
        if let Some(value) = present(params, key) {
            receipt.insert(if *key == "klass" { "class".into() } else { (*key).into() }, value);
        }
    }
    let board = resolve_board(ctx, params)?;
    authorize_edit(ctx, params, &board)?;
    let slug = slug(&board)?;
    let mut body = json!({"actor": ctx.actor(), "receipt": receipt});
    if let Some(request_id) = str_param(params, "request_id") {
        body["request_id"] = json!(request_id);
    }
    let reply = ctx.api.post(&format!("/boards/{slug}/items/{n}/receipts"), &body)?;
    let mut out = json!({"board": slug, "n": n, "receipt": compact_receipt(&reply["receipt"])});
    if reply["replayed"] == true {
        out["replayed"] = json!(true);
    }
    Ok(out)
}

/// `{found, sender, contains_head, contains_base}` for a linked message.
pub(crate) fn message_check(state: &Arc<Mutex<DaemonState>>, message_id: &str, head: &str, base: Option<&str>) -> Value {
    let handle = state.lock().unwrap_or_else(|p| p.into_inner()).messaging.clone();
    let found = {
        let slot = handle.lock().unwrap_or_else(|p| p.into_inner());
        slot.as_ref().and_then(|store| store.message_sender_and_body(message_id))
    };
    check_body(found, head, base)
}

/// The SHA must appear in the message (at least its first 12 characters).
pub(crate) fn check_body(found: Option<(String, String)>, head: &str, base: Option<&str>) -> Value {
    let Some((sender, body)) = found else {
        return json!({"found": false});
    };
    let lower = body.to_lowercase();
    let names = |sha: &str| {
        let sha = sha.trim().to_lowercase();
        sha.len() >= 12 && lower.contains(&sha[..12])
    };
    let mut out = json!({"found": true, "sender": sender, "contains_head": names(head)});
    if let Some(base) = base {
        out["contains_base"] = json!(names(base));
    }
    out
}

fn receipts(state: &Arc<Mutex<DaemonState>>, ctx: &Ctx, params: &Value) -> RpcResult {
    let n = item_n(params)?;
    let board = resolve_board(ctx, params)?;
    let slug = slug(&board)?;
    let mut query: Vec<(&str, String)> = vec![];
    if let Some(gate) = str_param(params, "gate") {
        query.push(("gate", gate.into()));
    }
    if params["history"] == true {
        query.push(("history", "true".into()));
    }
    let path = format!("/boards/{slug}/items/{n}/receipts");
    let mut reply = ctx.api.get(&path, &query)?;
    // A grant whose message had not reached this host yet is rechecked here.
    let mut rechecked = false;
    for g in reply["grants"].as_array().cloned().unwrap_or_default() {
        let waiting = g["problems"].as_array().is_some_and(|p| p.iter().any(|x| x == "message not found"));
        let (Some(id), Some(msg), Some(head)) = (g["id"].as_str(), g["message_id"].as_str(), g["head"].as_str()) else {
            continue;
        };
        if !waiting {
            continue;
        }
        let check = message_check(state, msg, head, g["base"].as_str());
        if check["found"] == true {
            ctx.api.patch(
                &format!("/boards/{slug}/items/{n}/grants/{id}"),
                &json!({"actor": ctx.actor(), "message_check": check}),
            )?;
            rechecked = true;
        }
    }
    if rechecked {
        reply = ctx.api.get(&path, &query)?;
    }
    Ok(reply)
}

fn waive(state: &Arc<Mutex<DaemonState>>, ctx: &Ctx, params: &Value) -> RpcResult {
    let n = item_n(params)?;
    let board = resolve_board(ctx, params)?;
    let slug = slug(&board)?;
    if let Some(label) = str_param(params, "revoke") {
        return ctx.api.patch(
            &format!("/boards/{slug}/items/{n}/grants/{label}"),
            &json!({"actor": ctx.actor(), "revoke": true}),
        );
    }
    let head = str_param(params, "head").ok_or_else(|| bad("head is required (the exact HEAD SHA the grant covers)"))?;
    let message_id = str_param(params, "message_id")
        .ok_or_else(|| bad("message_id is required: the chat message in which you granted it"))?;
    let base = str_param(params, "base");
    let mut body = json!({
        "actor": ctx.actor(),
        "kind": str_param(params, "kind").unwrap_or("waiver"),
        "gate": str_param(params, "gate").unwrap_or("gate"),
        "head": head,
        "step": str_param(params, "step").ok_or_else(|| bad("step is required"))?,
        "reason": params["reason"],
        "risk": params["risk"],
        "message_id": message_id,
        "message_check": message_check(state, message_id, head, base),
    });
    for key in ["base", "nodes", "cmd_digest"] {
        if let Some(value) = present(params, key) {
            body[key] = value;
        }
    }
    ctx.api.post(&format!("/boards/{slug}/items/{n}/grants"), &body)
}

fn profile(ctx: &Ctx, params: &Value) -> RpcResult {
    let board = resolve_board(ctx, params)?;
    let slug = slug(&board)?;
    let klass = str_param(params, "klass").or_else(|| str_param(params, "class"));
    let writing = params.get("profile").is_some_and(|p| !p.is_null()) || params["delete"] == true;
    if !writing {
        let mut query: Vec<(&str, String)> = vec![];
        if let Some(k) = klass {
            query.push(("class", k.into()));
        }
        if let Some(n) = params.get("n").and_then(Value::as_i64) {
            query.push(("n", n.to_string()));
        }
        return ctx.api.get(&format!("/boards/{slug}/gate-profiles"), &query);
    }
    let klass = klass.ok_or_else(|| bad("klass names the profile class to write"))?;
    let reason = str_param(params, "reason").ok_or_else(|| bad("reason is required for a profile edit"))?;
    ctx.api.put(
        &format!("/boards/{slug}/gate-profiles/{klass}"),
        &json!({
            "actor": ctx.actor(),
            "profile": if params["delete"] == true { Value::Null } else { params["profile"].clone() },
            "reason": reason,
            "default": params["default"] == true,
        }),
    )
}

/// The slim board line's gate cell: ` ✓PASS a1b2c3d r-sejd-14-3`,
/// ` ⟲STALE head_moved (was PASS)`.
pub(crate) fn slim_cell(name: &str, g: &Value) -> String {
    let verdict = g["verdict"].as_str().unwrap_or("?");
    let mark = match verdict {
        "PASS" => "✓",
        "FAIL" => "✗",
        "VOID" => "⊘",
        "INCOMPLETE" => "◐",
        "STALE" => "⟲",
        _ => "?",
    };
    let prefix = if name == "gate" { String::new() } else { format!("{name}:") };
    let mut out = if verdict == "STALE" {
        format!(
            " {prefix}{mark}STALE {} (was {})",
            g["stale"]["reason"].as_str().unwrap_or("?"),
            g["was"].as_str().unwrap_or("?")
        )
    } else {
        format!(
            " {prefix}{mark}{verdict} {} {}",
            g["head"].as_str().map(|h| &h[..h.len().min(7)]).unwrap_or("?"),
            g["id"].as_str().unwrap_or("")
        )
    };
    if let Some(s) = g["unchecked_s"].as_i64() {
        out.push_str(&format!(" (base unchecked {})", super::rpc::short_duration(s)));
    }
    out
}

// ---- refresh (§8) ---------------------------------------------------------------

const REFRESH_INTERVAL: Duration = Duration::from_secs(300);
const GIT_TIMEOUT_S: u64 = 60;
const MAX_CHANGED_PATHS: usize = 2000;

pub fn start_refresh(state: &Arc<Mutex<DaemonState>>) {
    let state = Arc::clone(state);
    let spawned = std::thread::Builder::new().name("gate-refresh".into()).spawn(move || loop {
        std::thread::sleep(REFRESH_INTERVAL);
        refresh_once(&state);
    });
    if let Err(e) = spawned {
        eprintln!("cm items: gate refresh thread failed to start: {e}");
    }
}

fn refresh_snapshot(state: &Arc<Mutex<DaemonState>>) -> Option<(String, super::api::Api)> {
    let (handle, api_url, api_token) = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        (s.messaging.clone(), s.config.api_url.clone(), s.config.api_token.clone())
    };
    let daemon_id = {
        let slot = handle.lock().unwrap_or_else(|p| p.into_inner());
        slot.as_ref()?.daemon_id.clone()
    };
    let api = super::api::Api::from_config(&api_url, &api_token).ok()?;
    Some((daemon_id, api.with_timeout(Duration::from_secs(20))))
}

/// One pass: ask which of this host's receipts to watch, observe them with
/// git, report. Best-effort; failures wait for the next pass.
pub(crate) fn refresh_once(state: &Arc<Mutex<DaemonState>>) {
    let Some((daemon_id, api)) = refresh_snapshot(state) else { return };
    let Ok(reply) = api.post("/receipts/refresh", &json!({"daemon_id": daemon_id, "checks": []})) else {
        return;
    };
    let watch = reply["watch"].as_array().cloned().unwrap_or_default();
    if watch.is_empty() {
        return;
    }
    let mut fetched = HashSet::new();
    let checks: Vec<Value> = watch.iter().map(|w| observe(w, &mut fetched)).collect();
    if let Err((_, e)) = api.post("/receipts/refresh", &json!({"daemon_id": daemon_id, "checks": checks})) {
        eprintln!("cm items: gate refresh report failed: {e}");
    }
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("timeout")
        .arg(GIT_TIMEOUT_S.to_string())
        .arg("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn rev(dir: &Path, rev: &str) -> Option<String> {
    git(dir, &["rev-parse", "--verify", "--quiet", rev]).filter(|s| s.len() == 40)
}

/// Observe one watched receipt in its checkout on this host.
pub(crate) fn observe(w: &Value, fetched: &mut HashSet<(String, String)>) -> Value {
    let id = w["receipt_id"].clone();
    let Some(path) = w["path"].as_str().map(Path::new).filter(|p| p.is_dir()) else {
        return json!({"receipt_id": id, "error": "checkout missing on this host"});
    };
    let mut out = json!({"receipt_id": id});
    for reference in [w["base_ref"].as_str(), w["runner_ref"].as_str()].into_iter().flatten() {
        // `origin/main` or `origin/main:path`: fetch origin's main once per checkout.
        let refname = reference.split(':').next().unwrap_or(reference);
        if let Some((remote, branch)) = refname.split_once('/') {
            let key = (path.display().to_string(), refname.to_string());
            if fetched.insert(key) {
                let _ = git(path, &["fetch", "--quiet", "--no-tags", remote, branch]);
            }
        }
    }
    if let Some(branch) = w["branch"].as_str() {
        if let Some(head) = rev(path, &format!("refs/heads/{branch}")) {
            out["branch_head"] = json!(head);
        }
    }
    if let Some(runner_ref) = w["runner_ref"].as_str() {
        if let Some(sha) = rev(path, runner_ref) {
            out["runner_sha"] = json!(sha);
        }
    }
    let (Some(base_ref), Some(head)) = (w["base_ref"].as_str(), w["head"].as_str()) else {
        return out;
    };
    let Some(base_now) = rev(path, base_ref) else { return out };
    out["base_sha"] = json!(base_now);
    let before = w["base_sha_after"].as_str().unwrap_or_default();
    if base_now == before || before.is_empty() {
        return out;
    }
    if w["scope"] == "tree" {
        match Command::new("timeout")
            .arg(GIT_TIMEOUT_S.to_string())
            .args(["git", "-C"])
            .arg(path)
            .args(["merge-tree", "--write-tree", &base_now, head])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
        {
            Ok(o) if o.status.code() == Some(0) => {
                let text = String::from_utf8_lossy(&o.stdout);
                if let Some(tree) = text.lines().next().filter(|t| t.len() >= 40) {
                    out["merge_tree"] = json!(tree.trim());
                }
            }
            Ok(o) if o.status.code() == Some(1) => out["merge_tree"] = json!("conflict"),
            _ => {}
        }
    } else if let Some(diff) = git(path, &["diff", "--name-only", before, &base_now]) {
        let paths: Vec<&str> = diff.lines().filter(|l| !l.is_empty()).collect();
        if paths.len() > MAX_CHANGED_PATHS {
            out["changed_paths_truncated"] = json!(true);
        } else {
            out["changed_paths"] = json!(paths);
        }
    } else {
        out["changed_paths_truncated"] = json!(true);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slim_cells_read_compactly() {
        let pass = json!({"verdict":"PASS","head":"9f8e7d6aaaaaaaaaaaaa","id":"r-sejd-14-3"});
        assert_eq!(slim_cell("gate", &pass), " ✓PASS 9f8e7d6 r-sejd-14-3");
        let stale = json!({"verdict":"STALE","was":"PASS","stale":{"reason":"head_moved"}});
        assert_eq!(slim_cell("gate", &stale), " ⟲STALE head_moved (was PASS)");
        let unchecked = json!({"verdict":"VOID","head":"abc","id":"r-x-1-2","unchecked_s":2400});
        assert_eq!(slim_cell("bench", &unchecked), " bench:⊘VOID abc r-x-1-2 (base unchecked 40m)");
    }

    #[test]
    fn message_checks_need_the_sender_and_the_sha() {
        let head = "a1b2c3d4e5f6a7b8c9d0a1b2c3d4e5f6a7b8c9d0";
        assert_eq!(check_body(None, head, None), json!({"found": false}));
        let ok = check_body(Some(("agent:d:ep".into(), format!("WAIVE mypy on {} : risk …", &head[..12]))), head, None);
        assert_eq!(ok, json!({"found": true, "sender": "agent:d:ep", "contains_head": true}));
        let short = check_body(Some(("agent:d:ep".into(), format!("WAIVE mypy on {}", &head[..7]))), head, Some(head));
        assert_eq!(short["contains_head"], false, "a 7-char SHA is too short to bind");
        assert_eq!(short["contains_base"], false);
    }

    fn git_ok(dir: &Path, args: &[&str]) {
        let st = Command::new("git").arg("-C").arg(dir).args(args)
            .env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t")
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        assert!(st.success(), "git {args:?}");
    }

    #[test]
    fn observe_reports_branch_runner_and_base_changes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git_ok(p, &["init", "-q", "-b", "main"]);
        std::fs::write(p.join("a.py"), "1").unwrap();
        std::fs::write(p.join("gate.sh"), "run").unwrap();
        git_ok(p, &["add", "."]);
        git_ok(p, &["commit", "-qm", "base"]);
        let base = rev(p, "HEAD").unwrap();
        git_ok(p, &["checkout", "-qb", "lane"]);
        std::fs::write(p.join("a.py"), "2").unwrap();
        git_ok(p, &["commit", "-qam", "lane"]);
        let head = rev(p, "HEAD").unwrap();
        git_ok(p, &["checkout", "-q", "main"]);
        std::fs::write(p.join("docs.md"), "x").unwrap();
        git_ok(p, &["add", "."]);
        git_ok(p, &["commit", "-qm", "unrelated"]);
        let w = json!({"receipt_id": 7, "path": p, "branch": "lane", "head": head,
                       "base_ref": "main", "base_sha_after": base, "runner_ref": "main:gate.sh",
                       "scope": "paths"});
        let out = observe(&w, &mut HashSet::new());
        assert_eq!(out["branch_head"], json!(head));
        assert_eq!(out["changed_paths"], json!(["docs.md"]));
        assert_eq!(out["runner_sha"].as_str().map(str::len), Some(40));
        let tree = observe(&json!({"receipt_id": 7, "path": p, "head": head, "base_ref": "main",
                                    "base_sha_after": base, "scope": "tree"}), &mut HashSet::new());
        assert_eq!(tree["merge_tree"].as_str().map(str::len), Some(40));
        let gone = observe(&json!({"receipt_id": 8, "path": "/nonexistent/x"}), &mut HashSet::new());
        assert_eq!(gone["error"], "checkout missing on this host");
    }
}
