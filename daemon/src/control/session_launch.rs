//! Strong-operator convenience entry point; no remote MCP routing or argv input.
//! A durable intent precedes every effect. Unknown outcomes never auto-relaunch.
use super::{
    methods, operator,
    protocol::{ErrorCode, Request, Response},
};
use crate::state::DaemonState;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

// Only this operator surface is serialized. Never hold a daemon lock while
// waiting; concurrent calls return busy without starting work or consuming IDs.
static LAUNCH: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Params {
    request_id: String,
    task_id: String,
    engine: String,
    model: String,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    label: String,
    repo_path: PathBuf,
    #[serde(default)]
    base: Option<String>,
    #[serde(default)]
    managed_by_uid: Option<String>,
    #[serde(default)]
    global_perms: bool,
}

impl Params {
    fn engine(&self) -> &str {
        if self.engine == "claude" {
            "claude-code"
        } else {
            "codex"
        }
    }

    fn validate(&self) -> Result<(), (ErrorCode, String)> {
        let invalid = |message: &str| (ErrorCode::InvalidParams, message.to_string());
        let safe_id = |id: &str| {
            !id.is_empty()
                && id != "."
                && id != ".."
                && id.len() <= 128
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_:.".contains(&b))
        };
        if !safe_id(&self.request_id) || !safe_id(&self.task_id) {
            return Err(invalid(
                "request_id and task_id must be 1..128 ASCII letters/digits or -_:.",
            ));
        }
        if !matches!(self.engine.as_str(), "claude" | "codex") {
            return Err(invalid("engine must be claude or codex"));
        }
        let model = crate::models::validate_model(&self.model).map_err(|e| invalid(&e))?;
        crate::models::check_engine(self.engine(), &model).map_err(|e| invalid(&e))?;
        if let Some(effort) = &self.effort {
            crate::models::validate_effort(self.engine(), effort).map_err(|e| invalid(&e))?;
        }
        if self.label.trim().is_empty() || self.label.len() > 256 || self.label.contains('\0') {
            return Err(invalid(
                "label must be nonempty, at most 256 bytes, without NUL",
            ));
        }
        if !self.repo_path.is_absolute()
            || self.repo_path.as_os_str().as_encoded_bytes().contains(&0)
        {
            return Err(invalid(
                "repo_path must be an absolute local repository path",
            ));
        }
        if self
            .prompt
            .as_ref()
            .is_some_and(|p| p.len() > methods::MAX_SEND_INPUT_BYTES)
        {
            return Err(invalid("prompt exceeds the normal send_input byte cap"));
        }
        if self.base.as_ref().is_some_and(|b| {
            b.is_empty() || b.starts_with('-') || b.contains('\0') || b.len() > 256
        }) {
            return Err(invalid(
                "base must be a nonempty commit/ref, at most 256 bytes, without leading '-' or NUL",
            ));
        }
        if self
            .managed_by_uid
            .as_deref()
            .is_some_and(|id| !safe_id(id))
        {
            return Err(invalid("managed_by_uid is not a valid session identifier"));
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct Receipt {
    version: u32,
    request_id: String,
    fingerprint: String,
    uid: String,
    // No prompt/argv/credentials in receipts: retain the hash and small result.
    result: Option<Value>,
    error: Option<(ErrorCode, String)>,
}

fn internal(error: impl std::fmt::Display) -> (ErrorCode, String) {
    (
        ErrorCode::Internal,
        format!("session.launch receipt: {error}"),
    )
}

fn save(path: &Path, receipt: &Receipt) -> Result<(), (ErrorCode, String)> {
    let dir = path.parent().expect("receipt directory");
    std::fs::create_dir_all(dir).map_err(internal)?;
    // The new directory entry must survive a crash before its first receipt.
    std::fs::File::open(dir.parent().unwrap())
        .and_then(|f| f.sync_all())
        .map_err(internal)?;
    methods_receipt_write(path, receipt)
}

fn methods_receipt_write(path: &Path, receipt: &Receipt) -> Result<(), (ErrorCode, String)> {
    crate::state::write_json_atomic(
        path,
        &serde_json::to_string(receipt).map_err(internal)?,
        true,
    )
    .map_err(internal)
}

fn run_once(
    root: &Path,
    params: &Params,
    launch: impl FnOnce(&str) -> methods::MethodResult,
) -> methods::MethodResult {
    let bytes = serde_json::to_vec(params).map_err(internal)?;
    let fingerprint = format!("{:x}", Sha256::digest(&bytes));
    let path = root.join(format!(
        "{:x}.json",
        Sha256::digest(params.request_id.as_bytes())
    ));
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)
    {
        Ok(file) => Some(file),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(internal(e)),
    };
    if let Some(file) = file.as_mut() {
        if !file.metadata().map_err(internal)?.is_file() {
            return Err(internal("non-regular receipt"));
        }
        let mut bytes = Vec::new();
        file.take(1_048_577)
            .read_to_end(&mut bytes)
            .map_err(internal)?;
        if bytes.len() > 1_048_576 {
            return Err(internal("oversized receipt"));
        }
        let receipt: Receipt = serde_json::from_slice(&bytes).map_err(internal)?;
        if receipt.version != 1
            || receipt.request_id != params.request_id
            || receipt.fingerprint != fingerprint
        {
            return Err((ErrorCode::Conflict, "request_id already exists with different parameters or unsupported receipt version".into()));
        }
        if let Some(result) = receipt.result {
            return Ok(result);
        }
        if let Some(error) = receipt.error {
            return Err(error);
        }
        return Err((ErrorCode::Conflict, format!("launch outcome unknown for reserved UID {}; inspect that UID on this host; this request will not spawn or deliver a prompt again", receipt.uid)));
    }
    let mut receipt = Receipt {
        version: 1,
        request_id: params.request_id.clone(),
        fingerprint,
        uid: methods::new_daemon_minted_session_uid(),
        result: None,
        error: None,
    };
    save(&path, &receipt)?;
    let outcome = launch(&receipt.uid);
    match &outcome {
        Ok(value) => receipt.result = Some(value.clone()),
        Err(error) => receipt.error = Some(error.clone()),
    }
    save(&path, &receipt).map_err(|(_, e)| {
        (
            ErrorCode::Internal,
            format!(
                "{e}; launch may exist at UID {}; retry only the original request_id",
                receipt.uid
            ),
        )
    })?;
    outcome
}

pub(crate) fn dispatch(state: &Arc<Mutex<DaemonState>>, req: &Request) -> Response {
    if let Err((code, _)) = operator::require_strong_operator(&req.caller) {
        return Response::err(
            req.id.clone(),
            code,
            "session.launch requires a configured strong operator token",
        );
    }
    let result = launch(state, &req.params);
    match result {
        Ok(value) => Response::ok(req.id.clone(), value),
        Err((code, message)) => Response::err(req.id.clone(), code, message),
    }
}

pub(crate) fn launch(state: &Arc<Mutex<DaemonState>>, value: &Value) -> methods::MethodResult {
    let params: Params = serde_json::from_value(value.clone()).map_err(|e| {
        (
            ErrorCode::InvalidParams,
            format!("session.launch params: {e}"),
        )
    })?;
    params.validate()?;
    let _guard = match LAUNCH.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            return Err((
                ErrorCode::Conflict,
                "another operator launch is in progress; retry the same request_id".into(),
            ))
        }
    };
    let root = crate::config::default_config_path()
        .parent()
        .unwrap()
        .join("session-launch-requests");
    run_once(&root, &params, |uid| {
        let repo_path = crate::worktree::main_checkout_of(&params.repo_path)
            .and_then(|p| std::fs::canonicalize(p).ok())
            .ok_or((
                ErrorCode::InvalidParams,
                "repo_path is not a local git checkout".into(),
            ))?;
        let context = methods::OperatorLaunchContext {
            uid: uid.into(),
            repo_path,
            base: params.base.clone(),
            managed_by_uid: params.managed_by_uid.clone(),
        };
        let mut result = methods::operator_launch_agent(
            state,
            &json!({
                "type": params.engine(), "task_id": params.task_id, "label": params.label,
                "model": params.model, "effort": params.effort, "prompt": params.prompt,
                "global_perms": params.global_perms,
            }),
            &context,
        )?;
        result["uid"] = uid.into();
        result["request_id"] = params.request_id.clone().into();
        Ok(result)
    })
}

#[cfg(test)]
mod tests {
    use super::super::protocol::Caller;
    use super::*;

    fn routed(state: &Arc<Mutex<DaemonState>>, req: &Request) -> Response {
        match super::super::dispatch::dispatch_request(state, req) {
            super::super::dispatch::DispatchOutcome::Done(response) => response,
            _ => panic!("session.launch must be a non-streaming RPC"),
        }
    }

    fn params() -> Params {
        serde_json::from_value(json!({"request_id":"launch-1", "task_id":"task-1",
            "engine":"codex", "model":"gpt-6.1-terra", "label":"worker", "repo_path":"/fixture/repo"})).unwrap()
    }

    #[test]
    fn session_launch_receipt_survives_retry_and_conflicting_payload() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("receipts");
        let p = params();
        let first = run_once(&root, &p, |uid| {
            Ok(json!({"uid":uid,"task_id":"task-1","worktree_path":"/fixture/tree"}))
        })
        .unwrap();
        let replay = run_once(&root, &p, |_| panic!("duplicate spawn or prompt")).unwrap();
        assert_eq!(replay, first);
        // A fresh state object/process has no role in durable replay.
        let mut changed = p.clone();
        changed.prompt = Some("different prompt".into());
        assert_eq!(
            run_once(&root, &changed, |_| panic!("conflict spawned"))
                .unwrap_err()
                .0,
            ErrorCode::Conflict
        );
        let text = std::fs::read_to_string(
            std::fs::read_dir(&root)
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path(),
        )
        .unwrap();
        assert!(!text.contains("different prompt"));
        assert_eq!(std::fs::read_dir(root).unwrap().count(), 1);
    }

    #[test]
    fn session_launch_crash_after_intent_and_terminal_error_never_relaunch() {
        let tmp = tempfile::tempdir().unwrap();
        let p = params();
        let crashed = std::panic::catch_unwind(|| {
            let _ = run_once(tmp.path(), &p, |_| {
                panic!("crash after intent, possibly after spawn")
            });
        });
        assert!(crashed.is_err());
        let error = run_once(tmp.path(), &p, |_| panic!("uncertain outcome repeated")).unwrap_err();
        assert_eq!(error.0, ErrorCode::Conflict);
        assert!(error.1.contains("outcome unknown"));
        assert!(error.1.contains("reserved UID ts-"));
        let mut other = p;
        other.request_id = "known-failure".into();
        let error = run_once(tmp.path(), &other, |_| {
            Err((ErrorCode::Internal, "after spawn failure".into()))
        })
        .unwrap_err();
        assert_eq!(
            run_once(tmp.path(), &other, |_| panic!("terminal failure repeated")).unwrap_err(),
            error
        );
    }

    #[test]
    fn session_launch_bad_receipt_is_fail_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let p = params();
        let path = tmp.path().join(format!(
            "{:x}.json",
            Sha256::digest(p.request_id.as_bytes())
        ));
        std::fs::write(&path, "not json").unwrap();
        assert!(run_once(tmp.path(), &p, |_| panic!("corruption repeated spawn")).is_err());
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("missing"), &path).unwrap();
        assert!(run_once(tmp.path(), &p, |_| panic!(
            "dangling receipt treated absent"
        ))
        .is_err());
        std::fs::remove_file(&path).unwrap();
        let cpath = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
        assert!(run_once(tmp.path(), &p, |_| panic!("FIFO receipt repeated spawn")).is_err());
    }

    #[test]
    fn session_launch_strong_auth_schema_and_busy_precede_effects() {
        let _env = crate::test_support::env_lock();
        let state = Arc::new(Mutex::new(DaemonState::default()));
        let mut req = Request {
            id: "frame".into(),
            method: "session.launch".into(),
            caller: Caller::operator("token"),
            params: serde_json::to_value(params()).unwrap(),
        };
        {
            let _auth = operator::test_override::set(None);
            assert_eq!(
                routed(&state, &req).error.unwrap().code,
                ErrorCode::Unauthorized
            );
            assert!(
                operator::validate_operator(&req.caller).is_ok(),
                "ordinary operator compatibility unchanged"
            );
        }
        let _auth = operator::test_override::set(Some("token"));
        for caller in [
            Caller::session("parent"),
            Caller::operator("wrong"),
            Caller::operator("cm-board-v1:wrong"),
        ] {
            req.caller = caller;
            assert_eq!(
                routed(&state, &req).error.unwrap().code,
                ErrorCode::Unauthorized
            );
        }
        req.caller = Caller::operator("token");
        for (field, value) in [
            ("engine", json!("bash")),
            ("request_id", json!("../escape")),
            ("task_id", json!("..")),
            ("model", json!("claude-opus-4-6")),
            ("repo_path", json!("relative")),
            ("argv", json!(["/bin/bash"])),
        ] {
            req.params = serde_json::to_value(params()).unwrap();
            req.params[field] = value;
            assert_eq!(
                routed(&state, &req).error.unwrap().code,
                ErrorCode::InvalidParams,
                "{field}"
            );
        }
        req.params = serde_json::to_value(params()).unwrap();
        let _held = LAUNCH.lock().unwrap_or_else(|p| p.into_inner());
        assert_eq!(
            routed(&state, &req).error.unwrap().code,
            ErrorCode::Conflict
        );
        assert!(super::super::dispatch::method_counts_as_restart_mutation(
            "session.launch"
        ));
    }
}
