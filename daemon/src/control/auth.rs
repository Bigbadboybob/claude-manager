//! Session-caller authorization. Slice 10d-mcp-surface.
//!
//! When the daemon dispatches a Session-caller MCP request (an agent
//! running inside a daemon-spawned session calling tools like
//! `send_input`, `kill_session`, `read_session_output`, `start_session`),
//! the dispatch arm calls into this module to answer:
//! "Is this caller authorized to act on this target?"
//!
//! ## Sub-1 (current): self-only
//!
//! The TUI's existing rule (`tui/src/control/methods.rs::caller_authorized_for`)
//! is task-tree-based: a caller with task X is authorized for target
//! sessions whose task is a self-or-descendant of X via the
//! `parent_task_id` chain. A taskless caller is authorized for any
//! session in the same workspace.
//!
//! Phase 1's daemon doesn't carry the planning task tree yet (cloud-mode
//! concept; not plumbed through to the daemon). Sub-1 originally tried
//! the "same workspace" subset as a Phase 1 stand-in — but review caught
//! that as a widening for task-bound callers (the TUI rule restricts
//! task-bound callers to their task subtree; same-workspace would let a
//! task-bound caller reach sibling tasks in the same workspace).
//!
//! Sub-1 tightens the rule to **self-only** until task plumbing lands:
//!
//!   1. **Self**: `caller_uid == target_uid` → `Allow`.
//!   2. **Anything else** → `OutOfScope`.
//!
//! Self-only is unambiguously a subset of every reading of the TUI's
//! rule (every TUI rule branch allows self), so no widening is possible.
//! It's also enough to ship a Session-caller surface for tools that
//! only target the caller's own session (which is a small set, and
//! Phase 1's dispatch arms remain Operator-only anyway — see
//! `daemon/src/control/dispatch.rs`'s `TODO(slice 10d-mcp-surface-2)`
//! markers).
//!
//! **DO NOT relax this to same-workspace without also implementing
//! descendant tracking.** That was the round-1 widening; the
//! `DaemonSession.task_id` field exists for sub-2 to walk the task
//! tree, but until sub-2 plumbs the actual task list into `DaemonState`
//! the helper must stay self-only. Same-workspace as a Phase 1
//! stand-in re-introduces the round-1 finding for task-bound callers.
//!
//! Sub-2 will relax by adding a descendant-task-tree branch:
//! `Allow` if caller's task subtree contains target's task. The
//! existing `workspace_id` + `task_id` threading on `DaemonSession`
//! is the scaffolding for that.
//!
//! ## Why this is its own module
//!
//! The auth check is shared across every Session-callable dispatch arm
//! (`send_input`, `kill_session`, `read_session_output`, `start_session`,
//! `list_sessions`, and future MCP tools). Extracting it here keeps the
//! dispatch.rs arms readable and gives the rule a single test surface.

use crate::state::DaemonState;

/// Outcome of the Session-caller authorization check. The error
/// variants carry concrete reasons rather than a single `Unauthorized`
/// — useful for diagnostics when an agent gets surprised by a
/// `Unauthorized` response. The dispatcher maps these into wire
/// `ErrorCode::Unauthorized` / `NotFound` responses.
#[derive(Debug, PartialEq, Eq)]
pub enum AuthDecision {
    /// Authorized. The dispatch arm proceeds to call the method body.
    Allow,
    /// Caller's uid doesn't correspond to a live session in the
    /// daemon's registry. Either the agent uses a stale uid (e.g. a
    /// post-restart agent against a daemon that doesn't know it) or
    /// it forged one. Wire-level: `Unauthorized`.
    CallerNotInRegistry,
    /// Caller is live but target isn't. Distinct from "caller not
    /// found" so diagnostics name the right side. Wire-level:
    /// `NotFound`.
    TargetNotInRegistry,
    /// Both live, but target is outside caller's scope (different
    /// workspace and not self). Wire-level: `Unauthorized`.
    OutOfScope,
}

impl AuthDecision {
    /// True iff the dispatch arm should proceed. False on any
    /// negative outcome.
    pub fn is_allow(&self) -> bool {
        matches!(self, AuthDecision::Allow)
    }
}

/// Sub-1 self-only Session-caller authorization. Looks up both
/// the caller and the target in `DaemonState.sessions` and
/// applies the rule documented at the module top: `Allow` iff
/// `caller_uid == target_uid` (both live in the registry);
/// `OutOfScope` for any other target.
///
/// Sub-2 will add a descendant-task-tree branch on top of this
/// (Allow if target's task is a self-or-descendant of caller's
/// task via the planning task tree). Don't add same-workspace
/// here — that's the round-1 widening for task-bound callers.
pub fn check_session_caller(
    state: &DaemonState,
    caller_uid: &str,
    target_uid: &str,
) -> AuthDecision {
    // Caller-existence is the foundational gate — surfaces
    // first even if the target also happens to be missing.
    if !state.sessions.contains_key(caller_uid) {
        return AuthDecision::CallerNotInRegistry;
    }

    // Self-acts always allowed once caller is verified live.
    // Note the self short-circuit happens BEFORE the target
    // lookup: self_call_with_missing_caller_does_not_short_circuit
    // pins that the caller-existence check has higher priority,
    // but a verified-live caller acting on its own uid is the
    // single Allow path sub-1 supports.
    if caller_uid == target_uid {
        return AuthDecision::Allow;
    }

    if !state.sessions.contains_key(target_uid) {
        return AuthDecision::TargetNotInRegistry;
    }

    // Sub-1: no Allow path for cross-uid targets. Same-workspace
    // is intentionally NOT enough — sub-2 wires the descendant-
    // task-tree branch. Do NOT relax this without also
    // implementing task tracking (round-1 widening lesson).
    AuthDecision::OutOfScope
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{DaemonSession, SpawnParams};
    use crate::state::DaemonState;

    /// Build a minimal `DaemonSession` for auth tests without
    /// spawning a real PTY. Uses SpawnParams + PendingSession +
    /// arm_reaper against /bin/sleep so the session has a real
    /// pid; we don't actually use the PTY here.
    fn make_session(uid: &str, workspace_id: &str) -> DaemonSession {
        let mut p = SpawnParams::new(uid, format!("test-{}", uid), "/bin/sleep");
        p.args = vec!["3".to_string()];
        p.workspace_id = workspace_id.to_string();
        let pending = crate::session::PendingSession::spawn(p)
            .expect("test PendingSession::spawn");
        pending
            .arm_reaper(None)
            .expect("test arm_reaper")
    }

    fn state_with(sessions: Vec<DaemonSession>) -> DaemonState {
        let mut state = DaemonState::new();
        for s in sessions {
            state.sessions.insert(s.uid.clone(), s);
        }
        state
    }

    #[test]
    fn self_call_is_allowed() {
        let s = make_session("ts-self", "ws-x");
        let state = state_with(vec![s]);
        assert_eq!(
            check_session_caller(&state, "ts-self", "ts-self"),
            AuthDecision::Allow,
        );
    }

    /// Sub-1 conservative-self-only rule: same-workspace
    /// siblings are NOT enough to authorize. The round-1
    /// review caught same-workspace as a widening for
    /// task-bound callers vs the TUI rule; sub-1 tightens to
    /// self-only. Sub-2 reopens with a descendant-task-tree
    /// branch once task plumbing lands.
    #[test]
    fn same_workspace_sibling_is_out_of_scope_pending_task_plumbing() {
        let a = make_session("ts-a", "ws-shared");
        let b = make_session("ts-b", "ws-shared");
        let state = state_with(vec![a, b]);
        assert_eq!(
            check_session_caller(&state, "ts-a", "ts-b"),
            AuthDecision::OutOfScope,
            "sub-1 must not allow same-workspace siblings — only \
             self-call is authorized until sub-2 plumbs the task tree",
        );
    }

    #[test]
    fn cross_workspace_sibling_is_out_of_scope() {
        let a = make_session("ts-a", "ws-1");
        let b = make_session("ts-b", "ws-2");
        let state = state_with(vec![a, b]);
        assert_eq!(
            check_session_caller(&state, "ts-a", "ts-b"),
            AuthDecision::OutOfScope,
        );
    }

    #[test]
    fn missing_caller_surfaces_not_in_registry() {
        let b = make_session("ts-b", "ws-x");
        let state = state_with(vec![b]);
        assert_eq!(
            check_session_caller(&state, "ts-missing", "ts-b"),
            AuthDecision::CallerNotInRegistry,
        );
    }

    #[test]
    fn missing_target_with_live_caller_surfaces_target_not_in_registry() {
        let a = make_session("ts-a", "ws-x");
        let state = state_with(vec![a]);
        assert_eq!(
            check_session_caller(&state, "ts-a", "ts-target-missing"),
            AuthDecision::TargetNotInRegistry,
        );
    }

    /// Self-acts on a missing caller must NOT short-circuit — the
    /// caller-existence check is foundational and surfaces first.
    /// Pre-fix this would return Allow for a stale uid that's the
    /// same on both sides; the post-fix version reports
    /// CallerNotInRegistry which the dispatcher maps to
    /// `Unauthorized`.
    #[test]
    fn self_call_with_missing_caller_does_not_short_circuit() {
        let state = state_with(vec![]);
        assert_eq!(
            check_session_caller(&state, "ts-ghost", "ts-ghost"),
            AuthDecision::CallerNotInRegistry,
        );
    }

    /// Pin `AuthDecision::is_allow()` against EVERY variant.
    /// For the `Allow` branch the test drives through the
    /// self-call code path (the only Allow source under sub-1's
    /// self-only rule) so this stays load-bearing under future
    /// refactors of `check_session_caller`.
    #[test]
    fn is_allow_helper_matches_variant() {
        // Allow: derived from the self-call path — sub-1's only
        // Allow source. If a future refactor reintroduces
        // same-workspace=Allow without a descendant check, the
        // `same_workspace_sibling_is_out_of_scope_pending_task_plumbing`
        // test catches it; this one just pins that the
        // is_allow() helper agrees with the Allow variant.
        let s = make_session("ts-self-helper", "ws-x");
        let state = state_with(vec![s]);
        let allow_decision = check_session_caller(&state, "ts-self-helper", "ts-self-helper");
        assert_eq!(allow_decision, AuthDecision::Allow);
        assert!(allow_decision.is_allow());

        // Negative variants — pure variant-shape pins.
        assert!(!AuthDecision::CallerNotInRegistry.is_allow());
        assert!(!AuthDecision::TargetNotInRegistry.is_allow());
        assert!(!AuthDecision::OutOfScope.is_allow());
    }
}
