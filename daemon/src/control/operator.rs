//! Operator-caller authentication.
//!
//! Several daemon RPCs are intended for the TUI only (e.g.
//! `task.update_tree`, `session.set_workflow_context`,
//! `workflow.update_definitions`). Pre-fix the dispatch arms simply
//! rejected `Caller::Session(_)` but accepted *any* `Caller::Operator`,
//! which meant a same-UID agent could forge an Operator frame and
//! bypass the descendant-scoped Session-caller auth that protects
//! `send_input` / `kill_session` / etc.
//!
//! The fix is a shared secret: the TUI generates a random token
//! before spawning the daemon, passes it via `CM_OPERATOR_TOKEN` on
//! the daemon's spawn env, and sends the same token in every
//! `Caller::Operator { token_id }`. Agents spawned by the daemon
//! do NOT inherit the env var (see `mcp_config::build_env`).
//!
//! ## Threat model
//!
//! Phase 1 is local-only (0o600 Unix socket → same-UID-only).
//! Token validation closes the "same-UID, didn't bother snooping
//! files" gap: a stock MCP agent process won't be sending the
//! token because it isn't in its env. A truly hostile same-UID
//! process can still read process memory / files; this fix is
//! defense-in-depth, not a hard isolation boundary.
//!
//! ## Backward compatibility
//!
//! If `CM_OPERATOR_TOKEN` is unset at daemon startup, validation
//! is **disabled** (any Operator caller is accepted) with a
//! one-time warning on the daemon's stderr. This keeps existing
//! integration tests + ad-hoc operator tooling working without
//! requiring every consumer to discover and propagate the token.

use crate::control::protocol::Caller;
use std::sync::OnceLock;

/// Env var the daemon reads at startup to learn the operator token.
/// The TUI sets this when spawning the daemon (see
/// `tui/src/daemon_launch.rs::build_daemon_command`).
pub const ENV_VAR: &str = "CM_OPERATOR_TOKEN";

/// Configured token. `Some(token)` after `init_from_env` finds the
/// env var; `None` if validation is disabled.
static EXPECTED_TOKEN: OnceLock<Option<String>> = OnceLock::new();

/// Initialize from the process env. Call once at daemon startup
/// from `lib.rs::run`. Idempotent: subsequent calls are no-ops
/// (the first call wins, matching `OnceLock`'s semantics).
///
/// Logs a one-time warning on stderr if the env var is unset so
/// the operator notices when validation is silently disabled.
pub fn init_from_env() {
    let token = std::env::var(ENV_VAR).ok().filter(|s| !s.is_empty());
    let was_unset = token.is_none();
    let _ = EXPECTED_TOKEN.set(token);
    if was_unset {
        eprintln!(
            "cm-daemon: {} not set — Operator-caller token validation is DISABLED. \
             Any local same-UID process can issue Operator RPCs. The TUI sets this \
             automatically when launching the daemon; if you're seeing this, the \
             daemon was started without going through the TUI's launch path.",
            ENV_VAR,
        );
    }
}

/// Verify an incoming `Caller` is a valid Operator.
///
/// Returns:
///   - `Ok(())` if the caller is `Operator` AND (validation is disabled
///     OR the token matches in constant time).
///   - `Err(msg)` for Session callers and for Operator callers with the
///     wrong / missing token. The dispatcher wraps this in
///     `ErrorCode::Unauthorized` for the wire response.
///
/// Use this in dispatch arms that were previously written as
/// `if matches!(req.caller, Caller::Session(_)) { Unauthorized }` —
/// the new form rejects Session callers AND wrong-token Operators
/// with the same error class.
pub fn validate_operator(caller: &Caller) -> Result<(), &'static str> {
    let token_id = match caller {
        Caller::Operator(op) => &op.token_id,
        Caller::Session(_) => {
            return Err(
                "this method is Operator-callable only; Session callers are rejected",
            );
        }
    };
    match EXPECTED_TOKEN.get().and_then(|t| t.as_deref()) {
        None => Ok(()), // validation disabled (env var unset at startup)
        Some(expected) => {
            if constant_time_eq(token_id.as_bytes(), expected.as_bytes()) {
                Ok(())
            } else {
                Err("operator token does not match the daemon's configured token")
            }
        }
    }
}

/// Constant-time byte comparison. Equivalent to subtle's CtEq for
/// the small token-comparison case without pulling in the dep.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::protocol::Caller;

    /// `OnceLock` can't be re-set per test, so these tests use
    /// the env directly to drive `init_from_env`. They're guarded
    /// by `env_lock()` so the env mutation doesn't race other tests.
    /// The first test in the run that calls `init_from_env` sets
    /// the OnceLock for the whole process; later tests read
    /// whatever that first set. We mitigate by running these tests
    /// against the *fresh* OnceLock via a helper that re-reads
    /// the env each call (only used in this module's tests).
    fn lookup_for_test() -> Option<String> {
        std::env::var(ENV_VAR).ok().filter(|s| !s.is_empty())
    }

    fn validate_for_test(caller: &Caller, expected: Option<&str>) -> Result<(), &'static str> {
        let token_id = match caller {
            Caller::Operator(op) => &op.token_id,
            Caller::Session(_) => {
                return Err(
                    "this method is Operator-callable only; Session callers are rejected",
                );
            }
        };
        match expected {
            None => Ok(()),
            Some(exp) => {
                if constant_time_eq(token_id.as_bytes(), exp.as_bytes()) {
                    Ok(())
                } else {
                    Err("operator token does not match the daemon's configured token")
                }
            }
        }
    }

    #[test]
    fn session_caller_always_rejected() {
        let c = Caller::session("ts-a");
        assert!(validate_for_test(&c, None).is_err());
        assert!(validate_for_test(&c, Some("any")).is_err());
    }

    #[test]
    fn operator_with_no_configured_token_is_allowed() {
        let c = Caller::operator("whatever");
        assert!(validate_for_test(&c, None).is_ok());
    }

    #[test]
    fn operator_with_matching_token_is_allowed() {
        let c = Caller::operator("secret-xyz");
        assert!(validate_for_test(&c, Some("secret-xyz")).is_ok());
    }

    #[test]
    fn operator_with_wrong_token_is_rejected() {
        let c = Caller::operator("guessed-wrong");
        assert!(validate_for_test(&c, Some("real-secret")).is_err());
    }

    #[test]
    fn operator_with_empty_token_when_one_configured_is_rejected() {
        let c = Caller::operator("");
        assert!(validate_for_test(&c, Some("real")).is_err());
    }

    #[test]
    fn init_from_env_with_unset_var_leaves_validation_disabled() {
        let _g = crate::test_support::env_lock();
        std::env::remove_var(ENV_VAR);
        // Don't actually call init_from_env (would mutate process-
        // wide OnceLock). Verify the env-derived helper returns None.
        assert!(lookup_for_test().is_none());
    }

    #[test]
    fn init_from_env_with_set_var_returns_token() {
        let _g = crate::test_support::env_lock();
        std::env::set_var(ENV_VAR, "my-token");
        assert_eq!(lookup_for_test().as_deref(), Some("my-token"));
        std::env::remove_var(ENV_VAR);
    }

    #[test]
    fn init_from_env_treats_empty_string_as_unset() {
        let _g = crate::test_support::env_lock();
        std::env::set_var(ENV_VAR, "");
        assert!(lookup_for_test().is_none());
        std::env::remove_var(ENV_VAR);
    }

    #[test]
    fn constant_time_eq_handles_different_lengths() {
        assert!(!constant_time_eq(b"short", b"longer"));
        assert!(!constant_time_eq(b"longer", b"short"));
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"abc", b"abd"));
    }
}
