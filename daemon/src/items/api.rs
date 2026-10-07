//! Planning-API calls for items and boards (doc/items-board.md §6).

use crate::control::protocol::ErrorCode;
use serde_json::Value;
use std::time::Duration;

/// Fail fast: there is no local queue, and an agent should learn quickly
/// that cm-manager is unreachable.
const TIMEOUT: Duration = Duration::from_secs(8);

pub type ApiResult = Result<Value, (ErrorCode, String)>;

#[derive(Clone, Debug)]
pub struct Api {
    base_url: String,
    token: String,
    timeout: Duration,
}

impl Api {
    /// `api_url` / `api_token` are `state.config` values snapshotted under the
    /// lock; empty values fall back to `CM_API_URL` / `CM_API_TOKEN`.
    pub fn from_config(api_url: &str, api_token: &str) -> Result<Self, (ErrorCode, String)> {
        let base_url = crate::planning_client::resolve_api_url(Some(api_url))
            .map_err(|e| e.to_method_err())?;
        let token = crate::planning_client::resolve_api_token(Some(api_token))
            .map_err(|e| e.to_method_err())?;
        Ok(Api { base_url, token, timeout: TIMEOUT })
    }

    /// The same API with a shorter overall timeout, for best-effort calls.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn agent(&self) -> ureq::Agent {
        ureq::Agent::new_with_config(
            ureq::config::Config::builder()
                .timeout_global(Some(self.timeout))
                .http_status_as_error(false)
                .build(),
        )
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn auth(&self) -> String {
        format!("Bearer {}", self.token)
    }

    pub fn get(&self, path: &str, query: &[(&str, String)]) -> ApiResult {
        let mut request = self.agent().get(&self.url(path)).header("Authorization", &self.auth());
        for (key, value) in query {
            request = request.query(*key, value);
        }
        finish(request.call(), path)
    }

    pub fn post(&self, path: &str, body: &Value) -> ApiResult {
        finish(
            self.agent()
                .post(&self.url(path))
                .header("Authorization", &self.auth())
                .send_json(body),
            path,
        )
    }

    pub fn patch(&self, path: &str, body: &Value) -> ApiResult {
        finish(
            self.agent()
                .patch(&self.url(path))
                .header("Authorization", &self.auth())
                .send_json(body),
            path,
        )
    }
}

fn finish(
    result: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    path: &str,
) -> ApiResult {
    let mut response = result.map_err(|e| {
        (
            ErrorCode::Internal,
            format!("planning_api_unavailable: {path}: {e} (items live on the cm-manager planning API; retry later)"),
        )
    })?;
    let status = response.status().as_u16();
    let body = response.body_mut().read_to_string().map_err(|e| {
        (ErrorCode::Internal, format!("planning_api_unavailable: {path}: reading reply: {e}"))
    })?;
    if (200..300).contains(&status) {
        return serde_json::from_str(&body)
            .map_err(|e| (ErrorCode::Internal, format!("planning API reply for {path}: {e}")));
    }
    Err(map_error(status, &body))
}

/// Map an API error reply to a daemon error. Item errors carry
/// `{"detail": {"code", "message", …}}`; FastAPI validation errors carry
/// `{"detail": [{"loc", "msg"}, …]}`.
pub fn map_error(status: u16, body: &str) -> (ErrorCode, String) {
    let code = match status {
        404 => ErrorCode::NotFound,
        409 => ErrorCode::Conflict,
        400 | 422 => ErrorCode::InvalidParams,
        _ => ErrorCode::Internal,
    };
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let detail = &parsed["detail"];
    let message = if let Some(message) = detail["message"].as_str() {
        match detail["code"].as_str() {
            Some(c) => format!("{c}: {message}"),
            None => message.to_string(),
        }
    } else if let Some(errors) = detail.as_array() {
        let parts: Vec<String> = errors
            .iter()
            .map(|e| {
                let loc: Vec<String> = e["loc"]
                    .as_array()
                    .map(|l| l.iter().skip(1).map(|p| p.to_string().trim_matches('"').to_string()).collect())
                    .unwrap_or_default();
                format!("{}: {}", loc.join("."), e["msg"].as_str().unwrap_or("invalid"))
            })
            .collect();
        format!("invalid_field: {}", parts.join("; "))
    } else if let Some(text) = detail.as_str() {
        text.to_string()
    } else if status == 401 {
        "the daemon's planning API token was rejected".to_string()
    } else {
        format!("planning API returned {status}: {}", body.chars().take(300).collect::<String>())
    };
    (code, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_errors_keep_code_and_message() {
        let (code, msg) = map_error(409, r#"{"detail":{"code":"cycle","message":"cycle: 1→2→1","cycle":[1,2,1]}}"#);
        assert!(matches!(code, ErrorCode::Conflict));
        assert_eq!(msg, "cycle: cycle: 1→2→1");
        let (code, msg) = map_error(422, r#"{"detail":{"code":"eta_required","message":"waiting requires eta"}}"#);
        assert!(matches!(code, ErrorCode::InvalidParams));
        assert_eq!(msg, "eta_required: waiting requires eta");
        let (code, _) = map_error(404, r#"{"detail":{"code":"not_found","message":"no item #9"}}"#);
        assert!(matches!(code, ErrorCode::NotFound));
    }

    #[test]
    fn validation_lists_and_bare_errors_are_readable() {
        let (_, msg) = map_error(
            422,
            r#"{"detail":[{"loc":["body","ns",0],"msg":"Input should be less than or equal to 2147483647"}]}"#,
        );
        assert_eq!(msg, "invalid_field: ns.0: Input should be less than or equal to 2147483647");
        let (code, msg) = map_error(500, r#"{"error":"internal_error"}"#);
        assert!(matches!(code, ErrorCode::Internal));
        assert!(msg.contains("500"));
        let (_, msg) = map_error(401, r#"{"detail":"Invalid token"}"#);
        assert_eq!(msg, "Invalid token");
    }
}
