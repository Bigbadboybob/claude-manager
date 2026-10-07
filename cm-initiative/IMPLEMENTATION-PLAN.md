# Implementation plan: coordination support

Status: **approved by Owner, in progress**, 2026-10-06. Implements the Owner-approved
[coordination brief](shared/coordination-brief.md). Three detailed area plans,
each grounded in the current code (file:line citations), sit beside this file:

- [impl/items-board.md](impl/items-board.md) — items, boards, flags, pushes, TUI board.
- [impl/session-state.md](impl/session-state.md) — one engine-reported state per session.
- [impl/messaging-availability.md](impl/messaging-availability.md) — messaging
  fixes, Owner availability and gated `notify_user`, orchestration skill.

This file is the sequencing and the cross-area decisions. Workers read their
area plan for files, tests and deploy steps.

## 1. Architecture in one paragraph

**Items** live in the planning API's Postgres on cm-manager (new tables:
boards, items, holders, deps, events, flags, session_states, push outbox),
reached by agents only through their host daemon, which stamps who is calling.
Each daemon sends a 30 s heartbeat with its sessions' **engine-reported state**
(`working / working-background / waiting-on-human / errored / idle(since) /
starting / exited / unknown`); a flag engine in the API process computes
flags and writes pushes to an outbox, which each daemon picks up in the
heartbeat reply and delivers through the existing native notification queue.
**Owner availability** is a replicated, Owner-only messaging event every
daemon reads locally; `notify_user` gains an urgency and is held or delivered
against the level, with one shared `escalate()` used by the stall alarm and the
board. Session state comes from Claude's per-process status file plus extra
hooks, and from the Codex app-server relay, with the PTY heuristic kept only as
a fallback.

## 2. Cross-area decisions (resolving overlaps between the three plans)

1. **Keys.** Board overlay `Alt+B`; availability picker `F7` (F8 messages, F9
   settings already taken).
2. **Escalation.** The board calls `owner_attention::escalate()` (messaging
   plan B6). Until that slice lands it calls today's `raise_system`; the swap is
   one line.
3. **Holder state.** The board heartbeat sends `agent_state` once session-state
   S1 is merged; before that it uses the fallback mapping in items §4, and the
   board's `holder_idle` flag stays **off by default** until engine state is
   live on all three hosts (otherwise it inherits today's false idles).
4. **Availability bars** follow the brief: away → emergency only; around →
   blocking+; focused → decision+; on-call → everything (fyi+). An unset level
   delivers everything, so nothing changes until Owner first sets one.
5. **Stall alarm under `away`** becomes held unless it escalates to
   emergency; with no viewer connected, `blocking` and above also go to
   Telegram (`notify_command`). Called out in `OWNER_NOTIFICATIONS.md`.
6. **Shared hot files** and who owns them:
   - `daemon/src/control/methods.rs` — session-state core worker only;
     the items `report_done` addition (~15 lines) lands after it.
   - `daemon/src/control/dispatch.rs` — touched by items S2, session-state
     S1, availability (ping). Small, additive arms; merge in wave order.
   - `mcp_server/server.py` — additive tools/fields from several slices; each
     worker rebases on main before merging.
   - `mcp_server/AGENT_GUIDE.md` — the docs worker owns it; others send text.
   - `mcp_server/monitor.py` — messaging slice 1 (Codex empty last turn) lands
     first; session-state S6 builds on it.
   - **All TUI work goes to one worker**, serialized (board overlay, state
     glyphs/A-g, availability picker) because they share `draw.rs`,
     `events.rs`, `input.rs`.
7. **Contracts first.** Two short wire specs are written and merged before the
   parallel work: `doc/items-board.md` (API + MCP tool shapes) and
   `doc/SESSION_STATE.md` (`agent_state` object). Everything else codes against
   them.

## 3. Waves

Each slice is independently mergeable to `main`, tested with
`scripts/cm-test-isolated` (private `CARGO_TARGET_DIR`), and deployed per
CLAUDE.md: API by scp + `systemctl restart claude-manager`; daemon by brain-only
`daemon.restart` with the complete `mcp_server/` payload, hub first for
messaging changes; TUI through the `cm-tui-release` skill.

### Wave 0 — small, immediate, no contracts needed

| Slice | What | Area plan |
|---|---|---|
| W0a | Codex `read_last_turn` empty content (EP A8) | messaging §A4, slice 1 |
| W0b | Viewer keystrokes no longer reset idle / cancel `report_done` | session-state S0 |
| W0c | Contracts: `doc/items-board.md`, `doc/SESSION_STATE.md` | items §7 S1, session-state §1 |
| W0d | Orchestration policy + skill + guide norms (without board tools section) | messaging §C, slice 3 |

### Wave 1 — foundations

| Slice | What | Area plan |
|---|---|---|
| W1a | Items schema + API core (write rules, cycles, numbering, history) | items S1 |
| W1b | `agent_state` core: derivation, `session.agent_report`, publish, sidecar | session-state S1 |
| W1c | Messaging send side: body `@Name`, names in `mentions[]`, send time, outbox | messaging 2a |

### Wave 2 — the working system

| Slice | What | Area plan |
|---|---|---|
| W2a | Daemon proxy + MCP tools `item / item_set / board / item_resolve` | items S2 |
| W2b | Heartbeat + push delivery | items S3 |
| W2c | Flag engine, digests, escalation, 24 h close-out | items S4 |
| W2d | Claude status-file reader | session-state S2 |
| W2e | Codex relay as ordered publisher + 0.160 rollout parsers | session-state S4, S5 |
| W2f | Messaging read side: newest-first, slim default, `mark_read_before`, unread count fix, quiet norms | messaging 2b |

**Milestone A (end of wave 2):** EP's board is live for agents with
`holder_gone`, `stale`, `overdue`, `unassigned` and dependency flags pushing to
EP; engine state is live for Claude and new Codex sessions.

### Wave 3 — consumers, Owner surfaces

| Slice | What | Area plan |
|---|---|---|
| W3a | Claude extra hooks (UserPromptSubmit, StopFailure, PermissionRequest) | session-state S3 |
| W3b | MCP consumers of state (monitors, waits, list_sessions status words) | session-state S6 |
| W3c | Daemon consumers (continuous drain, sweep, workflow gate) | session-state S7 |
| W3d | `report_done` returns held items; exit pokes | items S5 |
| W3e | Availability state, CLI `cm-availability`, exposure in `ping`/`chat_open` | messaging slice 4 |
| W3f | TUI: state glyphs and A-g, then board overlay | session-state S8, items S6 |

**Milestone B:** `holder_idle` turned on; Owner sees the board in the TUI.

### Wave 4 — Owner availability end to end

| Slice | What | Area plan |
|---|---|---|
| W4a | Gated `notify_user` with urgency, held/release, level-change wakes, shared `escalate()` | messaging slice 5 |
| W4b | TUI F7 picker, status-bar level, release digest | messaging slice 6 |
| W4c | Orchestration skill: add board and availability sections; `#cm-general` release notice | messaging §C |

## 4. Staffing

Lanes follow the brief's guidance: Codex for hard, technical, low-volume
work; a Claude session with subagents for broader work.

| Lane | Slices | Engine |
|---|---|---|
| state-core | W0b, W1b, W3c | Codex |
| claude-state | W2d, W3a | Claude |
| codex-state | W2e | Codex |
| items-api | W0c (items spec), W1a, W2c | Claude |
| items-daemon | W2a, W2b, W3d | Codex |
| messaging | W0a, W1c, W2f, W3e, W4a | Claude |
| tui | W3f, W4b | Claude |
| docs | W0c (state spec, with state-core), W0d, W4c | Claude (small) |

Given the token budget, a lighter start is three lanes — **state-core**,
**items-api → items-daemon**, **messaging** — which reaches Milestone A with
Claude-only state; the rest follow as budget allows. Swarm-Coord coordinates:
assigns slices, reviews each before merge, runs deploys in order, and records
progress here. EP's #gpu-utilization swarm is the first real user after
Milestone A.

## 5. Owner decisions (2026-10-06)

1. **Light staffing**, no time pressure: three lanes — **state-core** (Codex),
   **items** (Claude; takes the items-daemon slices too), **messaging**
   (Claude; also W0d). Other slices are picked up by these lanes in wave order.
2. **Swarm-Coord merges** each reviewed slice to main. **Deploys are consolidated**
   (Owner, 2026-10-07): chat/messaging fixes were the exception; everything
   else now accumulates on main and ships in one deploy when the remaining
   work is done, hub first, brain-only restarts.
3. **Existing Codex lanes are restarted** at Milestone A; Swarm-Coord gives
   Owner the list and Owner does the restarts.

## 6. Main risks

- Claude's status file is undocumented and may change between releases:
  defensive parsing, unreadable means `unknown`, never idle; checked on every
  Claude update.
- Codex background-terminal methods are experimental: detected per version.
- The planning API becomes a dependency for items: if cm-manager is down, item
  calls fail fast and flags pause; holder state shows `unknown`.
- New status words (`needs_human`, `errored`, `unknown`) can break agent prompts
  that pattern-match `awaiting_input`; `idle` stays for compatibility and the
  change is announced.
- Body `@Name` promotion could wake people quoted in prose: restricted to
  conversation members and exact names, skips code spans, always reported back.

## 7. Progress

Lanes (subtasks of 3b58ab69, worktrees cut from origin/main `c09b239`):

| Lane | Session | Task | Branch worktree |
|---|---|---|---|
| state-core (Codex) | ts-18dc037d730aafd8-d | d103ad60 | cm-sub-swarm-focused-design-cm-state-core-4802fe0 |
| items (Claude) | ts-18dc037e5a39099d-e | a1830143 | cm-sub-swarm-focused-design-cm-items-7fbf738 |
| msgfix (Claude) | ts-18dc037f83e30601-f | 09c2c2d0 | cm-sub-swarm-focused-design-cm-messaging-fixes-b46b0aa |

Shared lane rules: `~/.local/share/swarm-coord/LANE-PREAMBLE.md`. Lanes hand off
each slice in `#initiative/swarm-focused-design/claude-manager`; Swarm-Coord
reviews, merges to main and deploys (hub first).

| Slice | Lane | Status | Merged | Deployed |
|---|---|---|---|---|
| W0a Codex empty last turn | msgfix | reviewed PASS | 8ff296e | cm-manager + cm-sessions 19:37Z |
| W0b viewer input vs idle | state-core | reviewed PASS | c2fdd91 | cm-manager + cm-sessions 19:37Z |
| W0c-items contract | items | reviewed PASS | 92a5a28 | doc only |
| W0c-state contract | state-core | reviewed PASS | bc76c50 | doc only |
| W0d orchestration docs | msgfix | reviewed PASS | f1009f9 | policies + skill on cm-manager + cm-sessions 19:38Z |
| W1a items API | items | reviewed PASS (+fixes 9f1aa85) | 9f1aa85 | API live on cm-manager 19:17Z; `GET /boards` 200 |
| W1b agent_state core | state-core | reviewed PASS (+fixes) | 8223700 | cm-manager + cm-sessions 23:20Z (c61f920) |
| W2a items daemon proxy + MCP tools | items | reviewed PASS (follow-ups in W2b) | 5e24d12 | cm-manager + cm-sessions 23:20Z |
| W2b heartbeat + push delivery | items | reviewed PASS (+fixes) | 8411958, baee959 | API+018 cm-manager 23:40Z; brains cm-manager 23:44Z, cm-sessions 23:52Z; heartbeats 200 |
| W2c flag engine | items | reviewed PASS (+fixes) | 5fb76eb, bf1bf7c | pending big deploy |
| Prompt confirmation (start_session) | state-core | reviewed PASS (2 review rounds) | 376ff8c, dcf6399, 3025127 (merge 1dc3a2e) | pending big deploy |
| W3d report_done held items + pokes | items | in progress | | |
| W3f TUI state glyphs + board overlay; W4c docs | msgfix | in progress | | |
| W3a/W3b/W3c hooks + consumers | state-core | queued | | |
| W2d Claude status-file reader | state-core | reviewed PASS (+fixes, rollback switch presence_idle_enabled) | 327720b | cm-manager + cm-sessions 23:20Z; Claude sessions on source=presence |
| W2e Codex relay state + 0.160 parsers | state-core | reviewed PASS (+6 fixes) | cef633b, c61f920 | cm-manager + cm-sessions 23:20Z; existing Codex sessions need A-R |
| W1c messaging send side | msgfix | reviewed PASS | a11adcc | cm-manager + cm-sessions 19:37Z (Owner priority); verified body @Name wakes |
| W2f messaging read side + slim responses | msgfix | reviewed PASS | 084fff2, f9a13ee | cm-manager + cm-sessions 20:03Z (Owner priority) |
| W3e Owner availability state + CLI | msgfix | reviewed PASS | 43e30ef | cm-manager + cm-sessions 20:52Z; ~/.cm/bin/cm-availability on both |
| W4a gated notify_user | msgfix | reviewed PASS (+fixes) | fb7f9a3, e8eaede | cm-manager + cm-sessions 20:52Z |
| W4b TUI F7 picker + status level + release digest | msgfix | reviewed PASS | 9ef0e15 | release staged ~/.cm/releases/tui-9ef0e15; awaiting Owner laptop install |

Note: both Claude lanes' launch prompts were silently dropped at start_session (status file idle since spawn, no transcript, monitor timed out ~35 min later); redelivered with send_input 18:47Z. Follow-up: start_session should confirm the turn started (state-core, after W2d).

Wave-0 brain deploy: cm-manager (hub) done 19:06Z on f1009f9 (epoch 19, 39 sessions).
**Incident:** the hub's new brain took ~6 min loading the message store (~56k files)
before answering RPCs, so cm-sessions lost hub sync 19:06–19:12Z (messages queued
locally and flushed; none lost). cm-sessions deploy held until after EP's 23:00Z freeze,
with advance notice in #cm-general. Follow-up: make hub brain start non-blocking for
messaging (lazy/background store load).

19:35–19:37Z: Owner-prioritized deploy of a11adcc (wave 0 + W1c) to cm-manager, then
cm-sessions, with advance notice. Hub came back in ~20 s this time (store warm in page
cache); cm-sessions in ~90 s. Laptop daemon still on the old build (next TUI release).

Coordinator miss (21:31Z): the items lane sat idle ~2 h after its W1a fixes were merged
and deployed, because I never told it to start W2a. Exactly the failure the board's
`holder_idle` flag targets. Restarted at 21:31Z.

23:20Z: c61f920 (W1b+W2a+W2d+W2e) live on both cloud hosts. cm-sessions sessions_by_state
right after: errored 1, idle 26, starting 8, working 13. The new presence state immediately
showed the items lane idle since 22:00Z: it had paused on memory pressure and asked its
question only in its own session. Norm added: lanes post blockers in the channel.

Deploy-path bug found 23:44Z (pre-existing, holder): on cm-sessions `daemon.restart` armed the
new pinned brain fd, then the holder saw the outgoing brain's socket reset as a crash
("brain declared dead ... respawning current pin") and exec'd the OLD binary. Epoch bumped,
build_id unchanged. A second daemon.restart worked. Always verify build_id / /proc exe sha
after a deploy. Needs a holder fix (follow-up task).

**Pending big deploy** (merged to main, not deployed): W2c flag engine (API only), then
everything after. API: api/board_engine.py + api/main.py. Brains + MCP: per later slices.
TUI: rebuild release at the end (tui-9ef0e15 superseded unless Owner installs it first).
