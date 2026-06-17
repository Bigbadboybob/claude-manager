# Existing-session binding for workflow roles

## Summary

Re-add the ability to bind an already-running session to a workflow role at launch, instead of the daemon always spawning every participant fresh. The motivating case: an orchestrator (the `design-doc-impl-loop` / `parallel-impl-wave` skills) or a TUI `A-f` reuses a session that already has explored context as the worker, so the worker starts warm rather than cold with only the goal prompt.

## Problem

When workflow orchestration moved into `cm-daemon` (Phase 4 of the daemon relocation), `start_workflow` was built fresh-only: it spawns a brand-new session per role and has no way to adopt an existing one. The TUI `A-f` chooser was simplified to only ever offer `New(engine)` slots, and the MCP `start_workflow` tool exposes only `task_id` / `workflow_name` / `goal`. The pre-daemon "use the focused session as the worker" capability — where a session you'd been working in kept its context when it became the worker — was dropped for both local and cloud launches (the daemon is now the single orchestrator for both; there is no cloud-vs-local difference).

The cost: an orchestrator that spends a session exploring the codebase, then launches a feedback workflow, gets a cold worker that re-derives everything from the goal string. Context (the exploration, an accepted plan, prior discussion) is lost at the launch boundary.

## Goals

- `start_workflow` accepts an optional `role_sessions` map (`role -> existing daemon_session_uid`); for a bound role the daemon adopts the existing live session instead of spawning a fresh one.
- A bound role's message baseline is derived from its live transcript at bind time, so its prior turns are NOT counted as new turns (the idle gate fires only on turns produced after the bind).
- The initial role (worker), when bound, receives the goal/activation prompt delivered to the already-running agent.
- The MCP `start_workflow` tool exposes `role_sessions` so the orchestrator skills can bind.
- The TUI `A-f` flow can offer an existing session for an eligible role.
- The fresh-spawn path is unchanged when `role_sessions` is absent — the headless cm-manager e2e (which passes no `role_sessions`) is not regressed.

## Non-goals

- Binding a role that needs the workflow MCP tools (`needs_mcp = true`, e.g. a manager that calls `workflow_done`). Such a session would need to be respawned with `--resume` + workflow MCP config to gain `CM_WORKFLOW_RUN_ID` / `CM_ROLE`, which both adds the dormant-`respawn_existing_with_workflow_mcp` complexity AND defeats the "preserve the live UI/context" benefit (it kills + respawns the agent). In practice there is rarely a pre-existing "manager" session to bind anyway. `needs_mcp = true` roles are always fresh-spawned; a `role_sessions` entry for one is rejected with a clear error.
- Binding a `Context::Fresh` role (e.g. the feedback reviewer). "Fresh" means the agent is reset (`/clear` + rebind) on every activation to start clean — adopting a session to *keep* its context directly contradicts that. `Context::Fresh` roles are always fresh-spawned; a `role_sessions` entry for one is rejected.
- Cross-host binding (binding a session on a different daemon host). The bound uid must live in the launching daemon's own `state.sessions`.
- Reviving an exited session as a role. The bound session must be live (present in `state.sessions`).

## Current state

`start_workflow` (daemon/src/control/methods.rs `start_workflow`, params struct ~2480, body ~2500+) resolves the workflow definition + worktree, then in a loop "Spawn each role fresh, in role_order, via the in-process start_session" (~2782) builds a fresh `start_session` per role. At the end of each iteration it records `role_sessions.insert(role, RoleBinding { current_session_id: None, daemon_session_uid: Some(uid) })` (~2999) and `role_baselines.insert(role, MessageBaseline::default())` (~3004, i.e. 0/0). It then constructs the run via `WorkflowRun::new(...)` (~3012) and records the worker's initial `pending_activation` with `is_initial: true` (~3063). Claude roles arm no spawn-time detector and bind their sid causally at first activation (`ActivationPhase::RebindPending`); the poller's `sync_role_session_ids` / `sync_participant_transcript_paths` bridge a live session's `transcript_path` into `current_session_id`.

`RoleBinding` (daemon/src/workflow/run.rs:105) = `{ session_label, current_session_id: Option<String>, daemon_session_uid: Option<String> }`. `MessageBaseline` (run.rs:118) = `{ user_count, assistant_count }`. Per-role idle is gated against the baseline: a turn counts as "new" only when the live transcript's count exceeds the role's baseline (see `assistant_turn_completed_since` in the poller gate).

Message counts come from `workflow::transcript::count_messages(engine, worktree, sid, MessageKind)` (daemon/src/workflow/transcript.rs:356); the canonical sid for a live session is derived from `DaemonSession.transcript_path` (claude: file stem; codex: first-line `payload.id` — see `codex_session_id_from_path`).

Caller scope is enforced by `crate::control::auth::check_session_caller` (daemon/src/control/auth.rs:168) for Session callers (descendant-task / same-workspace), and Operator callers are validated by `operator::validate_operator` at the dispatch boundary.

The MCP tool `start_workflow` (mcp_server/server.py:615) forwards `task_id` / `workflow_name` / `goal` only.

The TUI builds slots in `enter_workflow_launch_confirm` (tui/src/app.rs ~12951) as `WorkflowSlotChoice { options: vec![WorkflowSlotSource::New(role.engine)] }` — the `WorkflowSlotSource::Existing(si)` variant exists and is rendered in `draw_workflow_launch` (~13890) but is never produced at launch. `launch_workflow_via_daemon` (~12978) calls `rpc_start_workflow` with no role bindings. `respawn_existing_with_workflow_mcp` (~1207) — the old "respawn an existing session with `--resume` + workflow MCP via the daemon RPC, minting a fresh uid" helper — still exists but has no production caller (only a structural pin test).

## Proposed design

### Eligibility

A `role_sessions` entry `role -> uid` is honored only when ALL hold; otherwise the launch fails with `InvalidParams` / `Unauthorized` naming the reason (fail loud, don't silently fresh-spawn a role the caller asked to bind):

- The role exists in the workflow definition.
- The role is `Context::Persistent` (not `Fresh`).
- The role has `needs_mcp = false`.
- `uid` is present in `state.sessions` (a live daemon session on THIS host — no cross-host binding).
- The bound session's engine matches the role's TOML engine (a Claude role cannot bind a Codex session). `DaemonSession.session_type` vs `Role.engine.as_session_type()` — normalize the `"claude"`/`"claude-code"` spelling difference (the TUI's `Engine::as_session_type()` returns `"claude"` while the daemon spawn path uses `"claude-code"`; compare on the normalized form).
- The bound session is in the run's RESOLVED workspace (the same workspace whose worktree the run uses), so baseline message-counting reads the session's own transcript dir, not a different worktree. `check_session_caller` alone is insufficient — it admits any descendant/same-workspace target, which need not be the launch workspace.
- **Exclusivity** (review finding): the `uid` is NOT already a participant of another ACTIVE run (not present in any active run's `role_sessions`), the same `uid` is not assigned to two roles in this `role_sessions` map, and the session has no stale `workflow_run_id`/`workflow_role` tag pointing at a different active run. Without this, `daemon_owns_run` (uid-based) would let the OTHER run keep driving the same PTY after this launch overwrote its tags — two pollers fighting over one agent.
- The caller is authorized for `uid`: Session caller passes `check_session_caller(state, caller_uid, uid)`; Operator caller has already been token-validated at the dispatcher.

This eligibility set is exactly the worker in `feedback.toml` (persistent, `needs_mcp = false`) and any analogous "produce-then-hand-off-via-static-on_idle" role.

### Daemon bind path

A bound role must enter the run with a FULLY-RESOLVED sid + baselines — never a `None` sid that falls through to fresh-spawn discovery. Review finding (high): treating `transcript_path == None` as "no transcript yet → use the `RebindPending` listing-diff path" is wrong for an already-running session. That path discovers a NEW transcript file; a bound agent APPENDS to its EXISTING file, so the diff never finds anything (claude) and finalization explicitly excludes unbound persistent codex from causal discovery — the run wedges at `NoTranscriptId`. And a `0/0` baseline against pre-existing turns fires the gate immediately (see below + finding interplay).

So binding RESOLVES the sid eagerly, and rejects the bind if it can't:

```
for role_name in &wf.role_order {
    if let Some(uid) = role_sessions_param.get(role_name) {
        // BIND: validate eligibility (above). Resolve sid eagerly:
        //   1. DaemonSession.transcript_path → stem/payload.id, OR
        //   2. resolve the session's existing transcript by scanning its
        //      worktree's transcript dir for the session's own file.
        // If neither yields a sid, REJECT the bind (InvalidParams "bound
        // session has no resolvable transcript yet; retry once it has run a
        // turn") — do NOT fall through to new-file discovery.
        let sid = resolve_existing_sid(&state, uid)?;     // never None past here
        // Baselines captured NOW, against the session's OWN worktree, so the
        // gate counts only turns produced AFTER the goal is delivered:
        let assistant_baseline = count_messages(engine, sess_wt, &sid, Assistant);
        let user_baseline      = count_messages(engine, sess_wt, &sid, User);
        let text_baseline      = list_messages(engine, sess_wt, &sid, Assistant).len(); // text-bearing
        // Snapshot any accepted pre-launch plan so {{ roles.<role>.plan }} survives:
        let plan = latest_plan(engine, sess_wt, &sid);    // -> role_plans[role] if Some
        // Record the binding (sid is Some — sync won't have to fill it):
        role_sessions.insert(role_name, RoleBinding {
            session_label: role_name,
            current_session_id: Some(sid),
            daemon_session_uid: Some(uid),
        });
        role_baselines.insert(role_name, MessageBaseline { user_count: user_baseline, assistant_count: assistant_baseline });
        bound_text_counts.insert(role_name, text_baseline); // threaded into WorkflowRun::new for the initial role
        bound_plans.insert(role_name, plan);                // -> run.role_plans
        bound_uids.push(uid);                               // for tag-after-save + failure restore
        // NOTE: no spawn, no detector arm, no spawn-queue ticket.
        continue;
    }
    // ...existing fresh-spawn block unchanged...
}
```

Tagging order (review finding, med): the existing sessions are tagged with `(run_id, role)` ONLY AFTER `state.json` is durably saved (the same point the spawned-session path is considered committed). A save failure before that point leaves the pre-existing sessions untouched (no orphan tag pointing at a nonexistent run); the fresh-spawn cleanup already removes spawned sessions, and bound sessions simply keep their prior (untagged) state. `resolve_existing_sid` reuses the transcript-path → sid logic (claude file-stem / codex `payload.id`) currently used by `sync_participant_transcript_paths`; it may need to be lifted to a shared helper (Phase 1 scope note).

### Initial activation for a bound worker

`WorkflowRun::new` seeds the initial role's iteration-1 history entry and `start_workflow` records the initial `pending_activation { is_initial: true }`. For a bound worker the finalize drainer renders the initial prompt (the goal) and delivers body+Enter to the role's existing PTY — no `--resume`, no respawn (the `needs_mcp=false` guarantee). The bound worker enters with `current_session_id = Some(sid)` (resolved eagerly above), NOT `None`.

Two ordering hazards a bound worker hits that a fresh worker does not (review findings, high + med):

- **Idle-eval-before-delivery.** `poll_once` runs `sync_role_session_ids` and `evaluate_snapshot` BEFORE `drain_finalizations`, and `evaluate_snapshot` does not skip a run with an in-flight `pending_activation`. A fresh Claude worker is safe because its sid is `None` (the gate skips with `NoTranscriptId` until the worker has run). A BOUND worker has a real sid from tick one, so the gate evaluates it before the goal is ever delivered. With a correct baseline (= current count) the gate sees `count == baseline` and skips — but this is one off-by-one away from a premature `worker -> reviewer` fire. Mitigation: `evaluate_snapshot` (and the nudge path) must SKIP a run whose `pending_activation` targets the active role — the role is mid-hand-off; the drainer owns it. This is a small, general guard (it also tightens the fresh path) and removes the dependence on baseline arithmetic for correctness.
- **Readiness, not just PTY-quiet.** The doc originally claimed delivery is safe because it's gated on PTY-quiet "as for a fresh role." That is weaker for a bound agent: a long tool call or a paused mid-turn can be quiet for the window, and unlike a just-spawned fresh session the bound agent may be genuinely busy. Mitigation: before delivering to a bound session, require `role_turn_complete(engine, wt, sid)` (the turn actually finished) in ADDITION to PTY-quiet. (`sid` is always available for a bound role, so this check is always evaluable.)

### Baseline correctness

A bound worker that already produced N assistant turns gets `assistant_count` baseline = N, so the idle gate (`assistant_turn_completed_since`) fires `worker -> reviewer` only after a turn produced in RESPONSE to the delivered goal — not on the pre-existing turns. Beyond the turn-count baseline, two more values must be seeded for a bound INITIAL worker or the manager's prompt context is wrong (review finding, med):

- `initial_text_count` → `WorkflowRun::new` for the initial role, set to the bound session's text-bearing assistant count (`list_messages(...).len()`). The initial finalizer path is delivery-only and never computes `text_messages_at_start`; without seeding it, `{{ roles.worker.this_turn }}` would include the worker's PRE-launch text instead of only its post-goal turn. `prior_*` slicing keys off `role_baselines`; `this_turn` keys off `history[*].text_messages_at_start` — both must be set intentionally at bind time.
- `role_plans[role]` ← `latest_plan(...)` snapshotted at bind time, so an accepted pre-launch `ExitPlanMode` plan survives into `{{ roles.<role>.plan }}` rather than being lost once the worker produces its next turn.

### MCP tool

`start_workflow(task_id, workflow_name, goal="", role_sessions=None)` — `role_sessions` is an optional `dict[str, str]` (`role -> session_uid`). Forwarded verbatim in the daemon RPC params. Docstring documents eligibility (persistent + non-mcp roles only) and that an ineligible entry fails the launch.

### A-f UI

`enter_workflow_launch_confirm` offers, for each eligible role, the existing sessions in the focused workspace as `WorkflowSlotSource::Existing(si)` options alongside `New(engine)`. On submit, `launch_workflow_via_daemon` maps any `Existing` slot to a `role -> uid` entry and passes it to `rpc_start_workflow`. Ineligible roles (fresh / needs_mcp) offer only `New`. This re-lights the dormant `Existing` slot machinery rather than adding new UI primitives.

Important (review finding): the TUI must forward the DAEMON session uid, not the local `TerminalSession` UI handle. A `TerminalSession` carries a separate `Session.daemon_session_uid`; only daemon-owned sessions (those with `daemon_session_uid.is_some()`) are bindable, and the daemon's eligibility check requires `state.sessions` membership. So the `Existing`-slot chooser must (a) offer only sessions whose `daemon_session_uid.is_some()`, and (b) send THAT uid. A purely TUI-local/unattached session is not bindable and must not appear as an `Existing` option. The daemon rejects a non-`state.sessions` uid regardless, but the UI should filter so the user isn't offered an unbindable choice.

### Alternatives considered

| Alternative | Why rejected |
|---|---|
| Support binding `needs_mcp=true` roles via respawn-with-`--resume`+MCP | Adds the dormant `respawn_existing_with_workflow_mcp` complexity AND kills/respawns the agent, defeating the "keep the live session" benefit; no real use case (rarely a pre-existing manager session). Non-goal. |
| Allow binding `Context::Fresh` roles | Contradicts fresh semantics (`/clear` + reset every activation); the adopted context would be wiped on first activation anyway. Non-goal. |
| Silently fresh-spawn an ineligible bind request | Hides caller error; a worker the user expected to keep its context would start cold with no signal. Fail loud instead. |
| Keep baseline at 0/0 for bound roles | The idle gate would fire immediately on the bound session's pre-existing turns, handing off before the worker addressed the goal. Must derive baseline from the live transcript. |
| Bind with `current_session_id = None` and let `RebindPending` discover the sid | A bound agent appends to its EXISTING transcript, so new-file discovery never fires (claude) and codex causal discovery excludes unbound persistent → run wedges at `NoTranscriptId`. Resolve the sid eagerly at bind; reject if unresolvable. |
| Gate delivery on PTY-quiet alone (as fresh) | A bound agent can be quiet mid-(long)-tool-call; a fresh just-spawned one cannot. Require `role_turn_complete` + quiet for bound delivery. |

## Risks and open questions

- Risk: a bound worker that is mid-turn (not idle) when the goal is delivered. Mitigation: delivery requires `role_turn_complete(engine, wt, sid)` AND PTY-quiet for a bound session (not PTY-quiet alone) — the turn must actually be finished, not merely paused-and-quiet.
- Risk: a bound worker idle-evaluated before its goal is delivered (poll evaluates before the drainer finalizes the initial activation). Mitigation: `evaluate_snapshot` + the nudge path skip a run whose `pending_activation` targets the active role.
- Risk: a bound uid still owned by another active run (uid-based `daemon_owns_run`) → two pollers driving one PTY. Mitigation: exclusivity eligibility check (reject uids in any active run's `role_sessions`, duplicate uids, or conflicting workflow tags).
- Risk: orphaned `(run_id, role)` tag on a pre-existing session if the launch fails after tagging. Mitigation: tag bound sessions only AFTER `state.json` is durably saved.
- Risk: regressing the fresh-spawn path. Mitigation: `role_sessions` is additive and optional; absent → the existing loop runs unchanged. Phase 1 acceptance includes "all existing start_workflow + poller tests pass." The `evaluate_snapshot` pending-activation skip is the one change that touches the shared gate; it gets its own test asserting fresh runs are unaffected.
- Risk: a bound session that exits between the eligibility check and the run starting. Mitigation: same TOCTOU posture as the rest of `start_workflow` — the poller's `daemon_owns_run` precondition surfaces `SessionNotFound` if the session is gone by drive time; the run doesn't wedge silently.
- Open question (does not block Phase 1): should `A-f` show eligible existing sessions across the whole task tree or only the focused workspace? Phase 3 starts with focused-workspace only (matches the old behavior); widening is a later tweak.

## Implementation plan

### Phase 1: Daemon `start_workflow` honors `role_sessions` (bind path) [feedback]

- **Goal:** `start_workflow` accepts a `role_sessions` param and binds an eligible existing session to a role instead of fresh-spawning it — with an eagerly-resolved sid, correct turn/text/plan baselines, exclusivity + workspace/engine eligibility, tag-after-save, and the idle-eval + delivery-readiness guards.
- **Scope:** `daemon/src/control/methods.rs` (`StartWorkflowParams`, the role loop bind branch, eligibility validation, tag-after-save, threading text-count/plan into `WorkflowRun::new`), `daemon/src/workflow/poller.rs` (the `evaluate_snapshot`/nudge skip-when-pending-activation guard; `role_turn_complete`+quiet delivery readiness for bound sessions — likely in the finalize drainer), `daemon/src/workflow/run.rs` (reuse `RoleBinding`/`MessageBaseline`; confirm `WorkflowRun::new` takes the initial text count + plans, which it already does), a shared `resolve_existing_sid` helper lifted from `sync_participant_transcript_paths`.
- **Out of scope for this phase:** MCP tool surface (Phase 2), A-f UI (Phase 3), `needs_mcp=true` / fresh-role binding (non-goals).
- **Acceptance criteria:**
  - `StartWorkflowParams` gains `role_sessions: Option<BTreeMap<String, String>>` (`#[serde(default)]`); absent → byte-identical behavior to today (regression test).
  - A test: binding an eligible persistent/non-mcp role to a live session with an N-assistant-turn / M-text transcript records `RoleBinding { daemon_session_uid: Some(uid), current_session_id: Some(sid) }`, `MessageBaseline.assistant_count == N`, the initial history entry's `text_messages_at_start == M`, and `role_plans[role]` set when the session had an accepted `ExitPlanMode` (no fresh spawn, no detector armed for that role).
  - A test: a bound session whose sid is NOT resolvable (no transcript yet) is REJECTED with `InvalidParams` — it does NOT enter the run with `current_session_id = None`.
  - A test: eligibility rejections each return `InvalidParams`/`Unauthorized` naming the reason — `Context::Fresh` role, `needs_mcp=true` role, unknown role, non-existent uid, out-of-scope/other-workspace uid, engine mismatch, a uid already in another active run's `role_sessions`, and a duplicate uid across two roles in the map.
  - A test: an eligible bound worker drives `worker -> reviewer` only AFTER a post-goal turn — and is NOT idle-evaluated while its initial `pending_activation` is in flight (the `evaluate_snapshot` skip).
  - A test: a launch that fails AFTER eligibility but before/at save leaves the bound session's `workflow_run_id`/`workflow_role` tags unchanged (tag-after-save).
  - All existing `start_workflow` + poller tests pass (the `evaluate_snapshot` pending-activation skip must not change fresh-run behavior — asserted).
- **Dependencies:** none.

### Phase 2: MCP `start_workflow` tool exposes `role_sessions` [one-shot]

- **Goal:** the MCP tool forwards `role_sessions` so orchestrator agents can bind an existing worker session.
- **Scope:** `mcp_server/server.py` (`start_workflow` signature + params forwarding + docstring), `mcp_server/tests/`.
- **Out of scope for this phase:** A-f UI.
- **Acceptance criteria:**
  - `start_workflow(task_id, workflow_name, goal="", role_sessions=None)`; when provided, `role_sessions` rides in the daemon RPC params; when `None`, the params dict omits it (no wire change for existing callers).
  - Docstring states eligibility (persistent + `needs_mcp=false` roles only) and that an ineligible entry fails the launch.
  - A test asserts `role_sessions` is forwarded in the params when provided and omitted when `None`.
- **Dependencies:** Phase 1.

### Phase 3: A-f UI re-enables the Existing-slot chooser [feedback]

- **Goal:** the TUI `A-f` launch flow can bind an existing session in the focused workspace to an eligible role.
- **Scope:** `tui/src/app.rs` (`enter_workflow_launch_confirm` slot construction, the `Existing`-slot submit mapping in `launch_workflow_via_daemon`, `rpc_start_workflow` signature to carry `role_sessions`), `tui/src/client_session.rs` (`rpc_start_workflow`).
- **Out of scope for this phase:** task-tree-wide session selection (focused workspace only).
- **Acceptance criteria:**
  - For an eligible role, `enter_workflow_launch_confirm` offers `Existing(si)` options for the focused workspace's DAEMON-OWNED sessions (`daemon_session_uid.is_some()`) plus `New(engine)`; ineligible roles, and sessions without a daemon uid, offer/appear only as `New`.
  - Selecting an `Existing` slot threads `role -> daemon_session_uid` (NOT the local `TerminalSession` UI handle) into `rpc_start_workflow`; selecting `New` sends no entry for that role.
  - A test (handler-level, matching the existing `WorkflowLaunchConfirmMut` tests) asserts an `Existing` selection produces the expected `role_sessions` map keyed on the daemon uid, and a `New` selection produces none.
  - Existing A-f / launch tests pass.
- **Dependencies:** Phase 1 (Phase 3 is independent of Phase 2).

## Testing strategy

- Phases 1–3 each carry unit tests named above (daemon `start_workflow` bind + eligibility + baseline; MCP forwarding; TUI handler slot→`role_sessions` mapping). These pin the bind semantics without a live agent.
- End-to-end manual check after Phase 2: on cm-manager (or locally), start a session, let it produce a turn or two, then launch `feedback` via the MCP `start_workflow` with `role_sessions={"worker": "<uid>"}`; confirm the worker role binds that session (its sid, not a fresh one), the goal is delivered to it, and the run drives worker→reviewer→manager to `done` — and that a launch WITHOUT `role_sessions` still fresh-spawns (the headless e2e is unchanged).
- Regression guard: the existing daemon + TUI + Python suites must stay green at every phase (the fresh-spawn path is the load-bearing one for the cm-manager headless e2e).

## Rollout / migration

N/A — internal change, no data migration. `role_sessions` is an additive optional param; existing callers and on-disk state are unaffected.
