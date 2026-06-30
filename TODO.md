# TODO / Roadmap

Open threads, refreshed 2026-06-29 (late session). Priority order; each item has enough context to resume cold.

---

## ✅ Shipped (architecture / infra roadmap — all complete)

- **P0 — Session durability (S1–S5)** — daemon persist + restore-at-same-uid + resume; live on cm-manager, exercised by ~5 restarts this session (bug restore, re-parent, deploys), each re-resumed all sessions, no orchestrator double-spawn. Detail in `DESIGN_SESSION_DURABILITY.md`.
- **P1 — Frozen-pane UX** — resolved by P0 S4 (remote-reattach + `A-r` "reconnect now").
- **P2 — Global-host removal (Phases A–D)** — `active_host`/`A-H` cycler/HostHeader highlight gone; host is per-workspace. `DESIGN_REMOVE_GLOBAL_HOST.md`.
- **P2 — Two-column continuous panel** — shipped, then **reworked this session** (`DESIGN_CONTINUOUS_PANEL.md` revision): single `A-c` toggle (dropped `A-C` + master-hide), continuous tasks ONLY in the column (never main), respawn-robust nesting (`managed_by_uid` OR task-tree `parent_task_id`), and per-column cursor memory (re-entry lands where you left off).
- **P3 — `continuous.update` + autocompact** — deployed; bug-triage orchestrator on `compact_every: 16`.

## ✅ Also done this session (not previously on the list)

- **A-a junk-local-spawn fix** — attaching to an empty *remote* workspace re-arms the deferred reattach instead of spawning a local claude in `$HOME` (commit `2cd1afb`).
- **Disabled the resume-from-summary prompt on cm-manager** — `CLAUDE_CODE_RESUME_THRESHOLD_MINUTES=999999` in `~/.claude/settings.json`, so daemon restarts re-resume cleanly without parking large sessions at the menu (memory: `reference_disable_resume_summary_prompt`). Confirmed working across a restart.
- **Restored bug-001…006** from their transcripts (hand-crafted daemon manifest entries at their original uids) + **re-parented 001/003/004/005/006** to the live orchestrator uid so they nest robustly (survive being marked done).

---

## 🟠 Open

### P1 — bug-triage review/merge (active)
- **bug-001** — fix `2e0c17b2` (gate cancel/placement-failure pending decrement on aggressive attribution; +138-line regression test) **merged to origin/main** ✅. Cleanup: `A-d` mark done + close session → drops the worktree.
- **bug-002** — fix `47e85e94`/`b69afafd` (tolerate missing/empty kalshi `markets`) **merged to origin/main** ✅, task done + closed.
- **bug-003 / 004 / 005 / 006 / 007 / 008** — still need review + merge as you go through them. The restored ones (003/004/005/006) are re-parented, so marking done keeps them nested until you close the session.
- **Decision still pending:** pause the orchestrator? It's now spawning up to **bug-012** — churn while we stabilize.

### P3 — Other headless planning tools 🟢 (reads DONE locally; deploy + notify_user pending)
The READ tools (`list_tasks` / `get_task` / `get_current_task`) are **implemented + tested** (commit `0581a56`): daemon `list_tasks`/`get_task` RPC handlers (reuse `api_*` helpers); MCP routes through the daemon when headless, else PlanningClient; `get_current_task` composed from `ping` + `get_task`. daemon 442 + mcp 136 green.
- **Pending: cm-manager deploy** (daemon binary + mcp_server → another clean restart) for it to take effect there.
- **Still open: `notify_user`** — deferred. It needs a CROSS-HOST delivery path: on a headless host the user isn't attached, so a daemon `notify_user` would have to post to the planning API (a notifications row/endpoint the laptop TUI surfaces) or degrade to a logged no-op. Design the delivery, then add the daemon handler. (`update_task`'s status case already falls back to `set_subtask_status`; non-status `update_task` headless is also still cli-only.)
Memory: `reference_headless_planning_tools`.

### ~~Deferred bug — reattach "stranding"~~ ✅ FIXED (`d89928f`)
Both deferred-reattach paths now bound the FRESH-attach retry like the reconnecting-slot path: the synchronous fallback no longer drops a fresh entry on the first failure (the stranding), and the production `drain_attach_results` no longer re-queues a fresh failure forever (the spin — its `attempts >= MAX` give-up was reconnecting-only); the dispatcher's retry throttle now applies to fresh entries too. Give-up after `REMOTE_REATTACH_MAX_ATTEMPTS` preserves the raw entry in `skipped_manifest_entries`. Test `fresh_deferred_reattach_retries_then_gives_up_bounded`; TUI 659 green.

### P3 niceties (non-blocking)
- Update the orchestrator's `default_prompt` to steer subtask agents to `set_subtask_status` + `ssh trader` (the headless-tools fix above also makes `update_task`/`notify_user` work).
- TUI editor for `continuous.update`.

---

## Done earlier this session (context, not threads)

- `set_subtask_status` (headless status PATCH) + `update_task` → `set_subtask_status` fallback; `create_subtask` parent-deleted self-heal.
- ssh tunnel keepalive; off-thread session poll + attach; phantom-duplicate adoption dedup.
- api: exclude archived from `GET /tasks`; dispatch `kind` migration (011).
- `/merge-main remote` mode (fetch-before + push-after); installed the `merge-main` skill on cm-manager.
- Deployed current-main daemon + mcp_server to cm-manager.
