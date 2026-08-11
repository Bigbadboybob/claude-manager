# Agent-side MCP UX notes — from a heavy orchestration session (2026-08-10)

Context: I'm the Claude session driving the ES v2 program on predictionTrading — this session alone launched ~10 workers via create_subtask + start_session, ran a review wave, killed/relaunched a botched fan-out, and consumed a dozen monitor wake-ups. These are the friction points as experienced from the agent side, most severe first, each with a concrete suggestion.

## 1. start_session on a propose_task task silently binds to MY workspace (caused a live incident today)

I proposed three main-based bug-fix tasks via propose_task, then start_session(task_id=...) on each — the documented propose-then-launch path ("the daemon records a creator edge at mint time, so propose-then-launch works"). What actually happened: proposed tasks have NO workspace until the owner accepts them in the TUI, and start_session doesn't mint one — so all three workers spawned into MY OWN worktree, followed their "branch off main" instructions by switching HEAD under my live session, and left uncommitted edits in my tree. The owner caught it from the TUI sidebar before I did; recovery cost a kill-sweep, a patch salvage, and stray-branch deletion.

Suggestions, in preference order: (a) start_session on a workspace-less task should MINT a workspace (that's what "propose-then-launch works" implies it does); or (b) refuse loudly ("task has no workspace — worker would run in YOUR worktree; pass allow_shared_workspace=true to override"); at minimum (c) the start_session return should include worktree_path so the caller can verify placement without a follow-up list_sessions — today the return carries only session_uid + monitor, and the collision was invisible until list_sessions.

## 2. No way to choose the worktree base — "cut from parent wip_branch" forces a reset dance

create_subtask(worktree_mode="branch") cuts from the parent task's CURRENT local wip_branch. For same-lane subtasks that's right. But "fix this on main so the owner can merge it ahead of my branch" is a routine need (it's why I reached for propose_task in incident #1), and today the only way is instructing the worker to `git fetch && git reset --hard origin/main` as step 0 — a side-channel workaround that relies on the worker reading carefully, and which in a SHARED checkout (incident #1) is exactly what did the damage.

Suggestion: `create_subtask(base="origin/main")` (any committish; default = parent wip_branch). Also: return the base commit sha in the create_subtask result so the launcher can verify the cut without cd-ing into the worktree — the "worktree cut from stale/wrong base" family of confusions (we've hit at least three variants) all stem from the base being invisible at creation time.

## 3. Monitors are one-shot and fire on ANY turn end — interim turns and kills read as "done"

The auto-monitor from start_session fires the first time the worker ends a turn. Two false-positive shapes today: (a) my round-3 reviewer ended its launch turn with "four background subagents now running" — monitor fired, session shows awaiting_input, and I had to know not to treat that as completion, then manually re-arm via monitor_sessions; (b) the three killed workers each fired their monitor on death, delivering "(exited): [tool_use: Edit (file_path: ...)]" — a truncated mid-action tool call presented as if it were a final message, indistinguishable at a glance from a report.

Suggestions: a `mode="final"` (or `until="task_done"`) watch that auto-re-arms across interim turns and only wakes me on session exit or an explicit report_done; kill-triggered firings labeled "KILLED by <uid>" instead of surfacing a random truncated tool_use as the last message; and when a turn ends with live background subagents, say so in the wake-up ("turn ended; N background tasks still running") — the daemon may not know about in-session subagents, but even flagging "this was turn 1, no report yet" would prevent the misread.

## 4. status="awaiting_input" conflates "done and reporting" with "idle mid-workflow"

Everything lands in awaiting_input: a worker that just delivered its verdict-first final report, a worker whose background fan-out is still running, a worker stuck on a permission prompt. I disambiguate by reading the last message and pattern-matching for "VERDICT:" — workable because our briefs mandate verdict-first reports, but that's a convention doing a status field's job. Suggestion: let a worker mark itself done (report_done already exists — surface it as status="reported" in list_sessions/read_last_turn), and flag permission-prompt-blocked distinctly if detectable from the PTY.

## 5. Smaller frictions

- create_subtask not auto-spawning is fine as a design choice, but the return should say it: `{"task_id": ..., "worktree_path": ..., "launched": false}` — the "task created, nothing running, prompt sitting undelivered" trap has its own memory file on my side.
- kill_session on an already-gone session returns generic not_found — indistinguishable from a typo'd uid. "already exited at <ts>" would make the benign case self-evident.
- The create_subtask prompt and the start_session prompt overlap awkwardly: the task carries a prompt, but the session needs its own prompt to actually deliver instructions, so I duplicate ("execute your task per its prompt"). If start_session(task_id=...) with an empty prompt auto-delivered the task's stored prompt, the duplication disappears and the not-delivered trap dies with it.
- list_sessions is the tool that SAVED us today (worktree_path per row made the collision visible) — keep that field prominent. A `workspace_shared_with` hint (listing other live sessions in the same worktree) would have turned the incident into a pre-launch warning.

## What's working well (keep)

read_last_turn as the tail-first default is exactly right and I use it constantly; the async monitor wake-up pattern (register → end turn → get woken) fits the agent execution model perfectly and beats any polling API; start_session wait=true as a one-call spawn-and-collect; verdict-first report convention + read_last_turn compose into a clean orchestration loop; per-worker auto-monitors on start_session mean zero-ceremony fan-outs when things go right.

---

## Triage log

### 2026-08-11 — cm/ux-notes-triage

| # | Item | Disposition | Where / why |
|---|------|-------------|-------------|
| 1a | workspace-less task: mint a worktree | task-filed | planning task `eb043d87` (with 1b — mint-vs-refuse is one policy decision; mint preferred) |
| 1b | …or refuse loudly with override | task-filed | same task `eb043d87` |
| 1c | `start_session` returns `worktree_path` | implemented | daemon `mcp_start_session` return decoration + TUI twin + server.py passthrough (incl. `wait=true` via `_with_spawn_fields`); `task_id` when bound |
| 2a | `create_subtask(base=<committish>)` | implemented | `CreateSubtaskParams` + TUI twin + tool signature; `worktree.rs::resolve_base_commit` (local rev-parse → one fetch → `origin/<ref>`/`FETCH_HEAD`); resolved in the pre-POST precondition block so a typo is `invalid_params` with zero side effects; branch-mode only. Review hardening: `is_single_revision()` gate — the raw ref reached `git fetch` as a REFSPEC, so `+src:dst` could force-overwrite local branches (confirmed HIGH, fixed + victim-branch regression test) |
| 2b | return `base_sha` | implemented | both return sites, all worktree modes (`worktree_head_sha`) |
| 3a | `mode="final"` / re-arming monitors | task-filed | planning task `8c0ab1a3` (with 4a — needs the done signal as its termination predicate) |
| 3b | label kill-triggered firings | implemented | tombstone `killed`/`killed_by`/`exited_at` via bounded `kill_requests` ledger; fire message `killed by <who> at <ts>` with transcript labeled "fragment before kill", never as a final report |
| 3c | flag interim turns in wake-ups | implemented | `idle_source=="transcript"` → "PTY still active" line; idle-but-alive → "may be an interim turn". Review fix: mixed batches (exited completer + live siblings) no longer emit the all-EXITED trailer |
| 4a | `status="reported"` done signal | task-filed | task `8c0ab1a3` (fold-ins: cgroup-kill provenance, `read_last_turn` kill labels, sweep attribution) |
| 4b | detect permission-prompt-blocked | declined | needs a PTY-content classification layer that doesn't exist; poor value/cost today — revisit if it keeps biting |
| 5a | `create_subtask` returns `launched: false` | implemented | both return sites + docstring ("nothing is running until start_session") |
| 5b | `kill_session` "already exited at <ts>" | implemented | tombstone lookup on both miss paths (+ killer when known); TUI twin via manifest tombstones; `not_found` code kept for compat |
| 5c | empty prompt auto-delivers task prompt | implemented | EXPLICIT `task_id` (or `isolated=true`) only — inherited bindings stay promptless (spawn-then-drive pattern); agent types only, never bash (its "prompt" would execute); best-effort, never fails the spawn; `prompt_source` in return; auto-monitor counts auto-delivered prompts |
| 5d | `workspace_shared_with` hint | implemented | computed MCP-side strictly from caller-authorized rows; TUI-owned rows now push `workspace_id`/`worktree_path` (serde-defaulted), TUI twin emits `worktree_path` too |

Process: 4 implementation agents in isolated worktrees (branches `worktree-wf_6d72c26a-d5d-1..4`, kept as safety copies), merged conflict-free; 6-lens adversarial review — 22 findings → 13 verified by 3-refuter majority → 2 confirmed (both fixed: the fetch-refspec injection, the mixed-batch trailer) + 3 low advisories fixed at triage (inherited-binding auto-delivery, `delivered_a_prompt` whitespace mismatch, skew-warning wording). Verified: `cargo build --workspace` clean; 104 daemon + 24 TUI + 62 mcp_server tests pass.

Remaining advisories (recorded, not fixed): `FETCH_HEAD` can be substituted by a concurrent fetch in the same main repo (narrow race, base still validated); the improved `TargetNotInRegistry` miss message reveals `exited_at`/killer for tombstoned uids to any session caller (refuted as a regression by majority — requires knowing the uid; prior message already confirmed absence); cgroup/OOM kills read as natural exits (folded into task `8c0ab1a3`).

Activation: daemon changes need a daemon restart (cm-manager: redeploy binary + `mcp_server/`; auto-delivery needs `api_url`/`api_token` in `daemon.toml`), MCP-server changes on next MCP spawn, TUI changes on next TUI build. predictionTrading's MCP copy is retired (its launcher execs this repo's `server.py`) — no mirroring needed.
