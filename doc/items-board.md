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
| `status` | `open` `active` `waiting` `blocked` `blocked_on_owner` `done` `dropped` | default `active` |
| `holders` | `[{pid, name, session_uid, daemon_id}]` | default: the caller |
| `note` | str ≤ 500 | one line, latest wins |
| `group` | str ≤ 80 | free-text heading |
| `blocked_by` | `[n]` | open items on the same board |
| `blocked_on` | str ≤ 200 | free text; no flag exemption |
| `check_back_at` | ts | re-flag time for `blocked_on` |
| `eta_at`, `waiting_set_at` | ts | set with `waiting`, or on `active` |
| `job` | str ≤ 200 | the job in flight on an `active` item (lane, PID, cell id) |
| `actionable_at` | ts | when it last became actionable (created or entered `active`, unblocked, handed back, assigned); the `unlaunched` clock |
| `launched_at` | ts | a holder's own last write or `#N` post on it |
| `links` | `[str]` ≤ 20 | task id, branch/SHA, message id, path, URL |
| `touched_at` | ts | last status/note/holder change |
| `clock_reset_at` | ts | set on auto-unblock |
| `closed_at`, `archived_at` | ts | |
| `created_by` | pid | |

**Strict boards.** A board with `backlog_allowed: false` (§4 settings) holds
only running or blocked work; see §2 rule 11 and
[strict-boards.md](strict-boards.md). Every other board keeps the rules
below unchanged.

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
   On a strict board an `eta` keeps the item `active`, and a handback or an
   unblock leaves an unheld item `active` (never `open`).
   Only a `blocked` item carries `blocked_by` / `blocked_on` / `check_back`:
   leaving `blocked` clears them, and passing them with another explicit
   status is `422 invalid_field`. `blocked` with neither returns a warning.
2. **Touch.** Every write by a board member (any `item_set`, even one that
   changes nothing, recorded as a `touched` event; `item_resolve`, including
   `nudge`) sets `touched_at`, so the orchestrator parking or checking an item
   resets its stale clock too. Observed activity also touches (2026-10-20): a
   chat post refreshes open item N when **all** hold — the sender holds N or
   is the board's `steward_pid` or orchestrator (explicit or picked); N
   resolves on the board whose `channel` is the post's channel, else on the
   only board where N is open and the sender holds it or is that board's
   explicit `steward_pid` / `orchestrator_pid` (several → skipped
   `ambiguous`); and the post is a
   top-level post naming `#N` or a reply in a thread whose root names `#N`.
   A reply naming some other `#M`, anyone else's mention, and quoted (`>`)
   lines are references only. Event `chat_ref` with the message id and a
   200-character excerpt; status and note never change. Only a holder's post
   resets the stale clock: a steward's or orchestrator's post about someone
   else's item is history only, so an item that keeps being nagged still
   goes stale and escalates. The sender's daemon
   does this after the send, on one bounded background worker, through
   `POST /items/chat-touch` (falling back to the holder-only
   `POST /boards/{ref}/items/touch` against an older API).
3. **`blocked_by`.** Every blocker must exist on the same board and be neither
   `done` nor `dropped` → else `422 invalid_blocker`. An item cannot block
   itself. A new edge that closes a cycle → `409 cycle` with the path, e.g.
   `cycle: 14→15→14`. Passing `blocked_by=[]` removes all edges.
4. **`waiting`** requires `eta` → else `422 eta_required`. `eta` is a duration
   (`40m`, `2h`, `1h30m`) or an RFC 3339 time; stores `eta_at` and
   `waiting_set_at=now`; leaving `waiting` clears both. An `active` item may
   carry an ETA too (explicit `status="active"` with `eta`: working, expected
   done by then; kept on later edits while it stays `active`); `eta` with any
   other explicit status is refused. `waiting` stays for parked work.
   `check_back` takes the same forms. `job` rides on an `active` item like
   its ETA: passing it with another explicit status is `422 invalid_field`,
   and leaving `active` clears it. On a strict board a `job` needs an `eta`
   (`422 eta_required`), so it always expires.
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
   `archived_at`. A **done** item reopens only on purpose (2026-10-24, sejd
   #1665/#1643/#1505: done → blocked writes from stale reads): any write that
   would give it another status, `dropped` included and on every route
   (`item_set`, bulk, `item_resolve`), is refused `409 reopen_required`
   naming who closed it and when (`closed_by`, `closed_at`) unless it passes
   `reopen: true`. Owner's writes (the TUI, Alt+B) imply it. The `updated`
   event records `reopened: "reopen=true"` or `"owner"`. Writes that keep it
   done (a note) are unaffected. Blockers named in a reopen are re-validated by rule 3.
   Reopening a dropped blocker clears its dependents' `blocker_dropped`, as
   does re-pointing a dependent's `blocked_by` away from dropped items.
8. **Assigned.** A holder added by an actor other than that holder queues an
   `assigned` push to the new holder.
9. **Blocked on Owner** (`blocked_on_owner`, sql/019): waiting on a decision
   only Owner can make. Owner may set it; a holder may set it only while
   naming the decision in `blocked_on` (else `422 decision_required`); anyone
   else gets `403 owner_only`. Only Owner moves it to any other status
   (including done/dropped, `403 owner_only` otherwise), except
   `item_resolve(n, "answer", message=…)`, which records Owner's answer (note
   `Owner: …`, event reason `answer: …`) and returns it to `active` (`open` if
   unheld). `blocked_by` is not allowed with it; `blocked_on` keeps the
   decision text. Every move in or out pushes the orchestrator at once (kind
   `owner_blocked`) unless the orchestrator made the change.
10. **Editing rights** (enforced by the daemon, §5): any session whose task
   resolves to the board, any current holder on it, a `global_perms` session,
   and Owner. Reading is open to every session.
11. **Strict boards** (`backlog_allowed: false`). New non-conforming intent
   is refused, and every refusal names the call that conforms; releases are
   accepted and flagged. A write that leaves an already non-conforming item
   as it was still succeeds, with a warning (`needs_triage` carries it).

   | Write | Strict board |
   |---|---|
   | entering `waiting` | `422 waiting_not_allowed`: active + `eta` + `job` for a job in flight, else `blocked_by` / `blocked_on` + `check_back` |
   | an `eta` with no status | keeps (or makes) the item `active` |
   | `open`, or a new item with no holder | `422 holder_required`: give it a holder, `item_resolve(n, "launch")`, or file a planning task. Except Owner's add (Alt+B `a`): created `active` with no holder, so `unlaunched` reaches the orchestrator, which assigns it |
   | `holder="none"` on an `active` item | accepted: stays `active`, unheld, with a warning; `unlaunched` after `launch_s` |
   | `blocked` with neither trigger | `422 trigger_required` |
   | `blocked_on` as the only trigger without `check_back` | `422 check_back_required` |
   | `check_back` beyond `check_back_max_s` (72 h) | `422 check_back_too_far` |
   | unheld `blocked` (by items, or on text with a check-back) | allowed: its trigger reaches someone |
   | `blocked_on_owner` | unchanged |

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
| `repush_s` | 1800 (how long the holder alone hears about `overdue` / `eta_passed`) |
| `escalate_s` | 3600 |
| `digest_s` | 300 |
| `stale_background_s` | 21600 (6 h) |
| `nudge_grace_s` | 1800 (30 min) |
| `holder_idle_enabled` | **false** until engine state is live on all hosts |
| `orchestrator_pid` | null = auto (below) |
| `steward_pid` | null; its `#N` posts are recorded on any item (§2). Set only by Owner or the board's current orchestrator (else `403`) |
| `channel` | null; the conversation path whose `#N` means this board's item N |
| `backlog_allowed` | true; false makes the board strict (§2 rule 11) |
| `launch_s` | 900 (strict boards: `unlaunched`) |
| `max_active_per_holder` | 2 (strict boards: `holder_overloaded`) |
| `check_back_max_s` | 259200 (72 h; strict boards) |

`orchestrator_pid`, `steward_pid`, `backlog_allowed`, `idle_s`, `launch_s`,
`max_active_per_holder` and `check_back_max_s` are set only by Owner or the
board's current orchestrator, as it is before the change (else `403`): a
member cannot name itself orchestrator and then use that. `board.settings`
forwards only the settings in this table plus `name`.

| Kind | Raised when |
|---|---|
| `unassigned` | `open`, no holders, older than `unassigned_s` |
| `holder_gone` | item held (below) and any holder exited ≥ 90 s ago, or has had no state row for ≥ 90 s since it was added while its daemon is heartbeating (a daemon not yet sending heartbeats leaves its holders `unknown`); applies even while blocked |
| `holder_idle` | `active` (or `open` with holders), no holder busy in background work, every holder `idle` with `now − max(idle_since, clock_reset_at, touched_at) ≥ idle_s` |
| `holder_waiting_on_human` / `holder_errored` | any holder in `waiting-on-human` / `errored` |
| `stale_background` | held, a holder is busy in background work (state `working-background`, or a live `agent_state.background` job with `wakes_agent`), and untouched for `stale_background_s` (default 6 h) |
| `stale` | held, no holder busy in background work, not parked (`blocked_on` + a future `check_back_at`), `now − max(touched_at, clock_reset_at) ≥ stale_s`; for `waiting` the threshold is `max(stale_s, eta_at − waiting_set_at)` |
| `overdue` | `waiting` and `now > eta_at + 0.25·(eta_at − waiting_set_at)` |
| `eta_passed` | `active` with an ETA (capped at `stale_background_s` after it was set, so a far ETA cannot silence an item for weeks), `now ≥ eta_at`, and no activity since (`max(touched_at, clock_reset_at) ≤ eta_at`); `ETA_STALE_GRACE_S` (1800, fixed) after the ETA, still without activity, `stale` too (detail `{eta_at}`, no nudge round). New activity or a new ETA clears both |
| `check_back` | `blocked_on` set and `check_back_at` passed |
| `blocker_dropped` | a blocker of this item was dropped |
| `needs_triage` | strict board: the item does not fit (detail `{reason}`: `waiting`, `open`, `no_trigger`, `no_check_back`, `check_back_too_far`, `job_without_eta`). Raised for every legacy item when the board turns strict; statuses are never rewritten. Clears when it conforms, or when the board allows backlog again. Reaches only the orchestrator and is never escalated |
| `unlaunched` | strict board: `active`, not inside a job's ETA, `now − actionable_at ≥ launch_s`, `holder_gone` not raised, and nobody live on it: no holder, or every holder `idle` and no holder wrote or posted `#N` about it itself since `actionable_at` (`launched_at`; a holder's own claim or job declaration counts, an orchestrator's note does not; `unknown` counts as live). A passed `check_back` never starts this clock. Detail `{actionable_at}`. To the orchestrator, naming up to three idle free sessions; never escalated to Owner (only an orchestrator escalates) |
| `holder_overloaded` | strict board: a holder (not the orchestrator or steward) holds more than `max_active_per_holder` `active` items outside a job's ETA; raised on each such item beyond its most recently touched ones. Detail `{holders}`. The holder and the orchestrator each hear once per holder episode (the flag moving between items as the focus changes is not news); never escalated, never reassigns |
| `suggested_transition` | a holder's, the steward's or the orchestrator's post about exactly one item (a thread reply must name that item itself) says `DONE`, `GREEN` or `DEPLOYED` (→ done) or `STOP` (→ blocked), in capitals. Never counted: quoted lines, code (fences, backticks), a word followed by `?`, "GREEN light", or a negation, condition or future (`not`, `don't`, `once`, `will be`, …) in the four words before it; conflicting words suggest nothing. Not raised on items blocked on Owner. Detail `{to, keyword, by, message_id}`. A suggestion only: the holders and the steward get one push per target (a new target settles the old suggestion and is pushed afresh); it never reaches the orchestrator or Owner; any status change settles it. `item_resolve(n, "apply", to=…)` confirms exactly that target (409 `suggestion_changed` if it moved); `"dismiss"` rejects it, and that target is not suggested again until the item's status or head changes (Alt+B `x` → 5 / 6) |

"Held" means `active`, `waiting`, `blocked`, or `open` with holders: an
`open` item that keeps holders gets the same holder and staleness flags as
`active`, so nothing blocked behind it can hide.

**Exemptions.** An item blocked on Owner never gets `holder_idle` or `stale`
(`holder_gone` still applies). An item blocked only by open items (`blocked_by` non-empty,
`blocked_on` empty) is exempt from `holder_idle` and `stale`. Free-text
`blocked_on` earns no exemption. `waiting` before its overdue point is exempt
from `holder_idle`. An `active` item before its ETA gets no `holder_idle`,
`stale`, `stale_background` or `check_back`; errored, waiting-on-human and
blocker flags still apply, and a holder that is gone or errored ends the
window at once. Nothing exempts `holder_gone`. Since `blocked_by` cannot
form a cycle, every waiting chain ends at an unexempt item.

**Nudge before flag.** When `stale` first holds, the engine pushes the holder
a quiet `stale_nudge` ("#N looks stale: item_set it … or post #N in chat"),
records `items.stale_nudged_at`, and raises `stale` (and tells the
orchestrator) only if the item is still untouched after `nudge_grace_s`
(default 30 min). A touch after the nudge starts a new episode.

**Lifecycle.** At most one open flag per `(item, kind)`; re-raising an open
flag with new detail updates the detail (event `flag_updated`). A flag auto-resolves
with resolution `cleared` when its condition stops holding, `closed` when the
item closes, or the action name when resolved by `item_resolve`. `nudge` sets
`snooze_until = now + <that kind's threshold>`; the flag is resolved and may
re-raise only after the snooze.

**Free capacity** (not a flag): live agent sessions on the board (task
resolves to it or holding any item there) that hold no `active`/`waiting`
item, never bash panes, the orchestrator or the steward, idle ones first.
Those that are `working`/`working-background` are listed as `untracked`
instead: busy with work the board does not show. `launch` pairs the
`unlaunched` items with the free sessions.

**Orchestrator.** `orchestrator_pid` if set; else a live session bound to the
initiative's `coordinator_task_id` (task board: the root task). Bash panes
never qualify; Claude/Codex sessions beat any other engine; ties go to the
most recent activity (latest state change or turn end, then latest report),
not to start time. None → `orchestrator: null` and escalations go straight to
Owner, through the coordinator's most recent non-bash session.

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
| `stale_nudge` | holder | `stale` first holds (the orchestrator hears only after `nudge_grace_s`) |
| `owner_blocked` | orchestrator | an item enters or leaves `blocked_on_owner` (immediately, not batched) |
| `nudge` | holders | `item_resolve(nudge)` |
| `holder_overloaded` | the overloaded holder | once per episode (strict boards) |
| `overdue` / `eta_passed` | holder when raised; orchestrator once the flag is `repush_s` old | flag |
| `board` | orchestrator | flags that are news: raised, or open while their item changed since they last reached the orchestrator (below); done/dropped/blocked events by others than the orchestrator, batched over a `digest_s` window. One row per orchestrator per tick, carrying only the news; no push without a live orchestrator |
| `escalation` (`owner_alert`) | Owner via the orchestrator's daemon (else the coordinator's most recent session, live or not) | no orchestrator, or it is `idle`/`unknown`/gone/`errored`/`waiting-on-human`, and flags unresolved for `escalate_s`; once per flag and item state |

**Change-only pushes (2026-10-20).** A flag reaches each audience (holder,
orchestrator, Owner) again only when its item changed since it last did: a
different status, holders or flag detail, or any activity (every member
write touches the item, so ETA, blocker and note changes count). Never on a
timer, and never just because the flag cleared and came back — except
`holder_errored`, `holder_waiting_on_human`, `holder_gone` and
`blocker_dropped`, where each new raise is a new episode (another prompt,
error, exit or drop) and is pushed again. A holder
flickering between background work and idle used to re-raise
`stale_background` and push the same flags over and over (EP). The engine
keeps `items.flag_pushed = {"<kind>[:holder|:escalated]": {fp, at}}`.

Text stays under ~1 KB, e.g.
`[cm-board sfd] 2 flags: #14 holder_idle (rl-scale-out idle 24m); #9 stale 2h10m · 3 done (#3,#5,#6). board() / item_resolve(n, action)`.

**Close-out.** Items closed for 24 h get `archived_at` (per board, under the
board-row lock, so a concurrent reopen cannot interleave) (off the board, still
searchable with `archived=true`). Delivered pushes, state rows exited and
`item_requests` keys older than 7 days are pruned (about every 10 minutes).

## 6. Planning API (`api/items.py`)

Bearer token as for every endpoint. Errors are `{"detail": {"code", "message", …}}`
with codes `not_found` (404), `cycle` and `board_not_empty` (409, plus `cycle: [n…]` /
`open_items`), `reopen_required` (409, plus `closed_by`, `closed_at`,
`would_be`) and `status_changed` (409, plus `current_status`, `expected`),
`invalid_blocker`, `eta_required`, `invalid_status`, `invalid_field`,
`no_holders`, `item_closed`, `check_back_required`, and on strict boards
`waiting_not_allowed`, `holder_required`, `trigger_required`,
`check_back_too_far` (422). Malformed bodies get
FastAPI's standard 422.

| Method + path | Body / query | Returns |
|---|---|---|
| `POST /boards/resolve` | `{task_id}` or `{ref}` | `board` header (creates if needed) |
| `GET /boards` | `?open_only=true` | `[board header]` |
| `GET /boards/{ref}` | `?since_version=&archived=false&q=&history=0` | `{board, items, flags, recently_closed, free_capacity, untracked, launch: {unlaunched: [{n, title, since}], free: [name]}}` (+ `archived` when `archived=true`) or `{unchanged: true, version}` |
| `DELETE /boards/{ref}` | (none) | `{deleted, id, items}`; `409 board_not_empty` with `open_items` while any item is open. Deletes the board's items, history, flags and pushes. For scratch boards |
| `PATCH /boards/{ref}` | `{actor, settings…, orchestrator_pid?}` | `board` header |
| `POST /boards/{ref}/items` | `{actor, items: [{title, holders?, status?, note?, group?, blocked_by?, blocked_on?, eta?, check_back?, links?}], request_id?}` (≤ 50, all-or-nothing) | `{board, items, unblocked, warnings}`; a repeated `request_id` on the board returns the first call's items with `replayed: true` |
| `PATCH /boards/{ref}/items` | `{actor, ns: [n], set: {…}, add_holders?, remove_holders?, reason?, each?, reopen?, expect_status?}` | `{items, unblocked: [n], warnings: [str]}`. `expect_status` is a compare-and-set checked under the board-row lock: `409 status_changed` (with `current_status`) unless each item has that status. A list is all-or-nothing (one refusal refuses the write); `each: true` reports per-item `results` and applies the rest |
| `POST /boards/{ref}/items/{n}/resolve` | `{actor, action, kind?, holders?, blocked_by?, blocked_on?, check_back?, reason?, message?}` | `{board, item, flags_resolved: [kind], unblocked, warnings}` |
| `GET /items` | `?holder_pid=&open=true` | `[{board, n, title, status}]` |
| `POST /boards/{ref}/items/touch` | `{actor, ns: [n], source: "chat", message_id?, excerpt?}` | `{board, touched: [n], skipped: [{n, reason: not_found|closed|not_holder}]}`; only open items the actor holds; never changes status or note |
| `POST /items/chat-touch` | `{actor (with task_id), ns: [n], channel?, source?, message_id?, excerpt?}` | `{touched: {slug: [n]}, skipped: [{n, reason: not_found|ambiguous|closed|not_holder, board?}]}`; the chat rules of §2 |
| `GET /items/owner-blocked` | (none) | `[{board, board_name, n, title, decision, since, holders: [{pid, session_uid, daemon_id, name}]}]`, oldest first, across boards |
| `POST /hosts/{daemon_id}/heartbeat` | `{host_label, sessions: [state row], exited: [pid], acked_push_ids: [id]}` | `{pushes: [{id, session_uid, pid, kind, text, owner_alert, board_id, board}], server_time}` (≤ 100 per beat) |

`holders` on the wire are resolved objects `{pid, name, session_uid,
daemon_id}`; name resolution happens in the daemon. `set` accepts the item
fields of §1 plus `eta` / `check_back` input forms; `null` clears a field.

Bounds: item numbers 1…2³¹−1, ≤ 50 numbers/holders per list, `reason` ≤ 500,
`message` ≤ 1000, ids and names ≤ 200, `eta`/`check_back` ≤ 64; thresholds
1 s…30 days. Out of range is a 422.

**Board header:** `{id, slug, name, initiative_id, root_task_id, version,
orchestrator: {pid, name, state} | null, settings, health: {blocked_on_owner,
owner_items: [n], unresolved, oldest_s}}`.

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
`item.set`, `item.resolve`, `board.read`, `board.settings`,
`board.owner_blocked` (both reads are restart-barrier read-only;
`board.settings` is `PATCH /boards/{ref}` for board members). The daemon resolves the caller's pid, task and
board, resolves holder names, enforces §2 rule 10, stamps `actor`, and
forwards. The Operator (TUI) acts as `owner`.

Gate receipts add `gate.report`, `gate.receipts`, `gate.waive` and
`gate.profile` (MCP `gate_report`, `gate_receipts`, `gate_waive`,
`gate_profile`) and the runner `scripts/cm-gate`; see
[gate-receipts.md](gate-receipts.md) §9.

**TUI.** Every 20 s the viewer reads `board.owner_blocked` (one small filtered
query, first reachable daemon) and marks each holder's sidebar row and its
workspace header with an amber ◇/◆ alternating about once a second plus a bold
`OWNER` tag, ranked just below a pending ⚑ alert; the status bar adds `◆N
OWNER`, N being the holder sessions (a session holding three such items counts once)
plus one for each such item nobody holds. In the board overlay the item's badge is `OWNER` with the diamond in the
flag cell, the banner leads with "N blocked on Owner", and `s` → `O` lets Owner
set the status (any other status clears it).

Blockers are shown on the row itself, in the tail column before the note:
`⟵ #1 bench run (WAIT 32m)` (first blocker, its badge, `+N` for more), or the
quoted `blocked_on` reason with `check back 20m` (red `overdue` once past;
shortened to `back 20m` on narrow terminals, never the reason). Items others
wait for show `→ blocks #4 #7` (the read's `blocks`, else computed in the
viewer). Under the summary, `⟵ N blocked: on items · on Owner · external`.
The detail pane lists **Blocked by** (each blocker's badge, title, holder
chips and ETA, then the reason and check-back time) and **Blocks**.
Gate receipts ([gate-receipts.md](gate-receipts.md)) add a cell after the badge
(`✓PASS` `✗FAIL` `⊘VOID` `◐INC` `⟲STALE`; the worst gate, `+N` for others), a
**Gate** section per gate in the detail pane, and `N stale receipts` /
`runner VOID ×N` in the banner.

Holder arguments accept a chat name, a session uid, a participant id, `me`, or
`none`. An unknown or ambiguous name is refused with the candidates.

```
item(title: str | list[str], holder=None, status="active", note=None, group=None,
     blocked_by: list[int] | None = None, blocked_on=None, eta=None, job=None,
     links: list[str] | None = None, board=None)
  -> {board, items: [{n, title, status, holders}]}

item_set(n: int | list[int], status=None, note=None, holder=None, add_holder=None,
         remove_holder=None, group=None, blocked_by=None, blocked_on=None,
         check_back=None, eta=None, job=None, title=None, links=None, reason=None, board=None,
         head=None, mf=None, ruling=None, gate_class=None, each=False,
         reopen=False, expect_status=None)
  -> {items, unblocked?, warnings?, results?}
  # reopen=True: needed to move a done item to any other status (§2 rule 7)
  # expect_status: refuse unless the item's status is still the one you read
  # head / mf / ruling / gate_class: gate bindings (gate-receipts.md §8)

board(board=None, view="slim", mine=False, group=None, include_closed=True,
      archived=False, query=None, settings=None)
  -> {board: {slug, orchestrator, health, backlog_allowed}, flags, items, recently_closed,
      free_capacity, untracked?, launch?}
  # settings={...}: board.settings first, e.g. {"stale_s": 9000}; the governed
  # settings (orchestrator_pid, steward_pid, backlog_allowed, idle_s, launch_s,
  # max_active_per_holder, check_back_max_s) are Owner's or the orchestrator's
  # only, e.g. {"idle_s": 1800}

item_resolve(n: int, action: "nudge"|"reassign"|"launch"|"block"|"drop"|"answer"|"apply"|"dismiss",
             holder=None, blocked_by=None, blocked_on=None, check_back=None,
             reason=None, engine=None, message=None, kind=None, task_id=None,
             board=None)
  -> {item, flag_resolved}
```

- A bare string as `item_set`'s second argument is `status`.
- `item_set` on a list of `n` is all-or-nothing. `each=True` writes each item
  on its own and adds `results: [{n, ok, error?}]`, so one refused item does
  not block the rest (bulk housekeeping, e.g. a wave of stale flags). Every
  failure is that item's result; if the database goes away mid-way, items
  already written stay written and the rest report `db_unavailable`. API:
  `PATCH /boards/{ref}/items` with `"each": true`.
- MCP arguments left out are unchanged. An empty string clears `note`,
  `group`, `blocked_on`, `check_back` or `eta`; `blocked_by=[]` clears
  blockers; `holder="none"` hands the item back.
- `board.read` also takes `since_version` (for the TUI's poll) and then may
  answer `{unchanged: true, version}`.
- `board(view="slim")` returns one line per item, flags first, then by group:
  `#14 active "fuse SEJD" @rl-scale-out[idle 24m] ⚑holder_idle · waiting on JP`;
  a job shows beside the holder's state, never merged with it:
  `#1216 active "pins gate" @release-4[idle 12m] job: cm-gate pins L3 (eta 25m)`.
  `launch` is one line, `2 unlaunched (#14 18m, #15 4m) · free: lane-a`, and
  `untracked` lists working sessions that hold no item.
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

## 8. Groups, epics, placement and roles (org model S3)

Design: [org-model.md](org-model.md) §7. Migrations `sql/029` (DDL) and
`029b` (one-time: `items.grp` → heading groups; holder roles).

**Groups.** `groups` rows per board: `kind` `heading` (a topic label, as
`items.grp` was) or `epic` (goal, `epic_status` active|done|cancelled, a team
from org S2 whose lead is the PoC; `poc_member_id` is derived, never stored).
Both nest; a name is unique among siblings; cycles are `409 cycle`.
`item_set(group="X")` finds the group by name (unique match, else the
top-level one; several → `422 ambiguous_group`) or creates a top-level
heading; `group_id` picks one by id. `items.grp` keeps the name for old
readers. A closed epic takes no new items (`409 epic_closed`). Creating or
promoting an epic, setting its PoC and closing it are Owner's or the board
lead's (its orchestrator's) acts; headings are anyone's.

**Placement.** `POST /tasks/{id}/place {actor, board, group?, group_id?,
holder?, role?, replace?}` links a planning task to a new item (`items.task_id`;
one live link per task). The task owns title, description and metadata
(board edits write the task); the item owns status, holders, group and flags.
Refused (`409 not_placeable`): backtests, continuous tasks, auto-dispatchable
cloud tasks; also a task with no description (`422 description_required`), a
done or archived task (`409 task_closed`), and a task placed on another board
(`409 placed_elsewhere` unless `replace`). Placing a draft accepts it.
Re-placing on the same board reactivates the archived link (same number).
`POST /tasks/{id}/unplace {actor, reason?}` archives the link (the item stays,
read-only: writes are `409 link_archived`) and returns the task to `backlog`;
refused while live `blocked_by` edges touch the item (`409 has_dependencies`).
Task rows carry `placement: {board, board_id, n, status, group_id, group,
item_id, mirror?, mirror_waiting_on?}` (null when not placed), written in the
same transaction, so the task change feed carries it.

**Status mirror.** open → backlog; active, waiting → running; blocked,
blocked_on_owner → blocked; done → done; dropped → done with
`metadata.dropped`. A board close stamps `metadata.terminal_via = "item"`,
which the daemon's lifecycle guard reads (the terminal sweep only unassigns).
The mirror **fails closed**: it runs only while every daemon that
heartbeated in the last 120 s advertises `lifecycle_guard_v1` (org S2's
`org_daemons`), there is at least one, and every daemon holding the task's
live sessions is among them. Otherwise `placement.mirror = "off"` with
`mirror_waiting_on`, the task status is left alone, and legacy status writes
keep updating the task row as before. The board engine re-mirrors such
tasks once the gate opens. The capability is advertised only by a build with
both the sweep guard and org's home-mark apply, so until both have landed
the mirror is off everywhere. `CM_ORG_MIRROR=0` turns it off (links kept). Item writes take the task-change
advisory lock first, then the board and task rows.

**Legacy task writes** to a placed task (`PATCH /tasks/{id}`,
`set_subtask_status`, `update_task`; optional `actor`, `reason`,
`blocked_by`): the status it already mirrors is ignored; `done` closes the
item; `blocked` with a reason or blockers blocks the item on them (reason →
`blocked_on` with a 24 h check-back); `blocked` alone gets
`blocked_on = "legacy status write: no reason given"`, a 24 h check-back and a
`legacy_blocked` flag pushed to the holders; `backlog` unplaces; `running`
activates (an unheld item's new owner is the writing session, e.g. the one a
spawn just started); `archived` on a closed item archives the link too
(planning's bulk archive), on an open one it is refused; anything else is
`409 placed_on_board`. A description
PATCH goes through the item: `description_rev` moves and the holders get a
`description_changed` push.

**Roles.** `item_holders.role`: `owner`, `contributor`, `reviewer`,
`approver`, or the migrated `holder` (co-owner). An active, waiting or blocked
item that is held has exactly one owner (`422 owner_required` /
`one_owner`); items with migrated co-owners are exempt until one is named
(`pick_owner` flag for the steward). New holders: the first is owner, later
ones contribute; `add_holders` takes `role`; `owner` transfers ownership in
one write (the previous owner and any co-owners become contributors); when the
owner leaves with one holder left, that holder becomes owner. Hand-back is
unchanged (strict boards keep the item active and unheld).

**Launch grants.** The owner of a placed item holds a `launch_grants` row for
its task, granted only when Owner, the board's orchestrator or steward, or
the previous owner handing over made the owner (a session naming itself
owner gets the role, not the grant); revoked on close, hand-back or unplace.
Placing and unplacing need the task in the caller's tree (or Owner/global),
and unplacing needs edit rights on the link's board. Each
heartbeat reply carries the daemon's full live set (`launch_grants`); the
daemon persists it and admits `start_session(task_id=…)` /
`item_resolve(launch)` for that task from the grant holder even where the
task-tree scope would refuse (taskless included), logging each use. Launch
only; no control of the task's other sessions.

**possible_duplicate.** Create and place replies carry hints for open items on
the board sharing two significant title words (≥40% of them) or a linked
path.

**Epics** (`dispatch/epics.py`, needs org S2 for teams; else `409
org_core_missing`):

| Call | Effect |
|---|---|
| `POST /boards/{ref}/epics/{gid}/poc {actor, member, team_id?}` | the epic is run by `member`'s team: `team_id` (one it leads), the epic's team if it leads it, else a new `kind='team'` team (parent: the enclosing epic's team, else the board's). Returns `staffing` offers for the current holders (`primary` = a move under the PoC, `guest` = keeps its manager) |
| `POST /boards/{ref}/epics/{gid}/staff {actor, member, as}` | accept an offer: a guest membership, or org's `move` (manager chain or Owner) |
| `POST /boards/{ref}/epics/{gid}/close-plan {actor, outcome}` | build and persist the close plan: `{plan_id, revision, items[{n, title, choice_options[move,unplace,drop]}], move_targets, team{options, default}, members[{member_id, role, options, default, needs}], workspaces[]}`. Refused with active sub-epics; a rebuild bumps the revision; a plan being applied is resumed, never replaced |
| `POST /boards/{ref}/close-plans/{id}/apply {actor, revision, choices}` | only on the epic's own board; one applier at a time (409 `close_in_progress`); validates every item choice, then applies step by step (recorded, so a retry resumes); items closed or unplaced since the plan are `skipped`; team standby/archive without Owner and every member move/retire come back as `pending` with `who_must_decide` (the move itself is org's `move`); ends guest memberships; after a final nothing-left-open check under the board lock the epic closes with a `close_record` |

Read-only: `GET /items/on-boards?since_version=` → `{version, epics[], items[]}` or `{unchanged, version}` (open epics and open
board-native items, across boards).

Daemon RPCs (all daemon-only, actor stamped): `item.place`, `item.unplace`,
`group.create`, `group.set`, `epic.set_poc`, `epic.staff`, `epic.close_plan`,
`epic.close_apply`, `boards.on_boards` (read-only). MCP tools: `place`,
`unplace`, `epic(action=create|promote|set_poc|staff|close)`, and on
`item`/`item_set`: `group_id`, `description`, `owner`, `role`, `team_id`.

**Lifecycle guard (daemon, TUI).** The terminal sweep and `mark_subtask_done`
(daemon and TUI routes) never close a member's main session (org S2's member
cache, persisted at `<cm>/org-members.json` so a restarted brain still knows
them) and never remove a pinned (later home/stream) workspace or one a spared
member main still uses (`workspace_kept: "pinned" | "member"`). A
`terminal_via=item` close closes only ephemeral workspaces' sessions.
