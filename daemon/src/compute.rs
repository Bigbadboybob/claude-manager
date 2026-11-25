//! Session-authenticated proxy for the planning API's compute lease service.
use std::sync::{Arc, Mutex};
use serde_json::{Map, Value};
use crate::control::protocol::{Caller, ErrorCode, Request, Response};
use crate::state::DaemonState;

pub fn dispatch(state: &Arc<Mutex<DaemonState>>, req: &Request) -> Response {
    let result = (|| {
        match &req.caller {
            Caller::Session(_) => (),
            _ => return Err((ErrorCode::Unauthorized, "compute leases require a session caller".into())),
        }
        let ctx = crate::items::rpc::Ctx::capture(state, req)?;
        let mut body: Map<String, Value> = req.params.as_object().cloned()
            .ok_or((ErrorCode::InvalidParams, "expected object parameters".into()))?;
        // Always replace the untrusted parameter with the daemon's identity.
        body.insert("actor".into(), ctx.actor());
        let path = match req.method.as_str() {
            "compute.request" => "/compute/request",
            "compute.extend" => "/compute/extend",
            "compute.release" => "/compute/release",
            "compute.status" => "/compute/status",
            _ => return Err((ErrorCode::UnknownMethod, "unknown compute method".into())),
        };
        ctx.api.post(path, &Value::Object(body))
    })();
    match result {
        Ok(value) => Response::ok(req.id.clone(), value),
        Err((code, message)) => Response::err(req.id.clone(), code, message),
    }
}
