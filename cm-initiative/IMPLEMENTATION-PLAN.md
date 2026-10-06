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
2. **Swarm-Coord merges and deploys** each reviewed slice itself, hub first,
   brain-only restarts.
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
