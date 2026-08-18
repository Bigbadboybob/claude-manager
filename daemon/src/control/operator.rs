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
//!
//! ## The strong gate (DESIGN_SEAMLESS_RESTART phase 2e, R14)
//!
//! The compat fail-open above is tolerable for today's Operator
//! methods — worst case, a same-UID process drives sessions it
//! could already reach. It is NOT tolerable for lifecycle RPCs
//! that replace the daemon's running code (`daemon.restart`):
//! with validation disabled, ANY same-UID process that can reach
//! the 0600 socket — including a hosted session — can present
//! itself as Operator. `require_strong_operator` /
//! `strong_operator_auth_configured` are the fail-closed
//! primitive those RPCs gate on; everything else keeps the
//! compat path via `validate_operator`, unchanged.

use crate::control::protocol::{Caller, ErrorCode};
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

// ============================================================
// Fail-closed strong-operator gate (DESIGN_SEAMLESS_RESTART
// phase 2e, review finding R14)
// ============================================================
//
// `daemon.restart` is arbitrary code replacement for a process that
// holds every session's PTY. The compat behavior at the top of this
// file — token validation disabled when `CM_OPERATOR_TOKEN` is unset —
// means "Operator" is otherwise a claim any same-UID socket client
// can make. These primitives are the design's answer: a
// code-replacement RPC calls `require_strong_operator` instead of
// `validate_operator` and refuses to run unless strong operator
// authentication is BOTH configured and passed by the caller.
//
// Deliberately NOT a change to `validate_operator` or the compat
// path: every existing method keeps today's behavior. This is a
// second, stricter gate that future lifecycle RPCs opt into.

/// The single expected-token accessor for the strong gate.
///
/// Reads the process-global `EXPECTED_TOKEN` set by `init_from_env`.
/// An unset `OnceLock` (init never ran — only possible in states not
/// built by `lib.rs::run`, i.e. tests) reads as *no token*, which the
/// strong gate treats as not-configured: fail closed, never open.
///
/// In test builds a scoped override can substitute either world
/// (configured / not configured) without touching the `OnceLock`,
/// which is process-global and first-set-wins — see `test_override`.
fn configured_token() -> Option<String> {
    #[cfg(test)]
    if let Some(world) = test_override::get() {
        return world;
    }
    EXPECTED_TOKEN.get().and_then(|t| t.clone())
}

/// Is strong operator authentication configured on this daemon?
///
/// True ONLY when operator-token validation is actually enabled —
/// `CM_OPERATOR_TOKEN` was set non-empty when `init_from_env` ran at
/// startup. Explicitly false when validation is disabled (the compat
/// fail-open path), **regardless of `[auth] mode`**: ssh-trust does
/// not count as strong here because it authenticates *socket
/// reachability*, and hosted same-UID sessions reach the same 0600
/// socket the operator does — socket reachability must not equal
/// restart authority (DESIGN_SEAMLESS_RESTART.md § Security posture,
/// R14).
///
/// Surfaced on `daemon.health` as `strong_operator_auth` so a deploy
/// script can check the precondition before attempting a future
/// `daemon.restart`.
pub fn strong_operator_auth_configured() -> bool {
    configured_token().is_some()
}

/// Fail-closed operator check for code-replacement / lifecycle RPCs
/// (DESIGN_SEAMLESS_RESTART phase 2e, R14).
///
/// `Ok(())` ONLY when (a) strong operator authentication is
/// configured (`strong_operator_auth_configured`) AND (b) the caller
/// is an Operator whose token matches in constant time. Everything
/// else errors — including an Operator-shaped caller in a daemon
/// whose validation is disabled, the case `validate_operator` waves
/// through for compatibility.
///
/// Errors are `(ErrorCode::Unauthorized, message)` pairs matching the
/// `MethodResult` convention in `control/methods.rs`; the dispatcher
/// forwards them onto the wire unchanged.
pub fn require_strong_operator(caller: &Caller) -> Result<(), (ErrorCode, String)> {
    require_strong_operator_with(caller, configured_token().as_deref())
}

/// The pure core of `require_strong_operator`, split out so tests can
/// inject the expected-token world directly (the established idiom in
/// this module — see `validate_for_test` in the tests below — since
/// the real token lives in a first-set-wins process-global).
fn require_strong_operator_with(
    caller: &Caller,
    expected: Option<&str>,
) -> Result<(), (ErrorCode, String)> {
    // (a) Configuration check FIRST: with validation disabled there is
    // no secret to compare against, so even a perfectly-shaped Operator
    // frame proves nothing. Fail closed — this is the exact gap R14
    // flagged (compat fail-open ⇒ any same-UID socket client, hosted
    // sessions included, can claim Operator).
    let expected = match expected {
        Some(t) => t,
        None => {
            return Err((
                ErrorCode::Unauthorized,
                format!(
                    "strong operator authentication is not configured: {} was unset \
                     (or empty) at daemon startup, so operator-token validation is \
                     disabled. This method can replace the daemon's running code, so \
                     it fails CLOSED instead of inheriting that compat fail-open path \
                     — and ssh-trust auth mode does not count, because socket \
                     reachability must not equal restart authority (hosted same-UID \
                     sessions reach the same socket). Set {} on the daemon's spawn \
                     env and restart. See DESIGN_SEAMLESS_RESTART.md § Security \
                     posture (R14).",
                    ENV_VAR, ENV_VAR,
                ),
            ));
        }
    };
    // (b) Caller check: same shape as `validate_operator`, but with no
    // disabled-world bypass above it.
    let token_id = match caller {
        Caller::Operator(op) => &op.token_id,
        Caller::Session(_) => {
            return Err((
                ErrorCode::Unauthorized,
                "this method is Operator-callable only; Session callers are rejected \
                 (code-replacement RPC — fail-closed per DESIGN_SEAMLESS_RESTART.md \
                 § Security posture, R14)"
                    .to_string(),
            ));
        }
    };
    if constant_time_eq(token_id.as_bytes(), expected.as_bytes()) {
        Ok(())
    } else {
        Err((
            ErrorCode::Unauthorized,
            "operator token does not match the daemon's configured token".to_string(),
        ))
    }
}

/// Test-only override of the configured-token world.
///
/// `EXPECTED_TOKEN` is a first-set-wins process-global, so tests can't
/// flip it between "configured" and "disabled" — and cargo runs the
/// whole crate's tests in one process. This scoped override lets a
/// test pin either world for the REAL public entry points
/// (`strong_operator_auth_configured`, `require_strong_operator`, and
/// `daemon.health`'s `strong_operator_auth` bit) instead of only the
/// injected core. Callers MUST hold `crate::test_support::env_lock()`
/// for the guard's lifetime — same serialization idiom as every other
/// process-global in the test suite — so parallel tests don't stomp
/// each other's world.
#[cfg(test)]
pub(crate) mod test_override {
    use std::sync::Mutex;

    /// `None` = no override (read the real `EXPECTED_TOKEN`);
    /// `Some(world)` = pretend init saw `world` (`Some(token)` /
    /// `None` for the disabled compat world).
    static OVERRIDE: Mutex<Option<Option<String>>> = Mutex::new(None);

    /// RAII guard: dropping it restores "no override" so a panicking
    /// test can't leak its world into the next one.
    pub(crate) struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            *OVERRIDE.lock().unwrap_or_else(|p| p.into_inner()) = None;
        }
    }

    /// Pin the token world for the guard's lifetime. Hold
    /// `env_lock()` across it.
    pub(crate) fn set(world: Option<&str>) -> Guard {
        *OVERRIDE.lock().unwrap_or_else(|p| p.into_inner()) =
            Some(world.map(String::from));
        Guard
    }

    pub(crate) fn get() -> Option<Option<String>> {
        OVERRIDE.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
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

    // ========================================================
    // Strong gate (DESIGN_SEAMLESS_RESTART phase 2e, R14)
    // ========================================================

    /// The R14 gap itself: validation disabled ⇒ the strong gate
    /// refuses even a perfectly Operator-shaped caller. This is the
    /// exact input `validate_operator` waves through for compat.
    #[test]
    fn strong_gate_fails_closed_when_validation_disabled() {
        let c = Caller::operator("any-claimed-token");
        let err = require_strong_operator_with(&c, None)
            .expect_err("disabled world must refuse Operator-shaped callers");
        assert_eq!(err.0, ErrorCode::Unauthorized);
        assert!(
            err.1.contains(ENV_VAR),
            "error must name what's missing: {}",
            err.1
        );
        assert!(
            err.1.contains("DESIGN_SEAMLESS_RESTART"),
            "error must cite the design doc: {}",
            err.1
        );
        // Contrast pin: the compat path accepts this same caller —
        // the strong gate is a second gate, not a change to the first.
        assert!(validate_for_test(&c, None).is_ok());
    }

    /// Same disabled world through the REAL public entry points
    /// (override machinery), not just the injected core.
    #[test]
    fn strong_gate_public_fns_in_disabled_world() {
        let _g = crate::test_support::env_lock();
        let _w = test_override::set(None);
        assert!(!strong_operator_auth_configured());
        let err = require_strong_operator(&Caller::operator("whatever"))
            .expect_err("disabled world must fail closed");
        assert_eq!(err.0, ErrorCode::Unauthorized);
    }

    #[test]
    fn strong_gate_accepts_operator_with_matching_token() {
        let c = Caller::operator("secret-xyz");
        assert!(require_strong_operator_with(&c, Some("secret-xyz")).is_ok());

        // And through the public fns.
        let _g = crate::test_support::env_lock();
        let _w = test_override::set(Some("secret-xyz"));
        assert!(strong_operator_auth_configured());
        assert!(require_strong_operator(&c).is_ok());
    }

    #[test]
    fn strong_gate_rejects_session_callers_even_when_configured() {
        let c = Caller::session("ts-a");
        let err = require_strong_operator_with(&c, Some("real-secret"))
            .expect_err("session callers must be rejected");
        assert_eq!(err.0, ErrorCode::Unauthorized);
        assert!(
            err.1.contains("Operator-callable only"),
            "error must say why: {}",
            err.1
        );
    }

    #[test]
    fn strong_gate_rejects_wrong_and_empty_tokens() {
        let wrong = Caller::operator("guessed-wrong");
        let err = require_strong_operator_with(&wrong, Some("real-secret"))
            .expect_err("wrong token must be rejected");
        assert_eq!(err.0, ErrorCode::Unauthorized);
        assert!(err.1.contains("does not match"), "{}", err.1);

        let empty = Caller::operator("");
        assert!(require_strong_operator_with(&empty, Some("real-secret")).is_err());
    }

    /// The override guard restores "no override" on drop, so one
    /// test's pinned world can't leak into the next (the test-order
    /// hazard this module's header warns about).
    #[test]
    fn strong_gate_test_override_is_scoped() {
        let _g = crate::test_support::env_lock();
        {
            let _w = test_override::set(Some("tok"));
            assert!(strong_operator_auth_configured());
        }
        // Guard dropped → back to the real (init-never-ran) world,
        // which reads as not-configured: fail closed.
        assert_eq!(test_override::get(), None);
    }
}
