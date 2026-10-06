# Design brief: keeping track of swarm work

Status: **draft for Owner review**, 2026-10-06. Nothing here has been approved or built.
Sources: Owner's dictated notes (2026-10-06) and EP's handoff
`~/.local/share/ep-owner-calls/CM-HANDOFF-EP-DIFFICULTIES-20261006.md`.

## 1. The problem

Owner runs initiatives as two levels: an **orchestrator** (e.g. EP, about 20
lanes in #gpu-utilization) that Owner mostly talks to, and **sessions** under it
(Codex sessions as strong individual contributors, Claude sessions as small
teams with native subagents). Work keeps getting lost between them:

- The orchestrator says it is starting something; it never starts, or stops
  halfway, and nobody notices until Owner checks again.
- Planning tasks are too heavy for this. The real units are small ("implement
  this speedup"), created mid-work, often several per session, and sessions are
  reused across them because they have good context.
- The orchestrator's view is pull-only: to learn status it messages each agent
  and waits. EP could not even see whether some channel members were idle.
- The only shared picture is a markdown table EP maintains by hand. It helped,
  but Owner cannot see it live and it goes stale as soon as EP gets busy.
- Owner's direct line (`notify_user`) is underused, and agents have no way to
  know whether Owner is asleep, half-available or actively unblocking.
- Several failures came from not knowing what wakes whom: the 6.5 h silent wake
  outage, `@Name` in a body not notifying, an inbox that shows September first.

## 2. Goals

1. **One live list** of the work in flight, per initiative, that Owner and the
   orchestrator both see at any time, showing who is on each item.
2. **Starting an item costs almost nothing.** One short call or line, no
   description, review, approval, worktree or new session.
3. **Dropped work becomes visible on its own.** An item nobody holds, or whose
   holder is idle, gone or silent, is flagged without anyone polling.
4. **Agents keep it current themselves**, with the system filling in whatever it
   already knows (session state, exit, `report_done`).
5. **Owner availability is explicit**, visible to agents, and changes how much
   they surface to Owner.
6. **Push versus check is a deliberate rule** for both Owner and agents.

Non-goals: replacing planning tasks, initiatives or the message board;
approval workflows for items; time tracking; GPU/VM spend ledgers (EP B7, a
separate idea).

## 3. Work items ("mini-tasks")

Working name **item**. Name is open (§8).

### What an item is

| Field | Required | Notes |
|---|---|---|
| title | yes | One line, e.g. "fuse SEJD hot operators" |
| holders | defaults to the caller | One or more sessions; may be empty (= unassigned) |
| status | defaults to `active` | `open` (nobody on it yet), `active`, `waiting` (long job running, with ETA), `blocked`, `done`, `dropped` |
| note | no | One line, latest wins: "waiting on JP PASS", "PR-ready at abc123" |
| group | no | Free text heading, e.g. "RL training" / "Inference", mirrors EP's goal file |
| blocked_by | no | Other items this one waits on (must be real, open items; see "Blocking on other items") |
| blocked_on | no | Free text for anything that is not an item: "EP GO", "JP review" |
| links | no | Planning task, branch/SHA, message, file |

Items live on a **board**. By default the board is the caller's initiative
(else its top-level task), so EP and its lanes share one board without
configuration. Items are not planning tasks and never appear in the planning
backlog; an item can link to one, and an item can be promoted to a task later
if it grows.

### Creating and updating

The whole point is near-zero overhead:

```
item("fuse SEJD hot operators")                     → mine, active
item(["speedup A", "speedup B", "speedup C"])       → three at once
item("rerun C2 r4", holder="rl-scale-out")          → orchestrator assigns
item_set(14, "done")                                → one word
item_set(14, "blocked", note="needs EP GO for GPU")
item_set(15, blocked_by=[14])                       → waits on item 14
item_set(14, "waiting", eta="40m", note="full C2 run")  → long job running
```

Holders are named by chat name or session uid. Reassigning or adding a holder
is the same call. A session that reports `done` on its last active item gets
nothing extra; the orchestrator is told (§5).

To keep agents from forgetting, the agent guide and the orchestration skill
(§7) state the norm "if you are doing something, it has an item", and the turn
hook can remind a session whose turn ended while it holds no items but has
been given work. Exact nudges are an implementation choice.

### What the system fills in

Each item shows its holders' **live state** without anyone reporting it:
working / idle (and for how long) / awaiting input / reported done / exited,
plus time since the item was last touched. From that the board raises flags:

- **unassigned**: `open` with no holder for more than a few minutes.
- **holder gone**: a holder session exited or was killed.
- **holder idle**: every holder of an `active` item has been sitting at its
  prompt (turn ended, nothing generating) for more than 20 min. The daemon
  already tracks this per session. Catches "said it would start, never did" and
  "stopped halfway".
- **stale**: nobody has touched the item (status, note, holder) for more than
  2 h while it is `active` or `blocked`, even if its holder is busy. Catches a
  session that has drifted onto other work, or a `blocked` item nobody is
  chasing.
- **free capacity**: a session on the board holds no active item (useful for
  the orchestrator, not an error).

These derived signals are what replaces polling. They work across hosts:
sessions on `sessions`, `manager` and local all report their state to the
board.

### Long-running jobs

A session that starts something long (a test suite, a backtest, a training
script) and then sits at its prompt is not idle in any useful sense, but the
engine signals cannot reliably tell a live background job from a stale monitor
or shell left over from earlier. So the holder **declares it**: set the item to
`waiting` with an ETA and a one-line note of what is running.

- While `waiting` and before its ETA (plus a grace of about 25%), the item is
  exempt from the idle flag. Stale still applies on the normal 2 h clock unless
  the ETA is longer.
- When the ETA passes and the item is still `waiting`, it is flagged
  **overdue**: the holder is nudged first, then the orchestrator.
- The holder's own wake-up on job completion is unchanged (its monitor or
  background shell). Setting `waiting` is the norm whenever a job is expected
  to run longer than the idle threshold, stated in the agent guide and the
  orchestration skill.
- Whether an engine signal can corroborate or auto-fill this (for example a
  known-live background task) is part of the idle-detection research.

### Blocking on other items

An item can be blocked by one or more other items (`blocked_by`). While every
item it waits on is still open, it is **exempt from the idle and stale flags**:
its holder is legitimately waiting, and the orchestrator is not bothered about
it. The rules keep this from becoming a hiding place:

- **Only real items.** `blocked_by` must name existing items that are not
  `done` or `dropped`. Waiting on anything else (an EP GO, a JP review, Owner)
  uses free-text `blocked_on`, which gets **no exemption**: it goes stale like
  any other item, so someone keeps chasing it.
- **No cycles.** Setting `blocked_by` is refused if it would create a cycle.
- **Why that is enough.** Every chain of waiting items therefore ends at an item
  that is not waiting on another item, and that item is still subject to the
  idle and stale flags. If the end of the chain stops, it is flagged, so a
  stopped piece of work can never hide behind a dependency.
- **When a blocker closes.** If it is `done`, it is removed from `blocked_by`;
  once nothing is left the item returns to `active`, its holder is woken, and the
  idle/stale clocks restart from that moment. If a blocker is `dropped`, the
  waiting item is flagged for the orchestrator to decide (reassess, drop, or
  re-point it), since its premise may be gone.
- **Holder gone still applies.** A waiting item whose holder exits is flagged
  as usual; the exemption covers waiting, not abandonment.

### Closing items out

The board should only ever show live work. Nothing is allowed to sit there
quietly.

- **Done.** The holder marks it `done` (one word, optional note such as a SHA).
  It moves to a "recently closed" strip for 24 h, where the orchestrator and
  Owner can glance at it or reopen it, and is then archived automatically.
  Archived items stay searchable but are off the board.
- **Cancelled.** Anyone marks it `dropped`, ideally with a one-line reason
  ("superseded by 14", "Owner ruled no"). Same strip, same archiving.
- **Holder exits.** When a session exits or is killed (including by
  `mark_subtask_done`), its active items are flagged **holder gone** immediately.
- **Session says it is finished.** `report_done` from a session that still holds
  active items returns those items and asks it to close or hand them back.

**Every flag must be resolved by the orchestrator.** A flag (unassigned, holder
gone, holder idle, stale) wakes the orchestrator and stays on the board until
it takes one of these actions, each one call:

| Action | Effect |
|---|---|
| **nudge** | Sends the holder a standard "item N looks idle/stale: update, close or hand back" prompt and clears the flag. |
| **reassign / launch** | Gives it to another session, or starts a new session with the item as its prompt. |
| **block** | Marks it blocked by another item (exempt while that item is open) or on something concrete in free text with a check-back time (reflags then if still blocked). |
| **drop** | Cancels it with a reason. |

A flag the orchestrator leaves unresolved re-wakes it after a while, and if the
orchestrator itself stays idle with unresolved flags it is surfaced to Owner
per the availability level (§4). The orchestrator losing track is the very
failure this is meant to catch, so it needs that backstop. The board header
shows the count and oldest age of unresolved flags, which is the health number
Owner can watch.

### Where it shows

- **Owner:** a board pane in the TUI (likely a tab beside the channel view or
  under the initiative's sidebar section), grouped by `group`, flags first.
  Owner can also edit items there (reassign, drop, add).
- **Orchestrator:** `board()` returns the same table compactly in one call,
  slim by default, flags first. This also answers EP's B2 "fleet dashboard":
  every holder's state, idle time and current item in one place, without global
  session permissions (reading state is not controlling).
- **Everyone else:** the same `board()` call, so a lane can see who to wait on.

## 4. Owner availability

A single setting Owner changes from the TUI (one key, also a CLI command for
phone/SSH). Agents read it in `ping`/`chat_open` and in `board()`, including
when it last changed.

| Level | Meaning | What reaches Owner |
|---|---|---|
| **away** | Asleep or not working | Nothing interrupts. Requests queue and are shown as a batch when Owner comes back. Only genuine emergencies (live trading harm, data loss, runaway spend) break through. |
| **around** | Cooking, reading, Discord; laptop nearby | Only things that are both important and blocking significant work. |
| **focused** | Working, but on one thing | Decisions that block work. Batch the rest. |
| **on-call** | Owner's job right now is unblocking agents | Anything where Owner's call would help, promptly. |

Enforcement should not rely only on agents remembering norms. `notify_user`
gains an urgency (`fyi` / `decision` / `blocking` / `emergency`, names open) and
the daemon holds alerts below the current level's bar, releasing them as a
digest when Owner raises the level. The agent always gets told "queued until
Owner is available" versus "delivered", so it can keep working on something
else.

When the level changes, **orchestrators are woken** (and any session with a
queued Owner request). Other sessions see it on their next call; waking twenty
lanes because Owner went to cook is itself noise.

## 5. Push versus check

The rule: **wake someone only when they must act, and make everything else
cheap to look up.**

| Event | Orchestrator | Holder | Owner |
|---|---|---|---|
| Item created, note changed | check | check | check |
| Item done / dropped | **push** (batched) | — | check |
| Item blocked on X | **push** | — | check; push only if X is Owner and level allows |
| Flag: unassigned / holder gone / holder idle / stale | **push** until resolved | via orchestrator's nudge | check |
| Flags left unresolved while orchestrator is idle | re-push | — | **push** per level |
| Assigned to me | — | **push** | — |
| Owner level changed | **push** | check | — |
| Owner request (`notify_user`) | — | — | **push** per level (§4) |
| Wake delivery stalled > 15 min | — | — | **push** (already shipped, c097938) |

Pushes use the existing native notification path. Done/blocked pushes to the
orchestrator are batched (one wake per few minutes) so twenty lanes finishing
do not mean twenty wakes. The board itself is the "check" surface for every
row.

## 5a. Reliable session state (research, 2026-10-06)

The idle and holder-gone flags are only as good as CM's idea of whether a
session is working. Two research reports
(`~/.local/share/swarm-coord/idle-research/claude-code.md` and `codex.md`)
found that today's detection is mainly "PTY quiet for 2 s", with turn-end
hooks bolted on and read inconsistently. Confirmed failures:

- **False busy:** Claude repaints while background agents/shells run; Codex
  status-line repaints; an operator scrolling.
- **False idle:** permission/approval prompts and AskUserQuestion; Codex
  thinking quietly; a detached Codex viewer.
- **Stuck "mid-turn":** Esc, API/rate-limit/auth errors (no Stop hook fires);
  any viewer keystroke, draft or mouse event counts as new input.
- **Invisible turns:** channel and task-completion wakes CM never sees start.

Both engines now expose real state that CM does not read:

- **Claude Code 2.1.291:** a per-process status file
  `~/.claude/sessions/<pid>.json` (`busy` / `idle` / `waiting` with reason /
  `shell` = only background shells or monitors live, plus last-change time),
  and hooks CM does not install yet: UserPromptSubmit (every turn start),
  StopFailure (error turns), PermissionRequest; the Stop hook already lists
  live background tasks and scheduled wake-ups. The status file is
  undocumented, so unreadable means *unknown*, never idle.
- **Codex 0.160.1 app-server:** `thread/status/changed` (`idle` / `active` /
  `systemError`, flags `waitingOnApproval` / `waitingOnUserInput`),
  `turn/completed` with completed / interrupted / failed, retry-aware errors,
  subagents as threads, and `thread/backgroundTerminals/list` with PID, CPU
  and command per background job (experimental). CM's rollout parsers are
  pinned to 0.153.4 and reject newer record types.

**Direction:** one engine-reported state per session, published everywhere
(TUI, `list_sessions`, monitors, board): `working`, `working-background`,
`waiting-on-human`, `errored`, `idle` (with `idle_since`), `starting`,
`exited`, `unknown`. Screen quietness stays only as a fallback for old
sessions and for timing prompt delivery. Viewer keystrokes stop resetting
idle. The board's idle flag counts only true `idle`; `waiting-on-human` and
`errored` get their own flags, and `unknown` is shown, never treated as idle.

**Long jobs:** both engines can say which background jobs are live; neither
can say whether they still matter, and Codex does not wake the agent when its
background job ends. So the declared `waiting` + ETA stays primary (§3), with
engine data used to pre-fill it, to flag "declared waiting but nothing is
running", "job finished but agent idle", and long undeclared background work.
CM can also wake a Codex agent itself when its listed job disappears. Jobs
started outside the engine (nohup, tmux, cloud VMs) still need the
declaration.

## 6. Small messaging fixes that belong with this

From EP's bug list, still open as far as we know. Each is small and independent.

- **`@Name` in a body that matches a participant becomes a mention**, or at
  least the send returns a warning. (EP A5/B6; cost EP hours.)
- **Inbox reads newest-first by default**, with bulk "mark read before T".
  (EP A4/B5.)
- **Authoritative send time** shown back to the sender. (EP A10.)
- **`read_last_turn` empty for Codex** sessions. (EP A8.)
- **Clear action for `context_status.stale_scopes`** or stop reporting it on
  every call. (EP A9.)
- Confirm the shipped stall alarm (c097938) and slim reads covered EP A1–A3, A6.

## 7. Orchestration guide (skill)

A short skill for orchestrators, written alongside the board:

- Codex sessions: smart individual contributors for hard, technical, low-volume
  work; little internal parallelism. Claude sessions: a lead with up to about
  six native subagents for broader or higher-volume work.
- Reuse sessions that hold good context; give them new items rather than new
  sessions.
- Every piece of work in flight is an item with a holder; check the board's
  flags instead of DMing lanes for status.
- Use `mentions` (or the fixed `@Name`) for anything that must wake someone.
- Owner requests go through `notify_user` with an honest urgency; logs and
  questions stay separate (EP's B9 lesson).

## 8. Owner decisions (2026-10-06)

1. **Name:** item.
2. **Board scope:** one board per initiative, with `group` for goals.
3. **Availability:** the four levels as written; `away` keeps the emergency
   breakthrough.
4. **Editing:** anyone on the board may edit any item; history is kept.
5. **Thresholds:** idle 20 min, stale 2 h (measured as in §3), adjustable per
   board.
6. **Charter:** no change needed. The "notification delivery excluded" remark
   came from the predictionTrading proposal briefs, not this charter. The
   charter does exclude a first-class "workstream/swarm" object; items are
   small units of work, not workstreams, so they fit.
7. **Close-out:** done and cancelled items leave the board; idle and stale
   items must be acted on by the orchestrator, never left to accumulate (§3).
8. **Item dependencies:** an item may be blocked by other real, open items,
   which exempts it from idle/stale; no cycles, so every stopped chain still
   surfaces at its end (§3).
9. **Long jobs:** a declared `waiting` status with an ETA, rather than
   inferring activity from monitors or shells, which are often stale (§3).
10. **Idle-detection research is in** (§5a); the design call is Owner's next
   step.

Not for Owner: storage, cross-host replication, schemas and wire formats. The
implementer picks those (current leaning: store items in the planning API so
every host and the TUI see one copy, with each daemon pushing its holders'
session state).

## 9. Possible order of work

1. Messaging fixes (§6) and the orchestration skill (§7). Small, immediate.
2. Reliable session state (§5a): Claude status file + hooks, Codex app-server
   status; one published state. The board's flags depend on it.
3. Items and `board()` for agents, with derived holder state and flags.
4. TUI board pane for Owner, and orchestrator pushes (§5).
5. Owner availability and gated `notify_user`.

Each step is useful on its own; EP's #gpu-utilization swarm is the first real
test, measured by whether Owner still finds dropped work by checking manually.
