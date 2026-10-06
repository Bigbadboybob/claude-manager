# Implementation plan: work items and the board

## 0. Design decisions, with the evidence for each
- **Store: the planning API in Postgres on cm-manager.** It is already the one shared copy every daemon reaches. Each daemon talks to it through `PlanningApiCreds` (`daemon/src/control/methods.rs:14952`), and the TUI already uses the API from the laptop (`tui/src/api.rs`).
  - I rejected the messaging sync store. It has "no planning-API dependency" and only syncs chat events (`doc/messaging/CROSS_MACHINE.md` §Enrollment), so it would mean a new replicated object type.
- **Every agent tool goes through the daemon** (the `backtest.submit` pattern: `mcp_server/control_client.py:299-307`, `daemon/src/control/dispatch.rs:784`).
  - The API has one shared bearer token (`api/auth.py`), so only the daemon knows who the caller is. It stamps the actor before forwarding.
  - There is no `PlanningClient` fallback. Tools routed through the CLI fail on headless hosts, and the daemon route works on every host.
- **Holder identity is the messaging participant id, `agent:<daemon_id>:<uid>`** (`daemon/src/messaging/store.rs:916`). `daemon_id` is the stable UUID stored in `~/.cm/daemon-id` (store.rs:305).
  - It is the same across hosts and survives renames. `chat_people` already lists remote participants on every replica, so a daemon can resolve names locally.
- **The flag engine runs in the API process** as one more lifespan task, like `dispatch_loop` and `change_log_maintenance_loop` (`api/main.py:82-100`). The API is the only process that sees every host's holders.
- **Pushes are delivered by the target session's own daemon** through `crate::notifications::publish(root, uid, id, source, text, marker)` (`daemon/src/notifications.rs:51`).
  - This is the native queue chat wakes and monitor fires already use. It works for Claude and Codex, `source` can be any string, and an id makes a repeat publish do nothing.
  - The API writes rows to an outbox table, and each daemon picks up its own rows in the reply to its state heartbeat. No new socket, long-poll or chat bot is needed.
- **Owner escalation** goes through `owner_attention::raise_system(state, uid, msg)` (`daemon/src/owner_attention.rs:148`) on the orchestrator's daemon. Availability gating belongs to the other planner; this plan only calls one function.
- **Board scope:** the initiative's board if the caller's task has one (`tasks.initiative_id`, `sql/015_initiatives.sql`), otherwise a board for its top-level task, found by walking `parent_task_id`.
- **The orchestrator** is `boards.orchestrator_pid` if set explicitly. Otherwise it is the newest live session (from heartbeats) bound to the initiative's `coordinator_task_id`, or to the root task for a task board. If none is found, the board shows `orchestrator: none` and escalates straight to Owner.

## 1. Schema: `sql/017_items.sql`
It is safe to re-run: only `CREATE ... IF NOT EXISTS`, `ADD COLUMN IF NOT EXISTS` and guarded `DO $$` constraints. There are no row UPDATEs and no backfill, so `migrations_applied` isn't needed (CLAUDE.md §Database; `sql/008`).
```
boards(id uuid pk, initiative_id uuid unique null fk→initiatives on delete set null,
  root_task_id uuid unique null fk→tasks on delete set null, slug text unique, name text,
  orchestrator_pid text null, idle_s int default 1200, stale_s int default 7200,
  unassigned_s int default 300, repush_s int default 1800, escalate_s int default 3600,
  digest_s int default 300, next_number int default 1, last_digest_at timestamptz,
  created_at, updated_at, CHECK (initiative_id is not null or root_task_id is not null))
items(id bigserial pk, board_id fk, number int, title text, status text CHECK in
  (open,active,waiting,blocked,done,dropped), note text, grp text, blocked_on text,
  check_back_at timestamptz, eta_at timestamptz, waiting_set_at timestamptz,
  links jsonb default '[]', touched_at, clock_reset_at, closed_at, archived_at,
  created_by_pid text, created_at, UNIQUE(board_id, number))
item_holders(item_id fk cascade, pid text, session_uid text, daemon_id text,
  name text, added_at, PRIMARY KEY(item_id,pid))
item_deps(item_id fk cascade, blocker_id fk, created_at, PRIMARY KEY(item_id,blocker_id))
item_events(id bigserial pk, board_id, item_id, actor_pid text, actor_name text,
  type text, prev jsonb, new jsonb, reason text, created_at)    -- history + board version
item_flags(id bigserial pk, item_id, kind text, detail jsonb, raised_at, resolved_at,
  resolved_by text, resolution text, snooze_until, last_pushed_at, push_count int,
  escalated_at)  + partial unique index (item_id,kind) WHERE resolved_at IS NULL
session_states(pid text pk, daemon_id, session_uid, host_label, task_id uuid, name,
  engine, state text, state_age_s int, idle_since timestamptz, reported_done_at,
  exited_at, killed_by, reported_at timestamptz)   -- server clock only
item_pushes(id bigserial pk, daemon_id, session_uid, pid, kind text, text text,
  dedupe text unique, created_at, delivered_at, owner_alert bool default false)
```
- Indexes: open items per board `(board_id) WHERE archived_at IS NULL`; `item_holders(pid)`; `item_pushes(daemon_id) WHERE delivered_at IS NULL`.
- None of these tables gets the `task_changes` triggers from `sql/016`, so the task feed is unaffected.

## 2. API: `api/items.py` (FastAPI `APIRouter`), `dispatch/items_db.py`, models in `api/items_models.py`
Every write takes `actor:{pid,name,session_uid,daemon_id,task_id}` (Owner is `pid="owner"`).

| Endpoint | Purpose |
|---|---|
| `POST /boards/resolve {task_id \| ref}` | Upsert the board (recursive CTE up `parent_task_id`, then `initiative_id`) |
| `GET /boards/{ref}?view=slim\|full&since_version=&archived=&q=` | Board read; `version = max(item_events.id)`; returns `{unchanged:true}` when nothing changed |
| `PATCH /boards/{ref}` | Thresholds, orchestrator |
| `POST /boards/{ref}/items` | Takes a list; per-board `number` from `UPDATE boards SET next_number=next_number+n RETURNING` |
| `PATCH /boards/{ref}/items/{n}` | Field edits |
| `POST /boards/{ref}/items/{n}/resolve` | Flag-resolution actions |
| `GET /items?holder_pid=&open=true` | Used by `report_done` |
| `POST /hosts/{daemon_id}/heartbeat {snapshot:[…], exited:[…], acked_push_ids:[…]}` | Returns `{pushes:[…]}` |

Write rules, each in one transaction with `SELECT … FROM boards WHERE id=$1 FOR UPDATE`, which serializes edits per board:
- Any change to status, note or holders sets `touched_at=now()` and writes an `item_events` row with prev/new values.
- **`blocked_by`:** every blocker must exist on the same board and not be done or dropped, or the call returns 422. A cycle check (recursive CTE over `item_deps` from the new blockers back to the item) returns 409 `cycle: 14→15→14`.
- **Done:**
  - Delete the item's `item_deps` rows where it is the blocker.
  - Any dependent left with no blockers and `status='blocked'` with no `blocked_on` goes back to `active`, with `clock_reset_at=now()` and a `unblocked` push to its holders.
  - Set `closed_at`, and resolve the item's own open flags with resolution `closed`.
- **Dropped:** dependents get a `blocker_dropped` flag and the edge is kept until someone acts. Set `closed_at`.
- **Holder added by someone other than that holder:** queue an `assigned` push.
- **Reopen:** setting status to anything other than done/dropped on a closed item clears `closed_at` and `archived_at`.
- **ETA:** `waiting` requires `eta` (relative such as "40m", or ISO); store `eta_at` and `waiting_set_at`.

## 3. Flag engine: `api/board_engine.py`
Started from lifespan as `board_loop(pool, interval=30)`. The core is a pure function `compute_flags(item, holders, states, deps, board, now) -> set[kind]` so it can be unit-tested.

**State staleness:** if `session_states.reported_at < now-90s`, the state counts as `unknown`. Unknown never counts as idle and never as gone.

**Flags:**
- **unassigned:** `open`, no holders, older than `unassigned_s`.
- **holder_gone:** any holder's `exited_at` is set, or the holder's pid has no row, on an item that is active, waiting or blocked. This applies even when the item is blocked by another item.
- **holder_idle:** `active`, and every holder is `idle` with `now - max(idle_since, clock_reset_at, touched_at) >= idle_s`. Skipped while the item is waiting before its overdue point, or blocked only by open items.
- **holder_waiting_on_human / holder_errored:** passed through from §5a states (counted for any holder).
- **stale:** active or blocked, and `now - max(touched_at, clock_reset_at) >= stale_s`. Waiting items use the larger of `stale_s` and `eta_at - waiting_set_at`. Exempt while blocked only by open items. Free-text `blocked_on` gets no exemption.
- **overdue:** `waiting` and `now > eta_at + 0.25*(eta_at - waiting_set_at)`. The first push goes to the holder; if still unresolved after `repush_s`, the orchestrator.
- **check_back:** `blocked_on` is set and `check_back_at` has passed.
- **blocker_dropped:** raised on the write path (§2).

**Flag lifecycle:**
- A flag is inserted when it first holds and auto-resolves (`resolution='cleared'`) when its condition stops holding.
- A `snooze_until` set by `nudge` stops it re-firing until another full threshold has passed.

**Pushes**, one outbox row per orchestrator per tick at most:
- New flags are pushed immediately.
- Unresolved flags are re-pushed after `repush_s`.
- Done/dropped/blocked events from `item_events` since `last_digest_at` are batched, sent at most once per `digest_s`.
- Text format: `[cm-board <slug>] 2 flags: #14 holder_idle (rl-scale-out idle 24m); … · 3 done (#9,#11,#12). board() / item_resolve(n, action)`.
- Kept under about 1 KB. `dedupe = board:tick:<hash>`.

**Escalation:** if the orchestrator's state is idle, unknown or gone and the oldest unresolved flag is older than `escalate_s`, write an `owner_alert=true` row addressed to the orchestrator's daemon and uid, or to the coordinator's last daemon if there is no orchestrator. Set `escalated_at`, once per flag.

**Close-out:** set `archived_at=now()` where `closed_at < now-24h` (an ordinary runtime UPDATE, not a migration). Prune delivered pushes older than 7 days, and `session_states` rows that exited more than 7 days ago.

## 4. Daemon: new module `daemon/src/items/` (keeps edits to the 16k-line `methods.rs` minimal)
- **`items/api.rs`:** ureq calls, the same shape as `api_get_initiative` (methods.rs:15140).
- **`items/rpc.rs`:** methods `item.create`, `item.set`, `item.resolve`, `board.read`.
  - Session callers: take uid and task_id from the registry, build the actor pid with `Store::participant_id`, and resolve holder names through the messaging store (`names`/`people`). Accept a uid, a name or `none`.
  - Auth: editing is allowed if the caller's task resolves to the board, or the caller holds any item on it, or it has `global_perms`. Reading is allowed for any session. The Operator (TUI) acts as `owner`.
  - `item.resolve action=launch` calls the existing spawn path (`mcp_start_session` internals) under the caller's permissions, with prompt `item #n: <title>`, then sets the new uid as holder.
- **`items/heartbeat.rs`:** a thread spawned in `lib.rs` next to `messaging::tasks::spawn` (lib.rs:1214). Every 30 s, plus `items::heartbeat::poke()` (debounced 2 s) on session exit, `report_done`, or a state change:
  - It sends a full snapshot of live sessions: `{uid, pid, task_id, name, engine, state, state_age_s, idle_for_s, reported_done, exited, killed_by}`. Durations are relative, so the server's clock decides and clock skew between hosts doesn't matter.
  - Pids missing from a snapshot and not tombstoned are marked `exited` server-side.
  - The `state` field comes from the other planner's published engine state. Until that lands, it falls back to today's status: `running`→working, `awaiting_input`/`semantic_idle`→idle, `reported`→idle with `reported_done`, and `exited`.
  - For each push returned: rows with `owner_alert` go to `owner_attention::raise_system`; everything else goes to `notifications::publish(cm_root, uid, "board:<id>", "board", text, "[cm-board <id>]")`. Delivered ids are acked on the next beat.
- **`report_done`** (methods.rs:12044): after the existing marking, a best-effort call (2 s timeout) to `GET /items?holder_pid&open=true` adds `held_items:[{n,board,title,status}]` and the hint "close (item_set n done) or hand back (item_set n holder=none)" to the response. An API error is swallowed into `held_items_error`.

## 5. MCP tools (`mcp_server/server.py`; add the method names to the daemon-only set in `control_client.py`; daemon-only, no TUI handler)
```
item(title: str|list[str], holder: str|list[str]|None=None, status="active", note=None,
     group=None, blocked_by: list[int]|None=None, blocked_on=None, eta=None,
     links: list[str]|None=None, board=None) -> {board, items:[{n,title,status,holders}]}
item_set(n: int|list[int], status=None, note=None, holder=None, add_holder=None,
     remove_holder=None, group=None, blocked_by=None, blocked_on=None, check_back=None,
     eta=None, title=None, links=None, reason=None, board=None) -> {items, unblocked?, warnings?}
board(board=None, view="slim", mine=False, group=None, include_closed=True,
     archived=False, query=None) -> {board:{slug,orchestrator,health:{unresolved,oldest_s}},
     flags:[...], items:[...], recently_closed:[...], free_capacity:[names]}
item_resolve(n: int, action: "nudge"|"reassign"|"launch"|"block"|"drop", holder=None,
     blocked_by=None, blocked_on=None, check_back=None, reason=None, engine="codex",
     message=None) -> {item, flag_resolved}
```
- **`board()` slim view:** one string per item, flags first, then by group, e.g. `#14 active "fuse SEJD" @rl-scale-out[idle 24m] ⚑holder_idle · waiting on JP`. `view="full"` returns dicts plus each item's last 5 events.
- **Status shorthand:** a bare string in the second position becomes `status`. A status without a holder defaults to the caller; `holder="none"` means `open`.
- **Restart barrier:** add `board.read` to `RESTART_BARRIER_READ_ONLY_METHODS` (`dispatch.rs:304`).
- **Docs:** add the norm "if you're doing it, it has an item; long job ⇒ waiting+eta" to `mcp_server/AGENT_GUIDE.md`.

## 6. TUI board overlay: `tui/src/app/board.rs` (new)
- Modeled on the Messages overlay (`tui/src/app/messages.rs`: `visible`, off-thread `rx` worker, local-daemon RPC through `host_pool.live_socket_path(HostId::local())` at line 594).
- **Toggle:** `Alt+B` / `F7`. `Alt+b` is taken by snapshots (`input.rs:3210`); `Alt+B` appears unused.
- **First version:**
  - Board picker: initiatives plus task boards that have open items.
  - Header: name, orchestrator and its state, health (flag count and oldest flag age).
  - Flags section, then items grouped by `group`, then a "recently closed (24 h)" strip.
  - Polls `board.read` with `since_version` every 5 s, and only while the overlay is visible, which follows the CLAUDE.md rule against periodic full polls.
- **Keys:** `j/k`, `Enter` (detail plus history), `a` add, `s` status, `n` note, `r` reassign (name prompt), `x` resolve flag (nudge/reassign/block/drop menu), `d` drop (asks for a reason), `o` reopen, `g` refresh.
- **Integration edits:** one key arm in `input.rs` beside `messaging_event`, one render call in `draw.rs`, one field in `model.rs`.
- **Later:** a flag-count badge on the initiative subsection header and the status bar.

## 7. Phased slices
| # | Slice | Files (owned) | Tests | Deploy |
|---|---|---|---|---|
| S1 | Schema, API core, write rules, cycle check, numbering, history; also writes the contract doc | `sql/017_items.sql`, `api/items.py`, `api/items_models.py`, `dispatch/items_db.py`, `api/main.py` (only `include_router`), `doc/items-board.md` | `mcp_server/tests/test_items_api.py` (mocked pool, same style as `test_backtest_auto_archive.py`): cycle refusal, blocker validation, unblock cascade, drop flag, numbering; migration re-run text check as in `test_initiative_tools.py:53` | scp `api/`, `dispatch/`, `sql/` to `/opt/claude-manager`, then `systemctl restart claude-manager` |
| S2 | Daemon proxy and MCP tools (needs S1 API) | `daemon/src/items/{mod,api,rpc}.rs`, `dispatch.rs` (4 arms and barrier list), `lib.rs` (`mod items`), `control_client.py` sets, `server.py` tools, `AGENT_GUIDE.md` | Rust: stub API (`planning_client::spawn_stub_api_for_test`) for name resolution, auth, actor stamping. Py: `test_item_tools.py` plus `DaemonMethodsAlignmentTests` | Local/cm-sessions/cm-manager: build the brain, stage the complete `mcp_server/`, `scripts/cm-op --ssh <host> daemon.restart` (HOWTO_HOLDER_BRAIN_SPLIT §3); agents reconnect MCP |
| S3 | Heartbeat and push delivery | `daemon/src/items/heartbeat.rs`, `lib.rs` spawn line; API heartbeat endpoint in `api/items.py` (coordinate with S1 owner, or split into `api/items_hosts.py`) | Rust: snapshot shape, acked ids, `notifications::publish` idempotent per push id, `raise_system` path; Py: missing-from-snapshot → exited | API restart, then brain restart on all 3 hosts |
| S4 | Flag engine, digest, escalation, archive | `api/board_engine.py`, `api/main.py` lifespan (2 lines) | `test_board_engine.py`: table-driven `compute_flags` (idle across all holders, waiting grace, chain end flagged, unknown ≠ idle, snooze, overdue, check_back); digest rate limit; archive after 24 h | API only |
| S5 | `report_done` held items and exit pokes | `methods.rs` `report_done` (about 15 lines), session exit/reap hook calls `items::heartbeat::poke` | Rust unit test with stub API; failure is non-fatal | Brain restart on 3 hosts |
| S6 | TUI board overlay | `tui/src/app/board.rs`, small edits to `input.rs`, `draw.rs`, `model.rs` | ratatui buffer render tests (flags first, grouping, closed strip); stubbed RPC | `cm-tui-release` skill (doc/TUI_RELEASES.md); no daemon restart |
| S7 | Orchestration skill and `#cm-general` notice | skill dir, `doc/AGENT_QUICKSTART.md` | n/a | n/a |

- **Order:** S1 first, since it fixes the contract. Then S2, S3, S4 and S6 can run in parallel against `doc/items-board.md`. S5 comes after S2 and S3.
- **Conflict boundaries:**
  - Only S2 touches `dispatch.rs`.
  - Only S5 touches `methods.rs`.
  - `lib.rs` gets one line each from S2 and S3: merge S2 first, or give S3 the spawn line inside `items/mod.rs::start()`.
  - `api/main.py` gets one line each from S1 and S4.
- **Local rehearsal:** run Rust tests through `scripts/cm-test-isolated` with a private `CARGO_TARGET_DIR`.

## 8. Open technical risks
1. **Actor spoofing:** the API trusts the daemon-stamped actor because there is one shared token. This is the same trust model as `metadata.filer`. Per-daemon tokens would be follow-up work.
2. **The API is a single point of failure.** If cm-manager is down, item calls fail fast with a clear error and there is no local queue. Flags pause and holder states go stale (shown as unknown), which is safe.
3. **Flag quality depends on the §5a engine state.** Before it lands, the fallback mapping inherits today's false-idle and false-busy behavior. Consider shipping S4 with `holder_idle` disabled by board setting until §5a merges.
4. **`daemon_id` unavailable:** a daemon whose messaging store is degraded has no `daemon-id`. Items must refuse with a clear error rather than mint an id.
5. **Undelivered pushes:** old embedded Codex sessions without a native consumer leave pushes pending (NATIVE_NOTIFICATIONS.md). The board should show `push undelivered` from the heartbeat acks.
6. **Orchestrator resolution is ambiguous** when several live sessions are bound to the coordinator task. The explicit `orchestrator_pid` override resolves it; `board()` shows which session was chosen.
7. **Per-board locking:** `FOR UPDATE` on the board row serializes writes. That is fine at swarm scale (about 20 lanes); a re-check would be needed if boards reach hundreds of writers.

### Summary (≤300 words)
- **Storage:** items go in the planning API's Postgres through migration `sql/017_items.sql`: boards, items, holders, dependencies, events, flags, session states, and a push outbox. All statements are safe to re-run with no row UPDATEs. A board is the caller's initiative, otherwise its top-level task.
- **Identity:** holders are messaging participant ids `agent:<daemon_id>:<uid>` (`store.rs:916`), stable across hosts and resolvable by name through the messaging store on every daemon.
- **Agent tools:** `item`, `item_set`, `board` and `item_resolve` go only through the daemon (the `backtest.submit` proxy pattern). The daemon stamps the actor, because the API has one shared token and no caller identity.
- **Session state:** each daemon sends a 30-second heartbeat, plus a poke on exit or state change, with a full snapshot of its sessions. Durations are relative so host clock skew doesn't matter. States older than 90 seconds count as `unknown`, never idle.
- **Flags:** an API-process loop (`api/board_engine.py`, run like `dispatch_loop`) computes them with a pure, unit-testable function. Cycle refusal, auto-unblock and blocker-dropped flags happen in the write transaction.
- **Pushes:** written to an outbox and returned in each daemon's heartbeat reply. The daemon delivers them through the existing native queue (`notifications::publish`) or escalates through `owner_attention::raise_system`, so no new transport is needed. Orchestrator pushes are capped at one per tick, with done/blocked batched every 5 minutes.
- **`report_done`** returns the caller's held items. The 24-hour recently-closed strip and auto-archive come from the engine loop.
- **TUI:** an `Alt+B`/F7 overlay modeled on Messages that reads through the local daemon, polling only while visible.
- **Slices:** S1 (API and schema) fixes the contract first. S2 (daemon and MCP), S3 (heartbeat), S4 (engine) and S6 (TUI) can then go to parallel workers with separate files; S5 (`report_done`) follows S2 and S3, and S7 (skill and docs) is independent.
- **Main risks:** trusting the daemon-stamped actor, the API as a single point of failure, and flag quality depending on the other planner's engine state.

### Critical Files for Implementation
- /home/lucas/.cm/worktrees/claude-manager-swarm-focused-design-cm/sql/015_initiatives.sql (the pattern for the new `sql/017_items.sql`)
- /home/lucas/.cm/worktrees/claude-manager-swarm-focused-design-cm/api/main.py (lifespan loops, router mount)
- /home/lucas/.cm/worktrees/claude-manager-swarm-focused-design-cm/daemon/src/control/dispatch.rs (method arms, read-only barrier list)
- /home/lucas/.cm/worktrees/claude-manager-swarm-focused-design-cm/daemon/src/notifications.rs (push delivery)
- /home/lucas/.cm/worktrees/claude-manager-swarm-focused-design-cm/mcp_server/control_client.py and /home/lucas/.cm/worktrees/claude-manager-swarm-focused-design-cm/mcp_server/server.py (tool routing and definitions)
