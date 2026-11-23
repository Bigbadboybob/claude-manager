//! Operator-only account usage snapshot. Runs on the per-connection worker;
//! never holds daemon state while the external hub command is running.
use crate::control::{
    operator,
    protocol::{ErrorCode, Request, Response},
};
use serde_json::Value;
use std::{
    collections::HashMap,
    io::Read,
    path::Path,
    process::{Command, Stdio},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Params {
    since: Option<u64>,
    granularity: Option<String>,
}
fn params(v: &Value) -> Result<Params, &'static str> {
    if v.is_null() {
        return Ok(Params {
            since: None,
            granularity: None,
        });
    }
    let o = v.as_object().ok_or("expected an object")?;
    if o.keys().any(|k| k != "since" && k != "granularity") {
        return Err("unknown parameter");
    }
    let since = match o.get("since") {
        None => None,
        Some(v) => Some(
            v.as_u64()
                .ok_or("since must be a non-negative integer epoch")?,
        ),
    };
    let granularity = match o.get("granularity") {
        None => None,
        Some(v) => match v.as_str() {
            Some(s @ ("hour" | "day")) => Some(s.to_owned()),
            _ => return Err("granularity must be hour or day"),
        },
    };
    Ok(Params { since, granularity })
}
static CACHE: OnceLock<Mutex<HashMap<Params, (Instant, Value)>>> = OnceLock::new();

pub(crate) fn validate(req: &Request) -> Result<(), Response> {
    operator::validate_operator(&req.caller)
        .map_err(|e| Response::err(req.id.clone(), ErrorCode::Unauthorized, e))?;
    params(&req.params).map_err(|e| Response::err(req.id.clone(), ErrorCode::InvalidParams, e))?;
    Ok(())
}

pub(crate) fn dispatch(req: &Request) -> Response {
    if let Err(e) = operator::validate_operator(&req.caller) {
        return Response::err(req.id.clone(), ErrorCode::Unauthorized, e);
    }
    let p = match params(&req.params) {
        Ok(p) => p,
        Err(e) => return Response::err(req.id.clone(), ErrorCode::InvalidParams, e),
    };
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return Response::err(
            req.id.clone(),
            ErrorCode::Internal,
            "home directory unavailable",
        );
    };
    match snapshot(&home, &p) {
        Ok(v) => Response::ok(req.id.clone(), v),
        Err((code, e)) => Response::err(req.id.clone(), code, e),
    }
}
fn snapshot(home: &Path, p: &Params) -> Result<Value, (ErrorCode, String)> {
    let binary = home.join(".local/bin/cm-claude-accounts");
    if !binary.is_file() || !home.join(".config/cm-claude-accounts/hub").is_dir() {
        return Err((
            ErrorCode::NotHub,
            "not_hub: account hub or command is not installed on this host".into(),
        ));
    }
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some((at, value)) = cache.lock().unwrap().get(p) {
        if at.elapsed() < Duration::from_secs(30) {
            return Ok(value.clone());
        }
    }
    let mut cmd = Command::new("python3");
    cmd.arg(binary).args(["hub", "usage", "--json"]);
    if let Some(s) = p.since {
        cmd.arg("--since").arg(s.to_string());
    }
    if let Some(g) = &p.granularity {
        cmd.arg("--granularity").arg(g);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| (ErrorCode::Internal, format!("usage command: {e}")))?;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let (out_tx, out_rx) = std::sync::mpsc::channel();
    let (err_tx, err_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = stdout.by_ref().take(16 * 1024 * 1024).read_to_end(&mut b);
        let _ = out_tx.send(b);
    });
    std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = stderr.by_ref().take(65536).read_to_end(&mut b);
        let _ = err_tx.send(b);
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() < Duration::from_secs(15) => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err((
                    ErrorCode::Internal,
                    "usage command timed out after 15s".into(),
                ));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err((ErrorCode::Internal, format!("usage command: {e}")));
            }
        }
    };
    let remaining = || Duration::from_secs(15).saturating_sub(start.elapsed());
    let timeout = || {
        (
            ErrorCode::Internal,
            "usage command timed out after 15s".to_owned(),
        )
    };
    let bytes = out_rx.recv_timeout(remaining()).map_err(|_| timeout())?;
    let errors = err_rx.recv_timeout(remaining()).map_err(|_| timeout())?;
    if !status.success() {
        return Err((
            ErrorCode::Internal,
            format!(
                "usage command failed: {}",
                String::from_utf8_lossy(&errors).trim()
            ),
        ));
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|e| {
        (
            ErrorCode::Internal,
            format!("usage command returned invalid JSON: {e}"),
        )
    })?;
    let mut c = cache.lock().unwrap();
    c.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(30));
    c.insert(p.clone(), (Instant::now(), value.clone()));
    Ok(value)
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn validates_arguments() {
        assert!(params(&json!({"since": 42, "granularity":"day"})).is_ok());
        for v in [
            json!({"since":-1}),
            json!({"since":"1"}),
            json!({"granularity":"day;id"}),
            json!({"extra":true}),
            json!([]),
        ] {
            assert!(params(&v).is_err(), "{v}");
        }
    }
    #[test]
    fn fixed_argv_returns_json_and_caches_by_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(dir.path().join(".config/cm-claude-accounts/hub")).unwrap();
        let script = bin.join("cm-claude-accounts");
        std::fs::write(&script, "import json, sys\nprint(json.dumps({'args':sys.argv[1:], 'extra':{'preserved':True}}))\n").unwrap();
        let p = params(&json!({"since":123456789, "granularity":"day"})).unwrap();
        let result = snapshot(dir.path(), &p).unwrap();
        assert_eq!(
            result,
            json!({"args":["hub","usage","--json","--since","123456789","--granularity","day"], "extra":{"preserved":true}})
        );
        std::fs::write(&script, "raise Exception('cache missed')").unwrap();
        assert_eq!(snapshot(dir.path(), &p).unwrap(), result);
        assert!(snapshot(
            dir.path(),
            &params(&json!({"since":123456790, "granularity":"day"})).unwrap()
        )
        .is_err());
    }
    #[test]
    fn session_callers_are_denied() {
        let req = Request {
            id: "test".into(),
            caller: crate::control::protocol::Caller::session("x"),
            method: "usage.snapshot".into(),
            params: json!({}),
        };
        assert_eq!(dispatch(&req).error.unwrap().code, ErrorCode::Unauthorized);
    }
    #[test]
    fn dispatcher_returns_before_blocking_command_completes() {
        use crate::control::dispatch::{dispatch_request, DispatchOutcome};
        let _guard = crate::test_support::env_lock();
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(home.path().join(".config/cm-claude-accounts/hub")).unwrap();
        std::fs::write(bin.join("cm-claude-accounts"), "import pathlib,time\np=pathlib.Path(__file__).parent/'go'\nwhile not p.exists(): time.sleep(.01)\nprint('voluntary')\n").unwrap();
        let old = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", home.path());
        }
        let state = std::sync::Arc::new(Mutex::new(crate::state::DaemonState::new()));
        let req = Request {
            id: "deferred".into(),
            caller: crate::control::protocol::Caller::operator("test"),
            method: "usage.snapshot".into(),
            params: json!({"since":345678901}),
        };
        let outcome = dispatch_request(&state, &req);
        assert!(matches!(outcome, DispatchOutcome::Deferred(_)));
        std::fs::write(bin.join("go"), "go").unwrap();
        let response = outcome.into_response();
        assert!(!response.ok); // the fake intentionally emits non-JSON
        match old {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }
    #[test]
    fn missing_hub_does_not_run_binary() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("cm-claude-accounts"), "raise Exception('ran')").unwrap();
        assert_eq!(
            snapshot(dir.path(), &params(&json!({})).unwrap())
                .unwrap_err()
                .0,
            ErrorCode::NotHub
        );
    }
}
