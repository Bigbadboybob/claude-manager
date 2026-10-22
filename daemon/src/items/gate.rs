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
/// No `attestation`: it is never the poster's to claim (doc §13).
const RECEIPT_FIELDS: &[&str] = &[
    "gate", "klass", "head", "mf", "runner", "ruling", "base", "closure", "worktree", "inputs",
    "steps", "waivers", "log_path", "repo", "branch",
];
/// Input fields only the daemon may set; anything the poster sends under
/// these names is dropped.
const DAEMON_INPUT_FIELDS: &[&str] = &["daemon_hash", "verification", "hashed_by", "key", "poster_content_id"];

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
    if let Some(inputs) = receipt.get_mut("inputs").and_then(Value::as_array_mut) {
        for input in inputs.iter_mut() {
            stamp_input(input);
        }
    }
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

/// Drop the poster's verification claims, then rehash a local file/dir
/// input from its resolved path on this host (the only way an input is
/// `hashed_at_run`). Runs without any lock held.
pub(crate) fn stamp_input(input: &mut Value) {
    let Some(obj) = input.as_object_mut() else { return };
    for key in DAEMON_INPUT_FIELDS {
        obj.remove(*key);
    }
    let kind = obj.get("kind").and_then(Value::as_str).unwrap_or_default();
    if !matches!(kind, "file" | "dir" | "artifact") {
        return;
    }
    let Some(path) = obj.get("resolved_path").and_then(Value::as_str).map(std::path::PathBuf::from) else {
        return;
    };
    if !path.is_absolute() {
        return;
    }
    let hash = match content_hash(&path) {
        Some((content_id, bytes)) => json!({"content_id": content_id, "bytes": bytes}),
        None => json!({"missing": true}),
    };
    obj.insert("daemon_hash".into(), hash);
}

/// `sha256:<hex>` for a file; for a directory `tree:<sha256 of the sorted
/// (relpath, size, sha256) lines>`, the same manifest cm-gate computes
/// (files of a directory first, then its subdirectories, symlinks skipped).
pub(crate) fn content_hash(path: &Path) -> Option<(String, u64)> {
    use sha2::{Digest, Sha256};
    fn file_sha(path: &Path) -> Option<String> {
        let mut file = std::fs::File::open(path).ok()?;
        let mut hasher = Sha256::new();
        std::io::copy(&mut file, &mut hasher).ok()?;
        Some(format!("{:x}", hasher.finalize()))
    }
    fn walk(root: &Path, dir: &Path, lines: &mut Vec<String>, total: &mut u64) -> Option<()> {
        let mut entries: Vec<_> = std::fs::read_dir(dir).ok()?.filter_map(Result::ok).collect();
        entries.sort_by_key(|e| e.file_name());
        let mut subdirs = vec![];
        for e in entries {
            let ft = e.file_type().ok()?;
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                subdirs.push(e.path());
            } else if ft.is_file() {
                let p = e.path();
                let size = e.metadata().ok()?.len();
                *total += size;
                let rel = p.strip_prefix(root).ok()?.to_string_lossy().replace('\\', "/");
                lines.push(format!("{rel}\t{size}\t{}", file_sha(&p)?));
            }
        }
        for d in subdirs {
            walk(root, &d, lines, total)?;
        }
        Some(())
    }
    let meta = std::fs::symlink_metadata(path).ok()?;
    if meta.is_file() {
        return Some((format!("sha256:{}", file_sha(path)?), meta.len()));
    }
    if !meta.is_dir() {
        return None;
    }
    let (mut lines, mut total) = (vec![], 0u64);
    walk(path, path, &mut lines, &mut total)?;
    let digest = Sha256::digest(lines.join("\n").as_bytes());
    Some((format!("tree:{digest:x}"), total))
}

/// `{found, sender, contains_head, contains_base?, contains_step, contains_kind}`
/// for a linked message.
pub(crate) fn message_check(
    state: &Arc<Mutex<DaemonState>>,
    message_id: &str,
    head: &str,
    base: Option<&str>,
    step: &str,
    kind: &str,
) -> Value {
    let handle = state.lock().unwrap_or_else(|p| p.into_inner()).messaging.clone();
    let found = {
        let slot = handle.lock().unwrap_or_else(|p| p.into_inner());
        slot.as_ref().and_then(|store| store.message_sender_and_body(message_id))
    };
    check_body(found, head, base, step, kind)
}

/// Is `needle` a whole token of `hay` (no word character on either side)?
fn has_token(hay: &str, needle: &str, word: impl Fn(char) -> bool) -> bool {
    if needle.is_empty() {
        return false;
    }
    hay.match_indices(needle).any(|(i, _)| {
        let before = hay[..i].chars().next_back().is_none_or(|c| !word(c));
        let after = hay[i + needle.len()..].chars().next().is_none_or(|c| !word(c));
        before && after
    })
}

/// The message must name the full 40-character head (and base) SHA as a
/// token, the step, and the kind of grant.
pub(crate) fn check_body(found: Option<(String, String)>, head: &str, base: Option<&str>, step: &str, kind: &str) -> Value {
    let Some((sender, body)) = found else {
        return json!({"found": false});
    };
    let lower = body.to_lowercase();
    let sha = |s: &str| {
        let s = s.trim().to_lowercase();
        s.len() == 40 && has_token(&lower, &s, |c| c.is_ascii_alphanumeric())
    };
    let ident = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.');
    let kind_word = if kind == "variance" { "variance" } else { "waive" };
    let contains_kind = lower.match_indices(kind_word).any(|(i, _)| {
        lower[..i].chars().next_back().is_none_or(|c| !c.is_ascii_alphanumeric())
    });
    let mut out = json!({
        "found": true,
        "sender": sender,
        "contains_head": sha(head),
        "contains_step": has_token(&lower, &step.trim().to_lowercase(), ident),
        "contains_kind": contains_kind,
    });
    if let Some(base) = base {
        out["contains_base"] = json!(sha(base));
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
        let check = message_check(
            state,
            msg,
            head,
            g["base"].as_str(),
            g["step"].as_str().unwrap_or_default(),
            g["kind"].as_str().unwrap_or("waiver"),
        );
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
    let kind = str_param(params, "kind").unwrap_or("waiver");
    let step = str_param(params, "step").ok_or_else(|| bad("step is required"))?;
    let mut body = json!({
        "actor": ctx.actor(),
        "kind": kind,
        "gate": str_param(params, "gate").unwrap_or("gate"),
        "head": head,
        "step": step,
        "reason": params["reason"],
        "risk": params["risk"],
        "message_id": message_id,
        "message_check": message_check(state, message_id, head, base, step, kind),
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

/// Every checkout CM knows on this host: workspaces, TUI-pushed sessions
/// and recently exited sessions. The refresh runs git only in these.
fn registered_worktrees(s: &DaemonState) -> HashSet<std::path::PathBuf> {
    let mut out = HashSet::new();
    let mut add = |p: &Path| {
        if let Ok(c) = std::fs::canonicalize(p) {
            out.insert(c);
        }
    };
    for ws in s.workspaces.values() {
        if let Some(p) = &ws.worktree_path {
            add(p);
        }
    }
    for t in s.tui_sessions.values() {
        if let Some(p) = &t.worktree_path {
            add(Path::new(p));
        }
    }
    for t in &s.recently_exited {
        if let Some(p) = &t.worktree_path {
            add(Path::new(p));
        }
    }
    out
}

fn refresh_snapshot(state: &Arc<Mutex<DaemonState>>) -> Option<(String, super::api::Api, HashSet<std::path::PathBuf>)> {
    let (handle, api_url, api_token, registered) = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        (s.messaging.clone(), s.config.api_url.clone(), s.config.api_token.clone(), registered_worktrees(&s))
    };
    let daemon_id = {
        let slot = handle.lock().unwrap_or_else(|p| p.into_inner());
        slot.as_ref()?.daemon_id.clone()
    };
    let api = super::api::Api::from_config(&api_url, &api_token).ok()?;
    Some((daemon_id, api.with_timeout(Duration::from_secs(20)), registered))
}

/// One pass: ask which of this host's receipts to watch, observe them with
/// git, report. Best-effort; failures wait for the next pass.
pub(crate) fn refresh_once(state: &Arc<Mutex<DaemonState>>) {
    let Some((daemon_id, api, registered)) = refresh_snapshot(state) else { return };
    let Ok(reply) = api.post("/receipts/refresh", &json!({"daemon_id": daemon_id, "checks": []})) else {
        return;
    };
    let watch = reply["watch"].as_array().cloned().unwrap_or_default();
    if watch.is_empty() {
        return;
    }
    let mut fetched = HashSet::new();
    let checks: Vec<Value> = watch.iter().map(|w| observe(w, &registered, &mut fetched)).collect();
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
    git(dir, &["rev-parse", "--verify", "--quiet", "--end-of-options", rev]).filter(|s| s.len() == 40)
}

/// A ref that cannot be read as a git option: plain characters, no leading
/// `-`, no `..`, and the ref part accepted by `git check-ref-format`.
/// `origin/main:path` (a runner ref) checks the path too.
pub(crate) fn safe_ref(dir: &Path, reference: &str) -> bool {
    let (refname, path) = match reference.split_once(':') {
        Some((r, p)) => (r, Some(p)),
        None => (reference, None),
    };
    let plain = |s: &str| {
        !s.is_empty()
            && !s.starts_with('-')
            && !s.contains("..")
            && s.chars().all(|c| c.is_ascii_alphanumeric() || "._/@+-".contains(c))
    };
    if !plain(refname) || path.is_some_and(|p| !plain(p)) {
        return false;
    }
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["check-ref-format", "--allow-onelevel", refname])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Observe one watched receipt in its checkout on this host. Only a
/// registered CM worktree is touched, and only with validated refs.
pub(crate) fn observe(w: &Value, registered: &HashSet<std::path::PathBuf>, fetched: &mut HashSet<(String, String)>) -> Value {
    let id = w["receipt_id"].clone();
    let Some(path) = w["path"].as_str().map(Path::new).filter(|p| p.is_dir()) else {
        return json!({"receipt_id": id, "error": "checkout missing on this host"});
    };
    if !std::fs::canonicalize(path).is_ok_and(|c| registered.contains(&c)) {
        return json!({"receipt_id": id, "error": "not a registered CM worktree; refresh skipped"});
    }
    let mut out = json!({"receipt_id": id});
    let base_ref = w["base_ref"].as_str().filter(|r| safe_ref(path, r));
    let runner_ref = w["runner_ref"].as_str().filter(|r| safe_ref(path, r));
    let head = w["head"].as_str().filter(|h| h.len() == 40 && h.chars().all(|c| c.is_ascii_hexdigit()));
    for reference in [base_ref, runner_ref].into_iter().flatten() {
        // `origin/main` or `origin/main:path`: fetch origin's main once per checkout.
        let refname = reference.split(':').next().unwrap_or(reference);
        if let Some((remote, branch)) = refname.split_once('/') {
            let key = (path.display().to_string(), refname.to_string());
            if fetched.insert(key) {
                let _ = git(path, &["fetch", "--quiet", "--no-tags", "--end-of-options", remote, branch]);
            }
        }
    }
    if let Some(branch) = w["branch"].as_str().filter(|b| safe_ref(path, b)) {
        if let Some(head) = rev(path, &format!("refs/heads/{branch}")) {
            out["branch_head"] = json!(head);
        }
    }
    if let Some(runner_ref) = runner_ref {
        if let Some(sha) = rev(path, runner_ref) {
            out["runner_sha"] = json!(sha);
        }
    }
    let (Some(base_ref), Some(head)) = (base_ref, head) else {
        return out;
    };
    let Some(base_now) = rev(path, base_ref) else { return out };
    out["base_sha"] = json!(base_now);
    let before = w["base_sha_after"]
        .as_str()
        .filter(|h| h.len() == 40 && h.chars().all(|c| c.is_ascii_hexdigit()))
        .unwrap_or_default();
    if base_now == before || before.is_empty() {
        return out;
    }
    if w["scope"] == "tree" {
        match Command::new("timeout")
            .arg(GIT_TIMEOUT_S.to_string())
            .args(["git", "-C"])
            .arg(path)
            .args(["merge-tree", "--write-tree", "--end-of-options", &base_now, head])
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
    } else if let Some(diff) = git(path, &["diff", "--name-only", "--end-of-options", before, &base_now, "--"]) {
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
    fn message_checks_need_the_full_sha_the_step_and_the_kind() {
        let head = "a1b2c3d4e5f6a7b8c9d0a1b2c3d4e5f6a7b8c9d0";
        let base = "0".repeat(40);
        let ep = |body: String| Some(("agent:d:ep".to_string(), body));
        assert_eq!(check_body(None, head, None, "mypy", "waiver"), json!({"found": false}));
        let ok = check_body(ep(format!("WAIVE mypy on {head}: stubs broken; risk: type errors")), head, None, "mypy", "waiver");
        assert_eq!(ok, json!({"found": true, "sender": "agent:d:ep", "contains_head": true,
                              "contains_step": true, "contains_kind": true}));
        let prefix = check_body(ep(format!("WAIVE mypy on {}", &head[..12])), head, None, "mypy", "waiver");
        assert_eq!(prefix["contains_head"], false, "a 12-char prefix no longer binds");
        let longer = check_body(ep(format!("WAIVE mypy on {head}ff")), head, None, "mypy", "waiver");
        assert_eq!(longer["contains_head"], false, "the SHA must be a whole token");
        let other_step = check_body(ep(format!("WAIVE mypy-strict on {head}")), head, None, "mypy", "waiver");
        assert_eq!(other_step["contains_step"], false, "mypy-strict is not mypy");
        let no_kind = check_body(ep(format!("mypy on {head} is fine")), head, None, "mypy", "waiver");
        assert_eq!(no_kind["contains_kind"], false);
        let variance = check_body(ep(format!("variance for pytest on {base}→{head}")), head, Some(&base), "pytest", "variance");
        assert_eq!((variance["contains_head"].clone(), variance["contains_base"].clone(), variance["contains_kind"].clone()),
                   (json!(true), json!(true), json!(true)));
        let half = check_body(ep(format!("variance for pytest on {head}")), head, Some(&base), "pytest", "variance");
        assert_eq!(half["contains_base"], false, "a base→head grant names both");
    }

    #[test]
    fn stamped_inputs_carry_only_the_daemons_own_hash() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("b.json"), "[1]").unwrap();
        std::fs::write(dir.path().join("sub/a.txt"), "hi").unwrap();
        let mut forged = json!({"name": "h", "kind": "dir", "resolved_path": dir.path(),
                                "content_id": "tree:00", "verification": "hashed_at_run",
                                "hashed_by": "cm-gate", "daemon_hash": {"content_id": "tree:00"}});
        stamp_input(&mut forged);
        assert!(forged.get("verification").is_none() && forged.get("hashed_by").is_none());
        // The documented manifest: files of a directory, then its subdirectories.
        use sha2::{Digest, Sha256};
        let sha = |b: &[u8]| format!("{:x}", Sha256::digest(b));
        let lines = format!("b.json\t3\t{}\nsub/a.txt\t2\t{}", sha(b"[1]"), sha(b"hi"));
        assert_eq!(forged["daemon_hash"], json!({"content_id": format!("tree:{}", sha(lines.as_bytes())), "bytes": 5}));
        let mut file = json!({"name": "f", "kind": "file", "resolved_path": dir.path().join("b.json")});
        stamp_input(&mut file);
        assert_eq!(file["daemon_hash"]["content_id"], format!("sha256:{}", sha(b"[1]")));
        let mut gone = json!({"name": "h", "kind": "dir", "resolved_path": dir.path().join("harvest"),
                              "content_id": format!("tree:{}", "0".repeat(64))});
        stamp_input(&mut gone);
        assert_eq!(gone["daemon_hash"], json!({"missing": true}), "a typed hash never stands in");
        let mut db = json!({"name": "t", "kind": "db", "content_id": "snapshot:x", "daemon_hash": {"content_id": "x"}});
        stamp_input(&mut db);
        assert!(db.get("daemon_hash").is_none(), "only file/dir inputs are rehashed");
        let mut relative = json!({"name": "r", "kind": "file", "resolved_path": "b.json"});
        stamp_input(&mut relative);
        assert!(relative.get("daemon_hash").is_none());
    }

    #[test]
    fn refs_that_look_like_options_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        for bad_ref in ["-x", "--upload-pack=evil", "main..x", "a b", "x;y", "origin/main:../x/-y", ""] {
            assert!(!safe_ref(dir.path(), bad_ref), "{bad_ref}");
        }
        assert!(safe_ref(dir.path(), "origin/main"));
        assert!(safe_ref(dir.path(), "origin/main:scripts/gate.sh"));
        assert!(!safe_ref(dir.path(), "origin/main.lock"), "git check-ref-format runs too");
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
        let unregistered = observe(&w, &HashSet::new(), &mut HashSet::new());
        assert_eq!(unregistered["error"], "not a registered CM worktree; refresh skipped");
        let registered: HashSet<_> = [std::fs::canonicalize(p).unwrap()].into();
        let out = observe(&w, &registered, &mut HashSet::new());
        assert_eq!(out["branch_head"], json!(head));
        assert_eq!(out["changed_paths"], json!(["docs.md"]));
        assert_eq!(out["runner_sha"].as_str().map(str::len), Some(40));
        let tree = observe(&json!({"receipt_id": 7, "path": p, "head": head, "base_ref": "main",
                                    "base_sha_after": base, "scope": "tree"}), &registered, &mut HashSet::new());
        assert_eq!(tree["merge_tree"].as_str().map(str::len), Some(40));
        let gone = observe(&json!({"receipt_id": 8, "path": "/nonexistent/x"}), &registered, &mut HashSet::new());
        let injected = observe(&json!({"receipt_id": 9, "path": p, "head": head, "base_ref": "--output=/tmp/x",
                                       "runner_ref": "-c", "base_sha_after": base}), &registered, &mut HashSet::new());
        assert!(injected.get("base_sha").is_none() && injected.get("runner_sha").is_none(),
                "option-shaped refs never reach git: {injected}");
        assert_eq!(gone["error"], "checkout missing on this host");
    }
}
