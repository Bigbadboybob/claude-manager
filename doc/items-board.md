# Work items and boards: wire contract

Contract for the items rollout (Swarm Focused Design, items S1–S6). It fixes
the planning-API endpoints, daemon RPCs, MCP tool shapes and flag semantics so
the API, daemon, engine and TUI slices can be built in parallel. Product
rationale: the initiative's coordination brief §3 and §5; holder state comes
from [SESSION_STATE.md](SESSION_STATE.md).

Consumers ignore unknown object keys. An unknown item status, flag kind or
holder state is shown verbatim and never treated as idle or closed.

## 1. Model

- **Board.** One per initiative (`tasks.initiative_id`), otherwise one per
  top-level task (walk `parent_task_id` to the root). Created on first use.
  Addressed by `ref` = its `slug` or its UUID. Initiative boards take the
  initiative slug; task boards take `task-<first 8 of root task id>`.
- **Item.** Addressed by `(board, n)`; `n` is a per-board number starting at 1,
  never reused. Items are not planning tasks and never appear in the backlog.
- **Participant id (`pid`).** Holders and actors are messaging participant ids,
  `agent:<daemon_id>:<session_uid>`, stable across hosts and renames. Owner is
  `owner`. A daemon with no `daemon_id` (degraded messaging store) refuses item
  calls with `daemon_id_unavailable`; it never mints one.
- **Actor.** Every write carries `actor = {pid, name, session_uid?, daemon_id?,
  task_id?}`, stamped by the daemon (agents cannot supply it). The API trusts it
  under the single shared bearer token, as with `metadata.filer`.

| Field | Type | Notes |
|---|---|---|
| `n` | int | per-board number |
| `title` | str ≤ 200 | required |
| `status` | `open` `active` `waiting` `blocked` `done` `dropped` | default `active` |
| `holders` | `[{pid, name, session_uid, daemon_id}]` | default: the caller |
| `note` | str ≤ 500 | one line, latest wins |
| `group` | str ≤ 80 | free-text heading |
| `blocked_by` | `[n]` | open items on the same board |
| `blocked_on` | str ≤ 200 | free text; no flag exemption |
| `check_back_at` | ts | re-flag time for `blocked_on` |
| `eta_at`, `waiting_set_at` | ts | set with `waiting` |
| `links` | `[str]` ≤ 20 | task id, branch/SHA, message id, path, URL |
| `touched_at` | ts | last status/note/holder change |
| `clock_reset_at` | ts | set on auto-unblock |
| `closed_at`, `archived_at` | ts | |
| `created_by` | pid | |

Timestamps on the wire are RFC 3339 UTC strings from the **API server clock**.
Durations sent by daemons are relative seconds, so host clock skew never matters.

## 2. Write rules

All writes for one board run in one transaction holding the board row
(`SELECT … FOR UPDATE`). Every change appends an `item_events` row
`{id, item n, actor, type, prev, new, reason, at}`; that table is the history
and `board.version = max(item_events.id)` for the board. Flag raise/resolve
also writes an event, so the version covers flags. Holder states are not
events: a `since_version` reader that shows live holder state should still
re-read fully every ~30 s.

1. **Defaults and shorthands.** No holder given → the caller. `holder="none"`
   (`holders: []` or `null`) on an `active` item sets `open` unless a status is
   given; a `blocked` or `waiting` item keeps its status, blockers and ETA,
   with a warning (stale/overdue still reach the orchestrator).
   Adding a holder to an `open` item sets `active` unless a status is given.
   Without an explicit status, a non-empty `blocked_by` or `blocked_on` sets
   `blocked`, an `eta` sets `waiting`, and clearing the last blocker of a
   `blocked` item with no `blocked_on` sets `active` (`open` if unheld).
   Only a `blocked` item carries `blocked_by` / `blocked_on` / `check_back`:
   leaving `blocked` clears them, and passing them with another explicit
   status is `422 invalid_field`. `blocked` with neither returns a warning.
2. **Touch.** Any change to status, note or holders sets `touched_at`.
3. **`blocked_by`.** Every blocker must exist on the same board and be neither
   `done` nor `dropped` → else `422 invalid_blocker`. An item cannot block
   itself. A new edge that closes a cycle → `409 cycle` with the path, e.g.
   `cycle: 14→15→14`. Passing `blocked_by=[]` removes all edges.
4. **`waiting`** requires `eta` → else `422 eta_required`. `eta` is a duration
   (`40m`, `2h`, `1h30m`) or an RFC 3339 time; stores `eta_at` and
   `waiting_set_at=now`; leaving `waiting` clears both, and `eta` with another
   explicit status is refused. `check_back` takes the same forms.
5. **Done.** Remove edges where the item is a blocker. Each dependent left with
   no blockers, status `blocked` and no `blocked_on` returns to `active` with
   `clock_reset_at=now` and an `unblocked` push to its holders. Set
   `closed_at`; resolve the item's open flags with resolution `closed`. A
   dependent still waiting on another blocker or on `blocked_on` stays
   `blocked`.
6. **Dropped.** Set `closed_at`; resolve its open flags (`closed`). Each
   dependent gets a `blocker_dropped` flag; the edge stays until someone acts.
   A `reason` is recommended and stored on the event.
7. **Reopen.** Any non-closed status on a closed item clears `closed_at` and
   `archived_at`. Blockers named in a reopen are re-validated by rule 3.
   Reopening a dropped blocker clears its dependents' `blocker_dropped`, as
   does re-pointing a dependent's `blocked_by` away from dropped items.
8. **Assigned.** A holder added by an actor other than that holder queues an
   `assigned` push to the new holder.
9. **Editing rights** (enforced by the daemon, §5): any session whose task
   resolves to the board, any current holder on it, a `global_perms` session,
   and Owner. Reading is open to every session.

## 3. Holder state

Each daemon heartbeats its live sessions (§4 `heartbeat`). The API stores one
`session_states` row per pid. A row whose last heartbeat is older than 90 s
reads as `unknown`. Holder states are the `agent_state.state` vocabulary of
[SESSION_STATE.md](SESSION_STATE.md); until a host publishes `agent_state` the
daemon maps today's status: `running`→`working`, `awaiting_input` /
`semantic_idle`→`idle`, `reported`→`idle` + `reported_done`, gone→`exited`.
A pid missing from its daemon's full snapshot is marked `exited` server-side.

Only `state=idle` counts toward idle; `unknown` is never idle and never gone.

## 4. Flags

Computed every 30 s by the engine in the API process (`api/board_engine.py`,
a pure `compute_flags(item, states, items, board, now, live_daemons)`), for
each board with unarchived items. Each pass takes the board-row lock and
writes through the same rules as the write path (actor
`system:board-engine`), so raises and clears are history events and move the
board version. Flag details are stable (names, numbers, times; never
durations), so an unchanged condition writes nothing. `blocker_dropped` is
also raised by the write path. Thresholds are per board:

| Board setting | Default |
|---|---|
| `idle_s` | 1200 (20 min) |
| `stale_s` | 7200 (2 h) |
| `unassigned_s` | 300 |
| `repush_s` | 1800 |
| `escalate_s` | 3600 |
| `digest_s` | 300 |
| `holder_idle_enabled` | **false** until engine state is live on all hosts |
| `orchestrator_pid` | null = auto (below) |

| Kind | Raised when |
|---|---|
| `unassigned` | `open`, no holders, older than `unassigned_s` |
| `holder_gone` | item held (below) and any holder exited ≥ 90 s ago, or has had no state row for ≥ 90 s since it was added while its daemon is heartbeating (a daemon not yet sending heartbeats leaves its holders `unknown`); applies even while blocked |
| `holder_idle` | `active` (or `open` with holders), every holder `idle` with `now − max(idle_since, clock_reset_at, touched_at) ≥ idle_s` |
| `holder_waiting_on_human` / `holder_errored` | any holder in `waiting-on-human` / `errored` |
| `stale` | held, `now − max(touched_at, clock_reset_at) ≥ stale_s`; for `waiting` the threshold is `max(stale_s, eta_at − waiting_set_at)` |
| `overdue` | `waiting` and `now > eta_at + 0.25·(eta_at − waiting_set_at)` |
| `check_back` | `blocked_on` set and `check_back_at` passed |
| `blocker_dropped` | a blocker of this item was dropped |

"Held" means `active`, `waiting`, `blocked`, or `open` with holders: an
`open` item that keeps holders gets the same holder and staleness flags as
`active`, so nothing blocked behind it can hide.

**Exemptions.** An item blocked only by open items (`blocked_by` non-empty,
`blocked_on` empty) is exempt from `holder_idle` and `stale`. Free-text
`blocked_on` earns no exemption. `waiting` before its overdue point is exempt
from `holder_idle`. Nothing exempts `holder_gone`. Since `blocked_by` cannot
form a cycle, every waiting chain ends at an unexempt item.

**Lifecycle.** At most one open flag per `(item, kind)`; re-raising an open
flag with new detail updates the detail (event `flag_updated`). A flag auto-resolves
with resolution `cleared` when its condition stops holding, `closed` when the
item closes, or the action name when resolved by `item_resolve`. `nudge` sets
`snooze_until = now + <that kind's threshold>`; the flag is resolved and may
re-raise only after the snooze.

**Free capacity** (not a flag): sessions on the board (task resolves to it or
holding any item there) that are live and hold no `active`/`waiting` item.

**Orchestrator.** `orchestrator_pid` if set; else the most recently started
live session bound to the initiative's `coordinator_task_id` (task board: the
root task). None → `orchestrator: null` and escalations go straight to Owner.

## 5. Pushes

The API writes an outbox row `{id, daemon_id, session_uid, pid, kind, text,
dedupe, owner_alert}`; the target's own daemon receives it in its heartbeat
reply and delivers it with
`notifications::publish(root, uid, "board-push:<id>", "board", text, "[cm-board <slug>]")`
— idempotent per id — or, when `owner_alert`, through Owner escalation
(`owner_attention::escalate()`). Delivered
ids are acked on the next beat; unacked rows are re-sent.

| Kind | To | When |
|---|---|---|
| `assigned` | new holder | added by someone else |
| `unblocked` | holders | last blocker done |
| `nudge` | holders | `item_resolve(nudge)` |
| `overdue` | holder when raised; orchestrator once the flag is `repush_s` old | flag |
| `board` | orchestrator | new flags immediately; unresolved flags every `repush_s` — whenever any flag is new or due, the push carries every open flag and their re-push clocks align, so k flags are one wake per window; done/dropped/blocked events by others than the orchestrator, batched over a `digest_s` window. One row per orchestrator per tick; no push without a live orchestrator |
| `escalation` (`owner_alert`) | Owner via the orchestrator's daemon (else the coordinator's most recent session, live or not) | no orchestrator, or it is `idle`/`unknown`/gone/`errored`/`waiting-on-human`, and flags unresolved for `escalate_s`; once per flag (`escalated_at`) |

Text stays under ~1 KB, e.g.
`[cm-board sfd] 2 flags: #14 holder_idle (rl-scale-out idle 24m); #9 stale 2h10m · 3 done (#3,#5,#6). board() / item_resolve(n, action)`.

**Close-out.** Items closed for 24 h get `archived_at` (per board, under the
board-row lock, so a concurrent reopen cannot interleave) (off the board, still
searchable with `archived=true`). Delivered pushes, state rows exited and
`item_requests` keys older than 7 days are pruned (about every 10 minutes).

## 6. Planning API (`api/items.py`)

Bearer token as for every endpoint. Errors are `{"detail": {"code", "message", …}}`
with codes `not_found` (404), `cycle` and `board_not_empty` (409, plus `cycle: [n…]` /
`open_items`),
`invalid_blocker`, `eta_required`, `invalid_status`, `invalid_field`,
`no_holders`, `item_closed`, `check_back_required` (422). Malformed bodies get
FastAPI's standard 422.

| Method + path | Body / query | Returns |
|---|---|---|
| `POST /boards/resolve` | `{task_id}` or `{ref}` | `board` header (creates if needed) |
| `GET /boards` | `?open_only=true` | `[board header]` |
| `GET /boards/{ref}` | `?since_version=&archived=false&q=&history=0` | `{board, items, flags, recently_closed, free_capacity}` (+ `archived` when `archived=true`) or `{unchanged: true, version}` |
| `DELETE /boards/{ref}` | (none) | `{deleted, id, items}`; `409 board_not_empty` with `open_items` while any item is open. Deletes the board's items, history, flags and pushes. For scratch boards |
| `PATCH /boards/{ref}` | `{actor, settings…, orchestrator_pid?}` | `board` header |
| `POST /boards/{ref}/items` | `{actor, items: [{title, holders?, status?, note?, group?, blocked_by?, blocked_on?, eta?, check_back?, links?}], request_id?}` (≤ 50, all-or-nothing) | `{board, items, unblocked, warnings}`; a repeated `request_id` on the board returns the first call's items with `replayed: true` |
| `PATCH /boards/{ref}/items` | `{actor, ns: [n], set: {…}, add_holders?, remove_holders?, reason?}` | `{items, unblocked: [n], warnings: [str]}` |
| `POST /boards/{ref}/items/{n}/resolve` | `{actor, action, kind?, holders?, blocked_by?, blocked_on?, check_back?, reason?, message?}` | `{board, item, flags_resolved: [kind], unblocked, warnings}` |
| `GET /items` | `?holder_pid=&open=true` | `[{board, n, title, status}]` |
| `POST /hosts/{daemon_id}/heartbeat` | `{host_label, sessions: [state row], exited: [pid], acked_push_ids: [id]}` | `{pushes: [{id, session_uid, pid, kind, text, owner_alert, board_id, board}], server_time}` (≤ 100 per beat) |

`holders` on the wire are resolved objects `{pid, name, session_uid,
daemon_id}`; name resolution happens in the daemon. `set` accepts the item
fields of §1 plus `eta` / `check_back` input forms; `null` clears a field.

Bounds: item numbers 1…2³¹−1, ≤ 50 numbers/holders per list, `reason` ≤ 500,
`message` ≤ 1000, ids and names ≤ 200, `eta`/`check_back` ≤ 64; thresholds
1 s…30 days. Out of range is a 422.

**Board header:** `{id, slug, name, initiative_id, root_task_id, version,
orchestrator: {pid, name, state} | null, settings, health: {unresolved,
oldest_s}}`.

**Item (read):** §1 fields plus `holders[].state = {state, for_s, reported_done}`,
`flags: [kind]`, `blocks: [n]`, and with `history=N` the last N events.

**Heartbeat state row:** `{pid, session_uid, task_id, name, engine, state,
state_age_s, idle_for_s, age_s?, reported_done, killed_by?, agent_state?}`;
rows are checked one by one: a row whose `pid` is not `agent:<daemon_id>:…` of
the posting daemon, or that lacks `session_uid`/`state`, is dropped and listed
in the reply's `dropped_sessions`; over-long text is clipped and bad numbers
read as null, so one bad row never fails the host's beat. TUI-owned sessions
are sent with state `unknown`. The snapshot is complete for that daemon: its
other live rows, and pids in
`exited`, are marked exited; a pid that reappears is live again. Acks only
settle that daemon's own pushes. The daemon beats every 30 s and 2 s after a
poke; it publishes each push as notification id `board-push:<id>` with marker
`[cm-board <board>]`, sends `owner_alert` pushes to Owner escalation (urgency
`blocking`, key `board:<board>:<uid>` so it never merges with the session's
own `notify_user` alert), acks pushes for sessions that cannot receive them
(gone, bash, unsendable text), and acks everything on the next beat. Each
hand-out counts an attempt; after 10 unacked attempts the API settles the push
with a `dropped_reason`, so a stuck push cannot starve newer ones.

`item_resolve` actions: `nudge` (push + snooze; needs a holder), `reassign`
(replace holders), `block` (`blocked_by`, or `blocked_on` + `check_back`),
`drop` (reason). Without `kind` an action resolves all the item's open flags.
Actions other than `drop` are refused on a closed item. `launch` is completed
by the daemon: it spawns the session, then calls `reassign`.

## 7. Daemon RPCs and MCP tools

Daemon methods (daemon-only; no TUI handler, no CLI fallback): `item.create`,
`item.set`, `item.resolve`, `board.read` (`board.read` is restart-barrier
read-only). The daemon resolves the caller's pid, task and board, resolves
holder names, enforces §2 rule 9, stamps `actor`, and forwards. The Operator
(TUI) acts as `owner`.

Holder arguments accept a chat name, a session uid, a participant id, `me`, or
`none`. An unknown or ambiguous name is refused with the candidates.

```
item(title: str | list[str], holder=None, status="active", note=None, group=None,
     blocked_by: list[int] | None = None, blocked_on=None, eta=None,
     links: list[str] | None = None, board=None)
  -> {board, items: [{n, title, status, holders}]}

item_set(n: int | list[int], status=None, note=None, holder=None, add_holder=None,
         remove_holder=None, group=None, blocked_by=None, blocked_on=None,
         check_back=None, eta=None, title=None, links=None, reason=None, board=None)
  -> {items, unblocked?, warnings?}

board(board=None, view="slim", mine=False, group=None, include_closed=True,
      archived=False, query=None)
  -> {board: {slug, orchestrator, health}, flags, items, recently_closed, free_capacity}

item_resolve(n: int, action: "nudge"|"reassign"|"launch"|"block"|"drop",
             holder=None, blocked_by=None, blocked_on=None, check_back=None,
             reason=None, engine=None, message=None, kind=None, task_id=None,
             board=None)
  -> {item, flag_resolved}
```

- A bare string as `item_set`'s second argument is `status`.
- MCP arguments left out are unchanged. An empty string clears `note`,
  `group`, `blocked_on`, `check_back` or `eta`; `blocked_by=[]` clears
  blockers; `holder="none"` hands the item back.
- `board.read` also takes `since_version` (for the TUI's poll) and then may
  answer `{unchanged: true, version}`.
- `board(view="slim")` returns one line per item, flags first, then by group:
  `#14 active "fuse SEJD" @rl-scale-out[idle 24m] ⚑holder_idle · waiting on JP`.
  `view="full"` returns the item dicts with the last 5 events each.
- `item_resolve(launch)` spawns through the normal `start_session` path under
  the caller's permissions (`engine` defaults to the caller's own engine), with
  prompt `item #<n> (<board>): <title>` plus `message`, then makes the new
  session the sole holder. Like `start_session`, it joins the caller's task
  and checkout unless `task_id` names a task (whose worktree it then gets).
  The daemon route registers no completion monitor;
  the item's flags are the follow-up.
- `item` and `item_resolve(launch)` carry a `request_id` (the MCP tool makes
  one when omitted). A retried launch with the same id reassigns the worker
  it already spawned instead of starting another (remembered in the daemon
  until a brain restart); if the final reassign fails, the error names the
  spawned uid. `launched` passes through the spawn's `worktree_path`,
  `shared_workspace`, `workspace_shared_with` and warnings. A TUI-owned
  caller cannot launch (it is not in the daemon's spawn registry). The MCP
  timeouts (90 s, 180 s for launch) exceed the worst-case call chain.
- `report_done` additionally returns `held_items: [{n, board, title, status}]`
  for the caller's open items (an empty list when none), plus
  `held_items_hint` ("close each … or hand it back …") when any are open. The
  lookup is bounded to 2 s; any failure appears as `held_items_error` and
  never fails `report_done`. `report_done` and every session exit (a recorded
  exit tombstone) also poke the heartbeat, so the board sees them within
  seconds.
- If the planning API is unreachable, item tools fail fast with
  `planning_api_unavailable`; there is no local queue.
