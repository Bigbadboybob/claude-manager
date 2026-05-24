//! Session-caller authorization. Slice 10d-mcp-surface.
//!
//! When the daemon dispatches a Session-caller MCP request (an agent
//! running inside a daemon-spawned session calling tools like
//! `send_input`, `kill_session`, `read_session_output`, `start_session`),
//! the dispatch arm calls into this module to answer:
//! "Is this caller authorized to act on this target?"
//!
//! ## Sub-2a: TUI-mirror task-tree + workspace
//!
//! Mirrors `tui/src/control/methods.rs::caller_authorized_for` so the
//! daemon and the TUI agree on who can act on what. Two regimes:
//!
//!   - **Tasked caller** (`caller.task_id = Some(_)`): authorized iff
//!     the target's `task_id` is the caller's task itself or a
//!     descendant of it via the planning task tree's
//!     `parent_task_id` chain. **Purely task-tree** — no workspace
//!     constraint. (Branch-mode subtasks live in fresh child
//!     workspaces, so the caller's workspace and the target's
//!     workspace are typically different.) A tasked caller CANNOT
//!     reach a taskless target — that's the `.unwrap_or(false)` arm
//!     in the TUI rule.
//!   - **Taskless caller** (`caller.task_id = None`, `A-n` shape):
//!     authorized iff the target is in the same workspace as the
//!     caller. Same workspace, any task or no task.
//!
//! Plus the foundational gates:
//!   - Self: `caller_uid == target_uid` → `Allow` (mirrors the
//!     TUI's implicit "I can act on myself" — the helper above
//!     bails before that on `caller_wi.is_none()`, but our daemon
//!     puts self-call ahead of the regime split for clarity).
//!   - Caller not in registry → `CallerNotInRegistry`.
//!   - Target not in registry → `TargetNotInRegistry`.
//!
//! ## Task-tree source of truth
//!
//! `DaemonState.task_tree: HashMap<task_id, Option<parent_task_id>>`
//! is a **TUI-pushed snapshot** updated via the `task.update_tree`
//! RPC whenever `App.tasks` mutates. Two reasons over "daemon owns
//! the task tree":
//!   1. Cheaper to land — no planning-API HTTP refactor needed.
//!   2. The dependency unwinds cleanly when the workflow controller
//!      relocates daemon-side (slice 10d-workflow-controller / sub-2c
//!      territory): the controller will own task transitions and
//!      write to `DaemonState.task_tree` directly, replacing the RPC
//!      push.
//!
//! Until the TUI side wires the push, `DaemonState.task_tree` stays
//! empty. The auth check then behaves as if every tasked caller's
//! task has no descendants — `task_is_self_or_descendant_of`
//! returns true only for `target == caller` (self-task), false
//! otherwise. Safer-than-TUI default until snapshots land.
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

/// Cap on the parent_task_id walk in
/// [`task_is_self_or_descendant_of`]. Defends against cycles or
/// pathologically deep trees — neither should occur in practice
/// but the auth check should not hang or stack-overflow if they
/// do. Mirrors `tui/src/control/methods.rs::MAX_TASK_DEPTH`.
pub const MAX_TASK_DEPTH: usize = 64;

/// Is `target_id` either equal to `ancestor_id` or a (transitive)
/// descendant of it via the `parent_task_id` chain in
/// `task_tree`? Walks up from `target_id` toward roots. Mirrors
/// `tui/src/control/methods.rs::task_is_self_or_descendant_of`.
///
/// Notes on the walk:
///   - `task_tree[child] = Some(parent)` means `child`'s parent
///     is `parent`. `task_tree[root] = None` means top-level.
///   - A `task_id` missing from `task_tree` is treated as a
///     top-level task with no parent — returns `target_id ==
///     ancestor_id` (self) and otherwise `false`. Safer than
///     assuming structure we don't have.
///   - Cycle detection: if the same `task_id` appears twice in
///     the walk, return false. Cap at `MAX_TASK_DEPTH` as a
///     belt-and-suspenders bound.
pub fn task_is_self_or_descendant_of(
    task_tree: &std::collections::HashMap<String, Option<String>>,
    target_id: &str,
    ancestor_id: &str,
) -> bool {
    if target_id == ancestor_id {
        return true;
    }
    let mut cur = target_id.to_string();
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    for _ in 0..MAX_TASK_DEPTH {
        if !visited.insert(cur.clone()) {
            return false;
        }
        let parent = match task_tree.get(&cur) {
            Some(p) => p.clone(),
            None => return false,
        };
        let parent_id = match parent {
            Some(p) => p,
            None => return false,
        };
        if parent_id == ancestor_id {
            return true;
        }
        cur = parent_id;
    }
    false
}

/// Sub-2a Session-caller authorization. Mirrors
/// `tui/src/control/methods.rs::caller_authorized_for` so the
/// daemon and TUI agree on the rule.
///
/// Decision order:
///   1. Caller missing from registry → `CallerNotInRegistry`.
///   2. Self-call (`caller_uid == target_uid`) → `Allow`.
///   3. Target missing from registry → `TargetNotInRegistry`.
///   4. Regime split on `caller.task_id`:
///      - `Some(caller_task)`: target must have `task_id` AND it
///        must be self-or-descendant of `caller_task` in
///        `state.task_tree`.
///      - `None` (taskless): target must be in the same workspace.
///   5. Otherwise → `OutOfScope`.
pub fn check_session_caller(
    state: &DaemonState,
    caller_uid: &str,
    target_uid: &str,
) -> AuthDecision {
    let caller = match state.sessions.get(caller_uid) {
        Some(s) => s,
        None => return AuthDecision::CallerNotInRegistry,
    };

    // Self-call short-circuits any further check. Both the TUI
    // rule and the daemon implicitly allow this (every TUI rule
    // branch admits self).
    if caller_uid == target_uid {
        return AuthDecision::Allow;
    }

    let target = match state.sessions.get(target_uid) {
        Some(s) => s,
        None => return AuthDecision::TargetNotInRegistry,
    };

    match &caller.task_id {
        Some(caller_task) => {
            // Tasked caller — purely task-tree, no workspace
            // constraint (branch-mode subtasks land in child
            // workspaces). Target MUST have a task_id; a tasked
            // caller cannot reach a taskless target.
            match &target.task_id {
                Some(target_task) => {
                    if task_is_self_or_descendant_of(
                        &state.task_tree,
                        target_task,
                        caller_task,
                    ) {
                        AuthDecision::Allow
                    } else {
                        AuthDecision::OutOfScope
                    }
                }
                None => AuthDecision::OutOfScope,
            }
        }
        None => {
            // Taskless caller (`A-n` shape) — same-workspace
            // rule. Mirrors TUI's `None => caller_wi == target_wi`.
            if caller.workspace_id == target.workspace_id {
                AuthDecision::Allow
            } else {
                AuthDecision::OutOfScope
            }
        }
    }
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
        make_session_tasked(uid, workspace_id, None)
    }

    /// Variant with explicit `task_id`. Sub-2a test helper for
    /// the descendant-task-tree branch.
    fn make_session_tasked(
        uid: &str,
        workspace_id: &str,
        task_id: Option<&str>,
    ) -> DaemonSession {
        let mut p = SpawnParams::new(uid, format!("test-{}", uid), "/bin/sleep");
        p.args = vec!["3".to_string()];
        p.workspace_id = workspace_id.to_string();
        p.task_id = task_id.map(str::to_string);
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

    // ============================================================
    // Sub-2a TUI-mirror tests: taskless and tasked regimes.
    //
    // Taskless caller → same-workspace rule (mirrors TUI
    //   `caller_authorized_for`'s `None` arm).
    // Tasked caller → purely task-tree, no workspace constraint
    //   (mirrors the `Some(task_id) => …unwrap_or(false)` arm).
    // Tasked caller targeting a taskless target → OutOfScope
    //   (the TUI rule's `.unwrap_or(false)` for missing
    //   target_task_id).
    // ============================================================

    /// Taskless caller, same workspace → Allow. Round-1 review
    /// caught this as widening for *task-bound* callers; sub-2a
    /// preserves the TUI's taskless-caller semantics where
    /// same-workspace IS authorized (the `A-n` shape).
    #[test]
    fn taskless_caller_same_workspace_sibling_is_allowed() {
        let a = make_session("ts-a", "ws-shared");
        let b = make_session("ts-b", "ws-shared");
        let state = state_with(vec![a, b]);
        assert_eq!(
            check_session_caller(&state, "ts-a", "ts-b"),
            AuthDecision::Allow,
            "TUI rule: taskless caller can reach any session in the same workspace",
        );
    }

    /// Taskless caller, different workspace → OutOfScope.
    #[test]
    fn taskless_caller_cross_workspace_sibling_is_out_of_scope() {
        let a = make_session("ts-a", "ws-1");
        let b = make_session("ts-b", "ws-2");
        let state = state_with(vec![a, b]);
        assert_eq!(
            check_session_caller(&state, "ts-a", "ts-b"),
            AuthDecision::OutOfScope,
        );
    }

    /// Tasked caller targeting same-task target → Allow.
    /// Task-tree branch of the TUI rule.
    #[test]
    fn tasked_caller_same_task_target_is_allowed() {
        let a = make_session_tasked("ts-a", "ws-1", Some("task-shared"));
        let b = make_session_tasked("ts-b", "ws-2", Some("task-shared"));
        let state = state_with(vec![a, b]);
        // Note: different workspaces — tasked caller doesn't care.
        assert_eq!(
            check_session_caller(&state, "ts-a", "ts-b"),
            AuthDecision::Allow,
        );
    }

    /// Tasked caller targeting a DESCENDANT task → Allow.
    /// Walks `task_tree` to verify the parent_task_id chain
    /// reaches the caller's task.
    #[test]
    fn tasked_caller_descendant_target_is_allowed() {
        let parent = make_session_tasked("ts-parent", "ws-1", Some("task-parent"));
        let child = make_session_tasked("ts-child", "ws-1", Some("task-child"));
        let mut state = state_with(vec![parent, child]);
        state.task_tree.insert("task-parent".into(), None);
        state.task_tree.insert("task-child".into(), Some("task-parent".into()));
        assert_eq!(
            check_session_caller(&state, "ts-parent", "ts-child"),
            AuthDecision::Allow,
        );
    }

    /// Reverse direction: child task cannot reach parent
    /// (descendant only, not ancestor).
    #[test]
    fn tasked_caller_cannot_reach_ancestor_target() {
        let parent = make_session_tasked("ts-parent", "ws-1", Some("task-parent"));
        let child = make_session_tasked("ts-child", "ws-1", Some("task-child"));
        let mut state = state_with(vec![parent, child]);
        state.task_tree.insert("task-parent".into(), None);
        state.task_tree.insert("task-child".into(), Some("task-parent".into()));
        assert_eq!(
            check_session_caller(&state, "ts-child", "ts-parent"),
            AuthDecision::OutOfScope,
            "descendant rule is one-directional — children can't reach parents",
        );
    }

    /// Tasked caller, sibling task (shared parent) → OutOfScope.
    /// The TUI rule walks from target UP; siblings share an
    /// ancestor but neither is the other's descendant.
    #[test]
    fn tasked_caller_sibling_task_target_is_out_of_scope() {
        let a = make_session_tasked("ts-a", "ws-1", Some("task-a"));
        let b = make_session_tasked("ts-b", "ws-1", Some("task-b"));
        let mut state = state_with(vec![a, b]);
        state.task_tree.insert("task-parent".into(), None);
        state.task_tree.insert("task-a".into(), Some("task-parent".into()));
        state.task_tree.insert("task-b".into(), Some("task-parent".into()));
        assert_eq!(
            check_session_caller(&state, "ts-a", "ts-b"),
            AuthDecision::OutOfScope,
        );
    }

    /// Tasked caller targeting a TASKLESS target → OutOfScope.
    /// Mirrors TUI rule's `.unwrap_or(false)` — a tasked caller
    /// cannot reach taskless targets.
    #[test]
    fn tasked_caller_cannot_reach_taskless_target() {
        let tasked = make_session_tasked("ts-tasked", "ws-1", Some("task-x"));
        let taskless = make_session_tasked("ts-taskless", "ws-1", None);
        let state = state_with(vec![tasked, taskless]);
        assert_eq!(
            check_session_caller(&state, "ts-tasked", "ts-taskless"),
            AuthDecision::OutOfScope,
        );
    }

    /// Same-workspace tasked siblings WITHOUT a shared task
    /// tree → OutOfScope. This is the round-1 widening that
    /// must NOT be reintroduced: tasked callers don't get
    /// workspace-scope fall-back.
    #[test]
    fn tasked_caller_same_workspace_unrelated_task_is_out_of_scope() {
        let a = make_session_tasked("ts-a", "ws-shared", Some("task-a"));
        let b = make_session_tasked("ts-b", "ws-shared", Some("task-b"));
        let mut state = state_with(vec![a, b]);
        // No parent_task_id links → tasks are independent.
        state.task_tree.insert("task-a".into(), None);
        state.task_tree.insert("task-b".into(), None);
        assert_eq!(
            check_session_caller(&state, "ts-a", "ts-b"),
            AuthDecision::OutOfScope,
            "tasked callers must NOT get workspace-scope fall-back \
             (round-1 widening lesson — TUI rule has no such fallback)",
        );
    }

    /// Empty task_tree with tasked caller → only self-task
    /// targets pass. Pre-snapshot behavior: safer-than-TUI
    /// default until `task.update_tree` lands.
    #[test]
    fn tasked_caller_empty_task_tree_only_allows_same_task() {
        let a = make_session_tasked("ts-a", "ws-1", Some("task-shared"));
        let b = make_session_tasked("ts-b", "ws-1", Some("task-shared"));
        let state = state_with(vec![a, b]);
        // task_tree is empty — no parent_task_id info available.
        assert_eq!(
            check_session_caller(&state, "ts-a", "ts-b"),
            AuthDecision::Allow,
            "same task_id is the only descendant relationship the empty tree can prove",
        );
        // Different tasks, empty tree → no Allow path.
        let c = make_session_tasked("ts-c", "ws-1", Some("task-different"));
        let mut state2 = state_with(vec![
            make_session_tasked("ts-a", "ws-1", Some("task-shared")),
            c,
        ]);
        // Empty tree — no parent_task_id info.
        assert_eq!(state2.task_tree.len(), 0);
        assert_eq!(
            check_session_caller(&state2, "ts-a", "ts-c"),
            AuthDecision::OutOfScope,
        );
        let _ = state2.task_tree.insert("placeholder".into(), None);
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

    // ============================================================
    // task_is_self_or_descendant_of — pure walk tests.
    // ============================================================

    fn make_tree(pairs: &[(&str, Option<&str>)]) -> std::collections::HashMap<String, Option<String>> {
        pairs
            .iter()
            .map(|(t, p)| (t.to_string(), p.map(str::to_string)))
            .collect()
    }

    #[test]
    fn task_walk_self_is_descendant_of_self() {
        let tree = make_tree(&[]);
        assert!(task_is_self_or_descendant_of(&tree, "task-a", "task-a"));
    }

    #[test]
    fn task_walk_direct_child_reaches_parent() {
        let tree = make_tree(&[("task-parent", None), ("task-child", Some("task-parent"))]);
        assert!(task_is_self_or_descendant_of(&tree, "task-child", "task-parent"));
    }

    #[test]
    fn task_walk_grandchild_reaches_grandparent() {
        let tree = make_tree(&[
            ("a", None),
            ("b", Some("a")),
            ("c", Some("b")),
        ]);
        assert!(task_is_self_or_descendant_of(&tree, "c", "a"));
    }

    #[test]
    fn task_walk_parent_is_not_descendant_of_child() {
        let tree = make_tree(&[("task-parent", None), ("task-child", Some("task-parent"))]);
        assert!(!task_is_self_or_descendant_of(&tree, "task-parent", "task-child"));
    }

    #[test]
    fn task_walk_siblings_not_descendants_of_each_other() {
        let tree = make_tree(&[
            ("p", None),
            ("a", Some("p")),
            ("b", Some("p")),
        ]);
        assert!(!task_is_self_or_descendant_of(&tree, "a", "b"));
        assert!(!task_is_self_or_descendant_of(&tree, "b", "a"));
    }

    #[test]
    fn task_walk_missing_target_returns_false() {
        let tree = make_tree(&[("ancestor", None)]);
        // Target not in tree, target != ancestor → false.
        assert!(!task_is_self_or_descendant_of(&tree, "unknown", "ancestor"));
        // Target == ancestor short-circuits to true regardless.
        assert!(task_is_self_or_descendant_of(&tree, "unknown", "unknown"));
    }

    #[test]
    fn task_walk_cycle_does_not_hang() {
        // Pathological tree with a cycle. The walk should
        // terminate (via visited-set OR MAX_TASK_DEPTH cap) and
        // return false rather than spin forever.
        let tree = make_tree(&[
            ("a", Some("b")),
            ("b", Some("a")),
        ]);
        let res = task_is_self_or_descendant_of(&tree, "a", "c");
        assert!(!res, "cycle must not produce a phantom positive");
    }

    #[test]
    fn task_walk_max_depth_terminates() {
        // Build a chain longer than MAX_TASK_DEPTH. The walk
        // should bail at the cap.
        let mut pairs: Vec<(String, Option<String>)> = Vec::new();
        pairs.push(("t-0".into(), None));
        for i in 1..(MAX_TASK_DEPTH + 10) {
            pairs.push((format!("t-{}", i), Some(format!("t-{}", i - 1))));
        }
        let tree: std::collections::HashMap<String, Option<String>> =
            pairs.into_iter().collect();
        // The deepest task is well past the cap; the walk
        // should bail and return false despite there being a
        // valid chain (the cap exists to prevent unbounded
        // work on adversarial trees).
        let deepest = format!("t-{}", MAX_TASK_DEPTH + 9);
        assert!(
            !task_is_self_or_descendant_of(&tree, &deepest, "t-0"),
            "MAX_TASK_DEPTH cap must defend against unbounded walks",
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
