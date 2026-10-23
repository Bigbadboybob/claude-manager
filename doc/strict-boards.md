# Strict boards: design

Status: **approved by EP (2026-10-23) and implemented** on `items/strict-boards`.
EP's §8 answers: refuse intent and flag releases, with errors naming the
conforming call; unheld blocked items allowed; `check_back_max_s` 72 h,
sejd `idle_s` 1800, `launch_s` 15 min; `holder_overloaded` a warning only,
with its threshold a board setting (`max_active_per_holder`, default 2:
lane-job holders legitimately hold two); `job` requires `eta` on strict
boards. The wire contract is [items-board.md](items-board.md) §2 rule 11
and §4; this doc keeps the reasoning.
Owner's ruling (relayed by EP, 2026-10-23 ~00:45Z): a board holds only work
that is running or blocked. EP's correction, sent at 00:43Z, scopes this to a
per-board opt-in: sejd first, and every other board keeps today's contract
until Swarm-Coord decides on a global migration. Intake:
`cm-initiative/shared/ep-interviews.md` § 2026-10-23 (Swarm-Gardener's PT
worktree). The current contract is [items-board.md](items-board.md); section
numbers like "§4" refer to it.

## 0. What already exists (checked against main 557545e and live sejd)

Several of the asks are partly built already. The design reuses those parts
rather than adding parallel ones.

| Ask | Already there | Missing |
|---|---|---|
| ETA on an active item | `item_set(n, "active", eta=…)` (§2 rule 4) suppresses `holder_idle`, `stale`, `stale_background` and `check_back` until the ETA; `eta_passed` fires after it, then `stale` | a bare `eta` (no status) still selects `waiting`; no `job` field |
| Concrete blockers | `item_resolve(block)` already needs `blocked_by`, or `blocked_on` + `check_back` | `item_set(n, "blocked", blocked_on=…)` without a check-back only warns; bare `blocked` only warns |
| Checkback due | the `check_back` flag (detail `{check_back_at}`), pushed to the orchestrator | nothing: it never changes the status |
| 30 min holder idle | `holder_idle`, per-board `idle_s` (1200), `holder_idle_enabled` (off by default). **sejd already has it on** (`holder_idle_enabled: true`, `idle_s: 1200`), and it has fired 6 times in the 50-event history window | sejd's `idle_s` set to 1800 |
| Not while a job runs | `holder_idle` is skipped while a holder reports background work (`working-background`, or a background job with `wakes_agent`) and inside an active item's ETA window | none |
| holder_gone | 90 s grace, applies even while blocked; verified live in §6 | none |
| Dedupe | change-only pushes per audience (`items.flag_pushed` fingerprints, 2026-10-20) | none; new kinds reuse it |
| Free capacity | `free_capacity` in `board()` and on the Alt+B banner | it lists bash panes, the orchestrator and the steward; it has no unlaunched count beside it |

sejd snapshot at 2026-10-23T00:46Z (read-only `GET /boards/sejd`): 99 open
items, of them 47 active (8 with an ETA), 50 blocked (26 by items, 34 on text
with a check-back, 0 with no trigger) and 2 open. **No `waiting` items are
left**: Board-Steward's board-law sweep (00:39Z) has already moved them. One
blocked item has no holder. Seven sessions hold more than one active item
(Prod-Reads holds 14). There are 16 `free_capacity` rows, 5 of them bash panes.

## 1. The setting

One new board setting, `backlog_allowed` (bool, default `true`). `false` makes
the board strict. It is set with `PATCH /boards/{ref}` by Owner or the board's
current orchestrator (the same gate as `steward_pid`), and it is recorded as a
`board_updated` event. The `board` MCP tool gains `settings=` (the daemon's
`board.settings` RPC; `board.read` stays read-only) so EP can flip it without
API access. Every rule below applies
only when `backlog_allowed` is false. Other boards are byte-for-byte
unchanged, apart from the free-capacity cleanup in §4, which is shared.

The strict board holds four open states: `active`, `blocked`,
`blocked_on_owner` and (legacy, flagged) anything else. History states stay
`done` and `dropped`.

## 2. Write rules on a strict board

### 2.1 Refuse or map/flag?

| Option | For | Against |
|---|---|---|
| **Refuse** every non-conforming write | The board is truthful by construction, and the error names the fix while the writer is still holding the context | Refusals land on end-of-life paths too: a worker handing an item back as it exits, `report_done`'s hint, a session that doesn't know the board is strict. A refused handback leaves a dead holder on the item, which is a worse lie. It also teaches agents to invent triggers (`blocked_on="x"`, `check_back="30d"`) to get past the error, which is Goodhart |
| **Map or flag** everything | Writes never fail, and the orchestrator sees each problem | The board fills with flagged junk again, the exact flag load EP asked to cut. Silently mapping `waiting` to `active` hides the "nobody is on it" case that `waiting` was being used for |

**Proposal: refuse new *intent* and flag *releases*.** When the writer is
declaring what the item is (its status, its blocker, a new item), CM refuses
and names the conforming call in the same error, so the fix costs one retry
while the context is still live. When the writer is giving up responsibility
(handing an item back, a holder exiting), CM accepts the write and flags it.
Refusing a release would only keep a stale holder on the item. CM never
rewrites a stored status on its own.

### 2.2 The rules

| Write | Today | Strict board |
|---|---|---|
| `status="waiting"` | needs `eta` | `422 waiting_not_allowed`: "board sejd holds only running or blocked work. A job in flight: `item_set(n, "active", eta="40m", job="…")`. Waiting on something: `blocked_by=[m]`, or `blocked_on=… + check_back=…`" |
| `eta` with no explicit status | selects `waiting` | keeps `active` (the item must be `active`; any other status → `422 invalid_field`, as today) |
| `status="open"`, or a new item created with `holder="none"` | allowed | `422 holder_required`: "give it a holder, launch it with `item_resolve(n, "launch")`, or file future work as a planning task (`propose_task`)". Backlog belongs in planning, not on the board |
| `holder="none"` on an `active` item (a handback) | becomes `open` | accepted. It stays **`active`, unheld** with a warning, and it is `unlaunched` after `launch_s` (§3.2). An unheld item is visible at once in the header (§4) |
| `blocked` with neither `blocked_by` nor `blocked_on` | warning | `422 trigger_required` |
| `blocked_on` without `check_back` | allowed (no exemption) | `422 check_back_required` (the code `item_resolve(block)` already uses) |
| `check_back` further out than `check_back_max_s` (new, default 72 h on strict boards) | allowed | `422 check_back_too_far`. A dated check-back three weeks out is a parking lot, so the cap keeps it a real next look. The value is EP's call |
| `blocked_on_owner` | needs the decision in `blocked_on` | unchanged; no check-back needed. Owner is the trigger |
| Unheld `blocked` (by items, or on text with a check-back) | allowed | **allowed** (see below) |

**Why unheld blocked items are allowed.** EP's ask lists "unheld" as refused
or flagged. A blocked item already names its trigger, though, and each
trigger already reaches someone. When the blocker closes, the item
auto-unblocks and the `unlaunched` timer (§3.2) starts. When the check-back
passes, `check_back` reaches the orchestrator. Forcing a holder onto every
queued item would make one session hold many (Prod-Reads already holds 14
active items) and would set off `holder_overloaded` on work nobody has
started. **Open question for EP:** accept this, or require a holder on
blocked items too.

`report_done`'s `held_items_hint` on a strict board reads "close each item,
block it with its trigger, or hand it back (it stays active and the
orchestrator is told it needs a launch)".

### 2.3 `job`

There is a new item field, `job` (str ≤ 200): the identity of the job in
flight, such as `cm-gate pins L3 pid 41231 on cm-sessions`, `cell C2-07` or a
release lane. It follows the ETA's rules: only an `active` item carries it,
leaving `active` clears it, and passing `job` with another explicit status is
`422 invalid_field`. It exists on every board, but strict boards are where it
matters. **On a strict board `job` requires `eta`**. Together they declare
"running until about T, and this is what to check". A `job` with no ETA would
quiet nothing and could never expire.

`board(view="slim")` shows the holder's session state and the job side by
side, never merged:
`#1216 active "pins gate" @release-4[idle 12m] job: cm-gate pins L3 (eta 25m)`.

### 2.4 Keeping the four meanings apart

- **Checkback due is not blocker resolved.** `check_back` (shown as
  "check-back due") never changes the status, never starts the `unlaunched`
  clock and never counts as an unblock. Someone has to decide: re-block with a
  new check-back, activate, or drop. A `blocked_by` blocker closing is a
  resolution. It auto-unblocks (rule 5) and starts the launch clock.
- **Session idle is not job running.** The holder's session state stays
  exactly what the heartbeat says. The job is the item's declared evidence. An
  idle session inside its job's ETA window gets no `holder_idle`. Past the
  ETA, `eta_passed` reaches the holder first and `stale` follows after
  `ETA_STALE_GRACE_S`, as today. CM does not claim the job is alive: it cannot
  see a gate on another box. The ETA is the declared bound. A later slice
  could let the holder's daemon confirm a declared pid on its own host (§7,
  S5).

### 2.5 Migration and `needs_triage`

Flipping `backlog_allowed` to false never rewrites an item. On its next pass
(≤ 30 s) the engine raises **`needs_triage`** on every open item that doesn't
conform. Detail `{reason}` is one of the following, with its fix:

| `reason` | Fix |
|---|---|
| `waiting` | `active` + `eta` + `job`, or `blocked` |
| `open` | give it a holder, or drop it and file a planning task |
| `no_trigger` (blocked with neither) | add `blocked_by`, or `blocked_on` + `check_back` |
| `no_check_back` | add a `check_back` |
| `job_without_eta` | add an `eta` |
| `check_back_too_far` | move the `check_back` closer |

The flag clears when the item conforms or closes. `needs_triage` reaches
**only the orchestrator**: one push lists them all (§5's change-only rule, so
there is one wake and no repeats while nothing changes), and it is **never
escalated to Owner**. A migration backlog is the orchestrator's sweep, not an
Owner page. The board header shows `needs_triage: N`. Writes to a flagged item
still succeed, so its holder can keep adding notes, with a warning naming the
reason. Only *entering* a non-conforming state is refused (§2.2). Flipping
the setting back to true clears every `needs_triage` flag.

On today's sejd this flags 2 items (the two `open` ones). The waiting items
are already gone.

## 3. Alerts to the orchestrator

### 3.1 Holder idle, 30 minutes

There is no new flag. The existing `holder_idle` already meets the ask: every
holder of an `active` item idle for `idle_s`, with no holder reporting
background work, and outside an ETA window. It goes to the orchestrator in the
`board` push, then to Owner after `escalate_s` if the orchestrator is
idle or gone. The `holder_idle_enabled` default stays **off**, and strict mode
doesn't change it. For sejd the change is one setting: `PATCH /boards/sejd
{"idle_s": 1800}` (it is already enabled). Dedupe is the existing change-only
push: one push per episode, and the next only after the item changes.

### 3.2 `unlaunched`, 15 minutes (new kind, strict boards only)

New setting: `launch_s` (default 900). An item is `unlaunched` when **all** of
these hold:

- its status is `active` (so a blocked item never qualifies);
- **no holder is live and on it**: it is unheld, or no holder has touched it
  (a write or a holder's `#N` chat post) since it became actionable and no
  holder is `working` / `working-background` now. A holder in state `unknown`
  counts as live, because `unknown` is never proof of absence (§3);
- `now − actionable_at ≥ launch_s`, where `actionable_at` = max(created_at,
  the auto-unblock `clock_reset_at`, the handback time, the time it last
  entered `active`);
- `holder_gone` is not open on it. Gone is the louder flag, and once the item
  is handed back its handback starts the `unlaunched` clock.

`check_back` passing does **not** make an item actionable (§2.4). On the
non-strict contract the closest thing is `unassigned` (`open`, no holder,
5 min). On a strict board no new `open` items exist, so `unassigned` only
appears through `needs_triage`.

Delivery: an `ENGINE_KINDS` flag (raised and cleared by the engine) that goes
straight to the orchestrator in the next `board` push (not holder-first: if the
holder is absent there is nobody else to ask). Its detail is
`{actionable_at}`, which stays stable so it fingerprints once per episode, and
any new activity starts a new one. It is never escalated to Owner: it is the
orchestrator's to act on, and only an orchestrator escalates (review of
94eedeb). Inside a declared job's ETA it is not raised, and the holder's own
write (claiming the item, declaring the job) counts as the holder on it
(`launched_at`).
Push text: `#1496 unlaunched (no live holder 18m; free: Batch-Valid, P1-Ingest)`.
The text names up to three idle free sessions (§4) so the orchestrator can
`item_resolve(n, "launch")` or `"reassign"` in one call.

**The 6 h G1 gap** had no item at all, so neither `holder_gone` nor
`unlaunched` could have fired. That gap is covered by process (a lead rule and
the Board-Steward sweep). §4's `untracked` list helps that sweep: it lists
sessions working on the board with no item.

## 4. Capacity

### 4.1 `holder_overloaded` (warning; strict boards, or when `max_active_per_holder` is set)

New setting: `max_active_per_holder` (default 2, read only on strict boards). A session is overloaded when it holds more than that many
`active` items **outside a job's ETA window**. An item with a declared job in
flight doesn't need its holder's attention, so it doesn't count. The
orchestrator and the steward are exempt, since they hold coordination items.

The flag is raised on each of that holder's counted items **except** the
`max_active_per_holder` it touched most recently, which are its apparent
focus. Detail is `{holders}`
only, so a change of focus doesn't re-fingerprint every sibling. The holder
gets one push per episode ("you hold 5 active items; block or hand back the
ones you're not on"). The orchestrator gets the flags in its `board` push.
Owner never gets them, since this is a warning and not a stall. CM never
reassigns anything.

With the default of 2, today's sejd would warn on Prod-Reads (12 extras) and
PointPF-Capacity (3) and P1-Integrator (2), fewer once ETA windows count.

### 4.2 Free capacity next to unlaunched

`board()` and the Alt+B banner gain a `launch` block:

```
launch: 2 unlaunched (#1496 18m, #1501 4m) · free: Batch-Valid idle, P1-Ingest idle, rl-scale-out idle
        untracked: HS-Cond working, RL-Actor-AOT working
```

`free_capacity` is fixed on **every** board. This is the only shared change,
and it is display-only. It drops bash panes, the orchestrator and the steward,
and sorts `idle` sessions first. Sessions that are `working` but hold no item
are moved into a new `untracked` list: they aren't free, they're doing work the
board can't see, which is the G1 pattern. The existing `free_capacity` key
keeps its list-of-rows shape with `state` on each row. `untracked` and
`launch: {unlaunched: [{n, title, since}], free: [...]}` are new keys that
older consumers ignore. The TUI banner gets a third line on strict boards.

## 5. Policy text alignment

Today, the deployed `~/.cm/policies/orchestration.md` §4 says that a long job
"is declared as waiting, with an ETA". AGENT_GUIDE says the same (two places).
items-board.md §2 rule 1 makes a bare `eta` select `waiting`. Exact changes:

**doc/ORCHESTRATION.md §3**, the bullet

> Workers set their own status: `done`, `waiting` with `eta`, `blocked`.

becomes

> Workers set their own status: `done`, `blocked` (with `blocked_by`, or
> `blocked_on` + `check_back`), and for a long job `active` with `eta` and
> `job` (§4).

**doc/ORCHESTRATION.md §4**, first paragraph, becomes

> A job you expect to run longer than 20 minutes (a build, a benchmark, a
> backtest, a long test suite) is declared on its item, with an ETA and what
> to check: `item_set(n, "active", eta="40m", job="full daemon suite, pid
> 41231")`. The item stays active, because someone's job is running; its holder
> may sit idle meanwhile without being flagged, and at the ETA it is asked
> (`eta_passed`). On a board that still allows backlog
> (`backlog_allowed`, the default) `item_set(n, "waiting", eta=…)` remains for
> work parked with nobody on it; a strict board (sejd) refuses `waiting`,
> `open` and unheld new items: there, a job in flight is active+eta+job,
> anything else that cannot proceed is blocked with a concrete trigger, and
> future work is a planning task. For continuous subtasks also set
> `metadata.continuous_stage="waiting"` with `next_action` (that is the
> task's stage, not the item's status). A channel post is optional. An
> orchestrator does not guess activity from a spinner, a shell or a stale
> monitor; it trusts the declaration and follows up when the ETA passes.

**doc/ORCHESTRATION.md §3 flag table**: change `holder_idle`'s row to "every
holder sat at its prompt for `idle_s` (20 min by default; sejd 30 min). Off by
default and enabled per board." Add three rows: `unlaunched` ("active, no live
holder on it for 15 min: launch or reassign; `board()` lists who is free"),
`needs_triage` ("doesn't fit a strict board: make it active+eta+job, blocked
with a trigger, or drop it") and `holder_overloaded` ("a session holds more
than one active item outside a job; block or hand back the extras"). Change
`check_back`'s meaning to "check-back due: decide (re-block, activate, drop);
not a sign the blocker cleared".

**mcp_server/AGENT_GUIDE.md**: in the "Running a job longer than about 20
minutes" bullet, lead with `item_set(14, "active", eta="40m", job="full C2
run")` and keep `waiting` as "boards that allow backlog: parked, nobody on
it". In the orchestrator core norms, change "Declare a job longer than 20
minutes on its item: `item_set(n, "waiting", eta=..., note=...)`" to "…:
`item_set(n, "active", eta=..., job=...)`". The guide is supplied to every
agent on every board, and active+eta+job is valid on every board, so the
guide can teach the one form that works everywhere.

**doc/items-board.md**, changes:
- §1: add `job` to the table; add a "Strict boards" paragraph pointing here.
- §2 rule 1: append "On a strict board (`backlog_allowed: false`) an `eta`
  keeps the item `active`, and a handback leaves it `active` and unheld."
- §2 rule 4: append the `job` rules (§2.3).
- New §2 rule 11: the strict-board table from §2.2.
- §4 settings table: `backlog_allowed`, `launch_s`, `max_active_per_holder`,
  `check_back_max_s`.
- §4 kinds table: `needs_triage`, `unlaunched`, `holder_overloaded`.
- §4 free capacity: the redefinition in §4.2.
- §6 error codes: `waiting_not_allowed`, `holder_required`,
  `trigger_required`, `check_back_too_far`.

The policy docs change in the same merge as the code, so the guide never
describes a call that the deployed API refuses. They are deployed with
`scripts/release-continuous-docs.py`.

## 6. holder_gone on sejd: verified, read-only

Evidence came from the sejd event history (`GET /boards/sejd?history=50`,
2026-10-23T00:46Z) joined with the session tombstones in cm-sessions'
`~/.cm/daemon-tombstones.json`. No board write and no test was performed.

| Holder session | Exited (tombstone) | `holder_gone` raised | Latency | Items |
|---|---|---|---|---|
| PointPF-Partner (`ts-18dc97cf…`, killed by EP) | 2026-10-17 21:30:07.6Z | 21:31:45.9Z | **98 s** | #801 |
| Liquidation-RFR-Codex (`ts-18df7143…`) | 2026-10-21 21:54:13.0Z | 21:55:50.4Z | **97 s** | #1011 |
| Latent-Joint-Design-Resume (`ts-18df848e…`) | 2026-10-21 21:54:13.3Z | 21:55:50.4Z | **97 s** | #1160 #1163 #1178 #1181 #1193 #1196 #1304 #1323 |

The latency is the 90 s `GONE_GRACE_S` plus the wait for the next 30 s engine
pass, so the worst case is about 2 min. That matches the contract ("exited
≥ 90 s ago"). The flags reached the orchestrator. EP acted on #1304 and #1323
at 21:56:30Z and 21:56:32Z (about 40 s after the raise), with reasons naming
the exit, and Board-Steward acted on #1011 at 22:05Z. It applies while
blocked: the Latent-Joint items were `blocked` behind #1304. All 10 raises in the window
resolved `cleared` once the item was handed back or reassigned.

Two observations, neither a holder_gone defect:

1. After a handback the item has no holder, so `holder_gone` correctly stops.
   Seven of those items then sat unheld and `blocked` behind #1304 for 15 h
   with no flag. They were exempt, which is correct under today's contract,
   because #1304 was held. On a strict board they stay allowed (§2.2), and
   `unlaunched` catches them when #1304 closes.
2. The history window is the last 50 events per item, so older exits aren't
   visible here. There are no live tests to run. If EP wants a deliberate
   check anyway, the safe form is a scratch board (`task-<id>` of a throwaway
   task) with one item held by a throwaway `bash` session that is then
   killed, which needs no sejd write. I won't run it without Swarm-Coord's go.

## 7. Slices (after the go)

| Slice | Content | Size |
|---|---|---|
| S1 | `sql/024`: `boards.backlog_allowed`, `launch_s`, `max_active_per_holder`, `check_back_max_s`; `items.job`. Write rules §2.2–2.3 in `items_rules.py`; settings in `patch_board`; MCP `job=` on `item` / `item_set`; `board(settings=)` through the daemon. Tests per row of §2.2 | ~1 day |
| S2 | Engine: `needs_triage`, `unlaunched`, `holder_overloaded` in `compute_flags`; the delivery rules (needs_triage to the orchestrator only, not escalated; holder push for overloaded); dedupe tests (no repeat push while unchanged, one per episode) | ~1 day |
| S3 | `free_capacity` cleanup, the `untracked` and `launch` blocks, slim-view `job`, the Alt+B banner line | ½ day |
| S4 | Policy text (§5), in the same merge as S1–S3 | ½ day |
| S5 (optional, later) | The holder's daemon confirms a declared job pid on its host, so `job` becomes evidence and not only a label | separate design |

Rollout on sejd after a deploy: EP sets `backlog_allowed: false`,
`idle_s: 1800` and `launch_s: 900`, the orchestrator clears `needs_triage`,
then `max_active_per_holder` is set. No other board changes.

## 8. Questions for EP

1. Refuse intent and flag releases (§2.1): agreed?
2. Unheld blocked items allowed (§2.2): agreed, or require a holder?
3. `check_back_max_s` 72 h, and `max_active_per_holder` 1 or 2 for sejd?
4. Should `job` require `eta` on strict boards (§2.3)?
