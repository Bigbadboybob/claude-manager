# TODO / Roadmap

Open threads, captured 2026-06-29. Priority order; each item has enough context to resume cold.

---

## P0 — Session durability (the bug-002 kill) 🟢 S1+S2+S3 done (not deployed)

**STATUS:** persist (S1) + restore (S2) + resume (S3) are committed (`17916fc` / `9d206ff` / `5a9cdb9`) and verified locally (unit tests + real daemon SIGKILL→restart→same-uid-restore loop). **Deploy to cm-manager pending user coordination** (the restart kills live sessions, then restore brings them back resumed — do S1+S2+S3 together). Remaining: S4 (TUI reattach coordination + frozen-pane UX — pairs with the P1 item below) + S5 (continuous-task resume). Full detail in `DESIGN_SESSION_DURABILITY.md`.

**Agent sessions must survive daemon restarts.** A `systemctl restart cm-daemon` (every deploy) SIGKILLs all its PTYs → every ad-hoc subtask session dies and does NOT come back. bug-002's session was lost this way. **Unacceptable.**

Principle (memory: `feedback_sessions_user_owned_lifecycle`): a session is a **durable, user-owned** thing. The orchestrator may drive/nudge it however makes sense, but **only the user marks it done or deletes it**; a backend restart must never erase it.

- **Root cause:** the daemon keeps its session registry **in memory only** — only continuous tasks persist (via `~/.cm/continuous-tasks/*/state.json`). Ad-hoc sessions vanish on restart. Transcripts DO survive on disk (`~/.claude/projects/<encoded>/*.jsonl`), so resume is possible.
- **Fix:** daemon-side **session persistence + restore** — persist the registry to disk; on restart, re-spawn each not-done session resuming its transcript (`claude --resume <sid>` / codex resume). Then deploys + crashes become transparent: sessions reappear alive, with history.
- **Leverage:** this also makes every future daemon deploy painless (no session loss) — which de-risks `continuous.update` and all future backend work.

## P1 — Frozen-pane UX (remote daemon restart) 🟠

When the remote daemon restarts (tunnel survives, sessions vanish *under* it), the TUI shows a **frozen attach pane**; `A-r` doesn't clear it. Distinct from the tunnel-keepalive reconnect work (there the *tunnel* dies). Need: detect "tunnel up but session gone/respawned" and surface it cleanly (exited — or, post-P0, restored). Pairs with P0.

## P1 — bug-triage review / merge (in progress) 🟠

- **bug-002** — fix `47e85e94` (tolerate missing/null/empty kalshi `markets`; +4 passing regression tests). Merge via **`/merge-main remote`** from a fresh session in its worktree → then `A-d` mark done.
- **bug-001 / 003 / 004 / 005 / 006 / 007 / 008** — investigate-only `NOTES.md`; read + decide per bug. ⚠️ **bug-001**: its proposed fix may be *insufficient* (the `set_delta` race) — needs the live-log dig via `ssh trader`.
- **Decision pending:** pause the orchestrator? It keeps spawning new bugs (now at 008) — churn while we stabilize.

## P2 — Global-host (`A-H`) removal 🟡

Tier-4 architectural change per `DESIGN_REMOVE_GLOBAL_HOST.md`: retire the global `active_host`; host becomes workspace-scoped. Large + touches host switching — start with the pre-coding audit + slice plan.

## P2 — Two-column continuous panel (TUI) 🟡

Tier-4 UI feature: split sidebar — attached pane LEFT, main + continuous columns RIGHT (50/50); `A-C` toggles the continuous column in/out; tasks spawned by continuous tasks nest under their orchestrator in the continuous column (even though not themselves continuous); unified cursor with left/right nav. Start with a design/slice plan for review before coding.

## P3 — `continuous.update` + autocompact 🟢

No `continuous.update` method exists (only create/list/pause/run_now/delete; create is idempotent-reuse) → can't change a live task's config. Needed to:
- set `compact_every` (autocompact) on the orchestrator. It IS persistent (confirmed: one session across 8 fires), but `compact_every: None`, so its context grows unbounded across fires. **Minor** per user, but real long-term.
- update the orchestrator `default_prompt` (steer subtask agents to `set_subtask_status` + `ssh trader`, not `update_task`/gcloud).

Alternative without the new method: delete + recreate the task with the new config — loses run-log history + kills the session (the `.bug-triage/` memory survives, it's in the worktree).

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
