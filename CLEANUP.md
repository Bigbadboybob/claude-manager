# Architecture & Code Cleanup

A prioritized list of refactors. File:line references current as of 2026-05-08. Items are ordered by leverage (impact ÷ effort, with dependencies pulling earlier items up). **Pick the top 2–3 and ship them.** Treat everything below position 5 as a parking lot and revisit quarterly.

Item #1 is a decision, not code. Make it before touching anything in `api/`, `dispatch/`, or `worker/` — it gates #8, #9, #10, and #11.

## Quick reference

| # | Item | Effort | Depends on |
|---|---|---|---|
| 1 | Decide whether cloud is alive, frozen, or gone | Decision (+ days if excising) | — |
| 2 | Centralize transcript and session-id lookup | ~1 day | — |
| 3 | Single-source task schema and status values | ~1 day | — |
| 4 | Carve the workflow runner out of `app.rs` | 1–2 days | — |
| 5 | Watch `~/.cm/workflow-runs/`, don't just load it once | Half a day | #4 |
| 6 | Replace pending-write slots with a typed delivery queue | ~1 day | — |
| 7 | Tighten Python error handling | Few hours | — |
| 8 | Split backend polling from cloud transfer | Half a day+ | #1 |
| 9 | Resolve the planning system's dual identity | Days | #1 |
| 10 | Consolidate config loading | Few hours | #1 |
| 11 | Add an integration test for the dispatch flow | Half a day | #1 (only if cloud stays) |
| 12 | Audit `app.rs` `.unwrap()`s in production paths | An hour | — |
| 13 | Smaller items | Various | — |

---

## 1. Decide whether cloud is alive, frozen, or gone

**Problem.** CLAUDE.md says "local + worktrees turned out to be much smoother and is now the default mode." But the cloud path is fully wired: FastAPI server, dispatch daemon, GCP VM lifecycle, warm pools, Postgres migrations, GCS push/pull. Half the Python codebase exists for it, plus most of the surface area in `dispatch/` and `api/`. Keeping it half-alive is the largest hidden tax in the repo.

**Three honest options.**

| Option | What it means | Cost |
|---|---|---|
| **A. Keep maintained** | Add a smoke test that exercises dispatch end-to-end at least monthly. Treat `api/`, `dispatch/`, `worker/`, `sql/` as first-class. | Real maintenance overhead; need the integration test from #11. |
| **B. Freeze** | Stop touching it. Document "cloud is frozen at v0.X; use this commit." Banner in CLAUDE.md. Skip cloud code in reviews. | Honest about reality. Bit-rot is bounded by frozen scope. |
| **C. Excise** | Move `api/`, `dispatch/`, `worker/`, `sql/`, GCS push/pull into a `cloud/` subdirectory or a separate branch. Strip cloud-only fields from the API client. | Several days of surgery, but `app.rs` and the Python codebase get materially smaller. The simplification cascades into #8, #9, and #10. |

**Recommendation.** B in the short term, C if you confirm you haven't used cloud in 1–2 months. Don't do A unless you genuinely need cloud — the volume of code for "rarely used" is high.

---

## 2. Centralize transcript and session-id lookup

**Problem.** Session identity and transcript location are duplicated:
- `tui/src/app.rs:737–812` scans Claude/Codex transcript locations to detect new session ids.
- `tui/src/workflow/transcript.rs:30–62` has its own Claude path encoding and Codex recursive scanner.
- `tui/src/backend.rs:317–349` has another `get_project_path()` / `find_latest_session()` implementation for push-to-cloud.
- Workflow rebinding, manifest restore, and transcript templating all depend on these being exactly aligned.

This is subtle, high-risk code: a path-encoding tweak, Codex JSONL shape change, or transcript rotation behavior can break workflow context, push/pull, or session restore differently in each caller.

**Suggested approach.**
1. Add a small `tui/src/agent_store.rs` (or `transcripts.rs`) with an `AgentEngine` enum and methods like `list_sessions(worktree)`, `find_session(engine, worktree, id)`, `detect_new_session(engine, worktree, baseline)`, and `latest_session(engine, worktree)`.
2. Move Claude path encoding and Codex first-line metadata parsing there.
3. Have `workflow::transcript`, `App` session-id detection, and `backend` push/pull use that module.
4. Add tests with temporary Claude and Codex directory shapes, including transcript rotation after `/clear`.

**Effort.** Closer to a day than half. Path encoding has corner cases (project-path encoding, transcript rotation, Codex's date-nested directories) and the temp-dir test setup is fiddly. Worth doing — it's a precursor to #4 because it removes one class of filesystem detail from `app.rs`.

---

## 3. Single-source task schema and status values

**Problem.** The task contract is hand-copied across every layer:
- `api/models.py:5, 22, 44` — Pydantic create/update/response models.
- `tui/src/api.rs:11, 47` — Rust `Task` and `TaskCreateBody`.
- `tui/src/planning.rs:19` — separate `PlanStatus` enum with string aliases (`blocked` maps to backlog, `in_progress` maps to running).
- `mcp_server/server.py:57, 61` — MCP response field allow-lists.
- `sql/004_planning_fields.sql` and `sql/005_add_archived_status.sql` — status check constraints.
- `cli/api_client.py` and `cli/planning_client.py` — untyped dict payloads.

This makes ordinary changes expensive. Adding `archived`, `source`, `is_cloud`, or a future `workspace_id` means remembering every copy, and the compiler can only help in the Rust slice.

**Suggested approach.**
1. Define status values once in the API (`Enum`/`Literal`) and use the same list in migrations/tests.
2. Use FastAPI's OpenAPI output to generate the Rust API structs, or at least add a schema-contract test that checks `api.models.TaskResponse` fields against `tui/src/api.rs` and the MCP field lists.
3. In Rust, introduce a `TaskStatusApi` enum instead of passing raw strings through `Task`, `PlanStatus::from_str`, and update-field maps.
4. Keep `PlanStatus` only if it is truly a presentation enum. If so, make the lossy mappings explicit (`blocked -> backlog column`) and tested.

**Effort.** About a day for a pragmatic contract-test version; more if you fully generate clients. High leverage because planning, MCP, API, and TUI are all changing this surface.

---

## 4. Carve the workflow runner out of `app.rs`

**Problem.** `tui/src/app.rs` is 6,926 lines. The `App` struct owns UI state, input handling, session lifecycle, workflow coordination, and config loading — all on one type. The three `impl App` blocks (lines 645, 5327, 6371) plus a `WorkflowResolver` impl (5223) live in the same file. Most of this is workflow-driver logic intermixed with rendering.

**Evidence.**
- `app.rs:645–5222` — main `impl App` block, ~4,500 lines, mixes everything.
- `app.rs:5223–5326` — `WorkflowResolver` impl that bridges to `workflow::template::RoleResolver`. This is the natural seam.
- `tui/src/workflow/` already contains `events.rs`, `run.rs`, `template.rs`, `toml_schema.rs`, `transcript.rs`, `spawn.rs` — the sub-module pattern works and has 51 tests. The driver side just hasn't moved yet.

**Suggested approach.**
1. Define a `WorkflowController` (or `WorkflowDriver`) struct in `tui/src/workflow/controller.rs` that owns: active `WorkflowRun`s, role-to-session bindings, pending transitions, event-tail offsets.
2. Move methods that mutate workflow state out of `App` into `WorkflowController`. `App` keeps a `controller: WorkflowController` field.
3. The seam: `WorkflowController` has a method like `tick(&mut self, sessions: &mut SessionMap) -> Vec<ControllerEffect>` that returns intents (e.g. "spawn role X with prompt Y"). `App` interprets effects against its session/PTY layer. This keeps the controller testable without a real terminal.
4. After the controller is out, the natural follow-ups are extracting `PlanningView` (already its own file but tightly coupled via `App` mutations) and a `SessionLifecycle` module.

**Effort.** ~1–2 days for the controller extraction alone. High leverage — every future workflow feature gets easier and `app.rs` shrinks materially. Cleaner if #2 ships first.

---

## 5. Watch `~/.cm/workflow-runs/`, don't just load it once

**Problem.** `workflow::run::load_all()` (`tui/src/workflow/run.rs:317`) is called at TUI startup and populates `App.workflow_runs`. The events file is tailed live (`workflow/events.rs:67`, `read_new`), but the `WorkflowRun` JSON metadata isn't re-read after startup. If a run mutates externally (e.g. you open a second TUI, or a tool edits the file), the running TUI doesn't notice.

**Suggested approach.**
- Add a `notify` (or `notify-debouncer-mini`) watcher on `~/.cm/workflow-runs/` in the background thread (`tui/src/backend.rs`).
- On change, send a `BackendEvent::WorkflowRunsChanged` and have the controller (see #4) re-load the affected run.
- Keep the events.jsonl tailer as-is — it's the hot path; the watcher is for the rarer metadata-mutation case.

**Effort.** Half a day. Low risk because the failure mode is "stale until next restart," which is what we have today.

---

## 6. Replace the pending-write slots with a typed delivery queue

**Problem.** The specific Codex failure documented in `BUGS.md` is already fixed in current code: the prompt branch waits for `ts.pending_clear.is_none() && ts.pending_enter.is_none()` (`tui/src/app.rs:1849`) before delivering the prompt, and `Session::new()` enables Kitty keyboard tracking (`tui/src/session.rs:60`). But the architecture is still brittle: input delivery is encoded as three independent optional slots (`pending_clear`, `pending_prompt`, `pending_enter`) plus ordering comments. `deliver_pending_write()` would still overwrite `pending_enter` if any future caller bypasses the exact drain-loop guard.

**Evidence.** `tui/src/app.rs:1837, 1851` — the `take().unwrap()` calls on `pending_clear` and `pending_prompt` indicate hand-rolled queue logic that's grown brittle.

**Suggested approach.**
- Replace the ad-hoc `Option<PendingWrite>` + `Option<PendingEnter>` slots with a single `VecDeque<PendingDelivery>` where `PendingDelivery` is an enum (`Write { text, gate }`, `Enter { gate }`, `Clear { gate }`).
- The delivery loop pops from the head when the gate is satisfied.
- Add a regression test that models `/clear` followed by a prompt and asserts two Enter deliveries. The current code should pass behaviorally; the test pins it before refactoring.
- Remove or update the stale `BUGS.md` entry once the regression test exists.

**Effort.** Half a day to a day. Makes input delivery understandable instead of ad-hoc.

---

## 7. Tighten Python error handling

**Problem.** Bare `except Exception:` blocks swallow real bugs. Located at:
- `dispatch/vm.py:90`, `dispatch/vm.py:100` — VM lifecycle (acceptable; logs and continues).
- `api/dispatch_daemon.py:30, 109, 134, 155, 234` — dispatch loop (acceptable as a daemon, but should at least distinguish transient from permanent errors).
- `api/main.py:114, 140, 221` — **request handlers**. These mask bugs and return generic 500s.

**Suggested approach.**
- Request handlers (`api/main.py`): replace bare excepts with specific exception types (`asyncpg.PostgresError`, `httpx.HTTPError`, `ValueError`). Let unexpected exceptions propagate so FastAPI logs the traceback.
- Dispatch daemon: keep the broad catch at the outer loop level (so the daemon survives), but inside, narrow to expected failure modes (network, GCP API, SSH timeout) and log the type explicitly.

**Effort.** A few hours. Independent of everything else. Worth doing alongside #1 since it touches the same files.

---

## 8. Split backend polling from cloud transfer operations

**Problem.** `tui/src/backend.rs` is doing three jobs in one background thread: API polling, planning sync, and cloud push/pull. The loop calls `do_refresh()` and `do_refresh_plan_tasks()` every five seconds; both currently fetch `client.list_tasks(None)` (`backend.rs:295` and `backend.rs:676`), so the TUI pulls the entire task table twice per tick and filters planning tasks client-side (`backend.rs:672–682`) even though the API already supports `project` filtering — there's even a comment in the code acknowledging this.

Cloud transfer also shells out to `git` and `gcloud` in the same module (`backend.rs:382–620`), with several best-effort commands whose failures are ignored until a later step happens to fail. That makes the background thread hard to reason about and hard to test.

**Suggested approach.**
1. Split into `api_sync.rs` (task/planning polling and update commands) and `cloud_transfer.rs` (push/pull, GCS, VM fallback).
2. Make planning refresh use an API-supported filter instead of `list_tasks(None)` + local `project.is_some()` filtering. If the current API cannot express "project is not null", add that endpoint before optimizing the TUI.
3. Return typed progress/error events from cloud transfer. Treat ignored `git commit`, `gcloud storage cp`, and fallback SSH failures as explicit states.
4. If #1 chooses to excise cloud, delete most of `cloud_transfer.rs` instead of polishing it.

**Effort.** Half a day for the split and duplicate-fetch fix; longer if you make cloud transfer fully typed. Depends on the cloud decision.

---

## 9. Resolve the planning system's dual identity

**Problem.** `PLANNING.md` describes a filesystem design (markdown task files in `~/.cm/projects/<project>/tasks/*.md`, `order.json` for ordering). The implementation (`tui/src/planning.rs`, 2,874 lines) reads from the FastAPI server, which reads Postgres. The filesystem layout still exists for repo discovery (`~/.cm/projects/*/repo_url`). This is a half-finished migration that's load-bearing in both directions:

- `mcp_server/server.py` and `cli/planning_client.py` always hit the API.
- `tui/src/planning.rs` is hardwired to API task schema (`crate::api::Task`).
- But the filesystem path is still the source of truth for the "list of projects" — repos are discovered by walking `~/.cm/projects/`.

**Suggested approach.** This couples to #1:
- If cloud stays (option A): commit fully to API-as-truth. Move repo discovery into the API. Delete `PLANNING.md`'s filesystem design or rewrite it to reflect reality.
- If cloud goes (option C): rebuild planning on the filesystem (markdown + JSON ordering) as `PLANNING.md` originally described. The TUI reads files directly. No backend needed for local planning. This is dramatically simpler and matches the "local default" direction.

**Effort.** Coupled to #1. Don't tackle this in isolation.

---

## 10. Consolidate config loading

**Problem.** Defaults and `.env` parsing happen in three places:
- `tui/src/config.rs` (Rust loader for `~/.config/claude-manager/.env`).
- `dispatch/config.py` (Python loader for the same file).
- The deployed cloud service's systemd environment, documented in `CLAUDE.md`, which must be kept in sync with the code-level `CM_*` defaults.

Both Rust and Python independently discover repos by walking `~/.cm/projects/*/repo_url`. Both define `CM_*` env-var names in their own constants. Planning adds a third source: `tui/src/planning.rs:471` has a hard-coded `repo_url_for_project()` fallback (with a comment "keep in sync with dispatch/config.py REPOS"), so a new project created in the planning view can write `repo_url` to disk but still create tasks against a guessed GitHub URL.

**Suggested approach.**
1. Single canonical list of env-var names — write them in `dispatch/config.py` as the Python source, and have `tui/src/config.rs` reference the same names with a comment pointing to the Python file. Or, better, generate the Rust list from the Python module at build time (if this is worth it — probably not for ~6 vars).
2. Pull repo discovery into one helper. Right now there's a Rust copy, a Python copy, and a planning fallback. Pick one canonical source: since the TUI needs it at runtime and the Python only needs it for the API (which may go away — see #1), the Rust/local project registry is the most likely owner. The Python copy can be deleted if cloud goes.
3. Make `PlanningView::create_task()` read the selected project's stored `repo_url` instead of calling a hard-coded `repo_url_for_project()` fallback.

**Effort.** A few hours. Cleanest after #1 is decided.

---

## 11. Add one integration test for the dispatch flow

**Problem.** Tests are concentrated in low-risk code:
- ~51 unit tests in `tui/src/workflow/*` — mostly TOML parsing and event decoding (the most static parts).
- ~13 tests in `app.rs` (the riskiest 6,926-line file).
- Zero tests on the Python side.
- Zero coverage for the dispatch → VM → ttyd → Claude path. This is the one path most likely to silently break.

**Suggested approach.** Skip if you take option C in #1. Otherwise:
- Add a Python integration test that mocks the GCP API client and walks the dispatch daemon through: claim task → launch worker → SSH-deliver prompt → mark done.
- Use `pytest` + `pytest-asyncio`. Mock `dispatch/vm.py:launch_worker` and the SSH boundary.
- One end-to-end test catches 80% of regressions in this surface area.

**Effort.** Half a day if you do it once. The value compounds — every dispatch change gets cheaper to verify.

---

## 12. Audit `app.rs` `.unwrap()`s in production paths

**Problem.** Five `.unwrap()`s in `app.rs`:
- `app.rs:1072` — `cloud_vm.unwrap()` — assumes a cloud VM exists. Will panic if launched against a malformed task.
- `app.rs:1837, 1851` — taking `pending_clear` / `pending_prompt` (related to #6).
- `app.rs:5388` — `names.into_iter().next().unwrap()` after a non-empty check.
- `app.rs:5449` — context unclear without re-reading.

**Suggested approach.** Convert `1072` to a logged-and-ignored path (cloud task without a VM is a state inconsistency, not a panic). The pending-* unwraps disappear with #6. The `next().unwrap()` is fine if the non-empty check is structurally enforced — verify and add a comment if so.

**Effort.** An hour. Mostly defensive.

---

## 13. Smaller items

- `cli/main.py` is 544 lines — large for a CLI. Subcommand handlers could move into `cli/commands/<name>.py`.
- `tui/src/planning.rs` (2,874 lines) is the second-largest Rust file. Same story as `app.rs`: rendering + input + state mutation in one place. Lower priority because it's less coupled than `app.rs`.
- If planning stays in Rust/API form, split `tui/src/planning.rs` into `model`, `layout`, `editor`, `actions`, and `render` modules before adding major board features.
- `mcp_server/server.py` and `cli/planning_client.py` both construct PlanningClient-shaped objects — verify they share a single class and aren't divergent copies.
- `tui/src/backend.rs` (688 lines) bundles API HTTP, GCS push/pull, and the background-thread dispatch. If cloud goes (#1), this file shrinks dramatically.
- The MEMORY note about "Two MCP server copies" (planning tools must be added to BOTH `mcp_server/server.py` AND `predictionTrading/scripts/mcp/claude_manager_server.py`) — that's a cross-repo duplication. Worth at minimum a comment at the top of `mcp_server/server.py` noting the sibling location.
