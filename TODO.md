# TODO / Roadmap

Open threads, captured 2026-06-29. Priority order; each item has enough context to resume cold.

---

## P0 — Session durability (the bug-002 kill) ✅ COMPLETE (S1–S5)

**STATUS: DONE.** persist (S1) + restore-at-same-uid (S2) + resume (S3) + continuous-orchestrator resume (S5) + TUI reattach (S4) are all committed (`17916fc` / `9d206ff` / `5a9cdb9` / `1bc059a` / `bdea0de` / `d3c91ba`). The daemon side (S1–S5) is **LIVE on cm-manager** — verified on prod: a restart restored BUG-007/008/009 AND the bug-triage orchestrator at their same uids, RESUMED, no scheduler double-spawn. S4 (TUI, runs locally — no deploy) turned out composition-complete (the remote-reattach machinery already handles daemon restart) + added an `A-r` "reconnect now" lever. The bug-002 class of kill is fixed end-to-end. **Resolves the P1 frozen-pane item too.** Only follow-up: autocompact for the resumed orchestrator's growing context (P3). Full detail in `DESIGN_SESSION_DURABILITY.md`.

**Agent sessions must survive daemon restarts.** A `systemctl restart cm-daemon` (every deploy) SIGKILLs all its PTYs → every ad-hoc subtask session dies and does NOT come back. bug-002's session was lost this way. **Unacceptable.**

Principle (memory: `feedback_sessions_user_owned_lifecycle`): a session is a **durable, user-owned** thing. The orchestrator may drive/nudge it however makes sense, but **only the user marks it done or deletes it**; a backend restart must never erase it.

- **Root cause:** the daemon keeps its session registry **in memory only** — only continuous tasks persist (via `~/.cm/continuous-tasks/*/state.json`). Ad-hoc sessions vanish on restart. Transcripts DO survive on disk (`~/.claude/projects/<encoded>/*.jsonl`), so resume is possible.
- **Fix:** daemon-side **session persistence + restore** — persist the registry to disk; on restart, re-spawn each not-done session resuming its transcript (`claude --resume <sid>` / codex resume). Then deploys + crashes become transparent: sessions reappear alive, with history.
- **Leverage:** this also makes every future daemon deploy painless (no session loss) — which de-risks `continuous.update` and all future backend work.

## ~~P1 — Frozen-pane UX (remote daemon restart)~~ ✅ RESOLVED by P0 S4

Was: when the remote daemon restarts the TUI showed a **frozen attach pane** and `A-r` didn't clear it. Fixed by P0 S4 (`d3c91ba`): the remote-reattach machinery already keeps the session reconnecting (`⟳`) and rebinds to the daemon-restored session once it's back (the daemon dies with no `End` frame → transport-EOF reconnect path, not the exited path); and `A-r` is now a "reconnect now" lever that accelerates/revives stuck reconnects.

## P1 — bug-triage review / merge (in progress) 🟠

- **bug-002** — fix `47e85e94` (tolerate missing/null/empty kalshi `markets`; +4 passing regression tests). Merge via **`/merge-main remote`** from a fresh session in its worktree → then `A-d` mark done.
- **bug-001 / 003 / 004 / 005 / 006 / 007 / 008** — investigate-only `NOTES.md`; read + decide per bug. ⚠️ **bug-001**: its proposed fix may be *insufficient* (the `set_delta` race) — needs the live-log dig via `ssh trader`.
- **Decision pending:** pause the orchestrator? It keeps spawning new bugs (now at 008) — churn while we stabilize.

## P2 — Global-host (`A-H`) removal 🟡

Tier-4 architectural change per `DESIGN_REMOVE_GLOBAL_HOST.md`: retire the global `active_host`; host becomes workspace-scoped. Large + touches host switching — start with the pre-coding audit + slice plan.

## ~~P2 — Two-column continuous panel (TUI)~~ ✅ DONE (S1–S5)

Shipped per `DESIGN_CONTINUOUS_PANEL.md`. `A-C` toggles a dedicated continuous column (terminal | main | continuous) where orchestrators carry their spawned subtasks nested (`├`/`└` tree, matched by `managed_by_uid`); `A-h`/`A-l` move the unified cursor between columns, `A-j`/`A-k` within. Persisted (`continuous_column_on` on the manifest). Came with the **keybinding reshuffle** (S1): retired the `A-H` host-switcher, hide→`A-H`, push/pull→`A-9`/`A-0`, freeing `A-h`/`A-l`. Local-only (TUI) — no deploy. TUI 659 green. (Possible follow-ups: multihost grouping inside the column; transitive nesting depth.)

## ~~P3 — `continuous.update` + autocompact~~ ✅ DONE (deployed)

`continuous.update` (operator-only, in-place load-modify-save of a live task's mutable config — `compact_every`, `default_prompt`, schedule, …, preserving run history) is committed + **live on cm-manager**. Autocompact switched from `/clear` (wipe) to **`/compact` (summarize)** per user — a compact fire delivers `/compact` alone (it's an async turn) and the prompt resumes the next fire. The bug-triage orchestrator now has **`compact_every: 16`** (~48h) set via `continuous.update` (run_count preserved = no recreate). Its context is now bounded across fires/restarts.

Remaining nicety (not blocking): updating the orchestrator's `default_prompt` to steer subtask agents to `set_subtask_status` + `ssh trader` (not `update_task`/gcloud) — `continuous.update` now supports it; just needs the desired prompt text. And TUI integration for `continuous.update` (an editor) whenever the two-column continuous panel (P2) lands.

## P3 — Other headless planning tools 🟢

`list_tasks` / `get_task` / `get_current_task` / `notify_user` are cli-routed → fail headless (memory: `reference_headless_planning_tools`). `update_task`'s status case is already fixed (daemon fallback). Daemon-route the rest if agents need them.

---

## Done this session (context, not a thread)

- `set_subtask_status` (headless status PATCH) + `update_task` → `set_subtask_status` fallback.
- `create_subtask` top-level fallback (parent-deleted self-heal); recreated the deleted orchestrator parent row; re-linked the 6 subtasks; flipped bugs to `blocked`.
- ssh tunnel keepalive (hung-tunnel auto-reconnect); off-thread session poll + attach (TUI responsiveness); phantom-duplicate adoption dedup.
- api: exclude archived from `GET /tasks`; dispatch `kind` migration (011) deployed (fixed the crash-loop).
- `/merge-main remote` mode (fetch-before + push-after).
- Deployed the current-main daemon (global_perms + my features) + mcp_server to cm-manager — consistent.
- Docs: cm-manager `ssh trader` (CLAUDE.md); `gcp-instances` skill (pushed to predictionTrading).
