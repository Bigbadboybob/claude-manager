# Orchestrating sessions

Owner policy, 2026-10-06 (Swarm Focused Design initiative). This is the shared
guide for any session that runs other sessions: an initiative coordinator, a
continuous-task orchestrator, or a lead with a few workers. It applies in every
project. The deployed copy is `~/.cm/policies/orchestration.md`; the
`orchestrate-swarm` skill points here. Continuous tasks also follow
[continuous reviews and stages](continuous-review-routing.md), which is stricter
where the two differ.

Owner usually talks to the orchestrator and rarely to its workers. Most lost
work so far fell between the two: a lane that was "starting" never started, a
long job went silent, a mention in a message body never woke anyone, or an Owner
question was buried in a progress log. The rules below exist to close those gaps.

## 1. Pick the engine for the work

- **Codex** is a strong individual contributor: hard, technical, low-volume
  work with little internal parallelism (a tricky fix, a careful refactor, a
  performance investigation).
- **Claude** is a lead with up to about six native subagents: broader or
  higher-volume work (a sweep across many files, docs plus code, a review
  fan-out).

`start_session(type="codex" | "claude-code", isolated=true, ...)` launches
either one. `isolated=true` (or a `task_id` with its own worktree) keeps workers
out of each other's checkouts; share a checkout only when you mean to.

## 2. Reuse sessions that hold good context

A session that just finished a piece of work in an area knows that area. Give
it the next piece with `send_input(session_uid, "...")` instead of starting a
new session. Start a new session when the work is unrelated, needs a different
engine, or the old session's context is spent (repeated compactions, confused
replies). `send_input` auto-registers a completion monitor exactly like
`start_session`, so the reuse costs nothing in tracking.

## 3. Every piece of work in flight has a holder

Each unit of work in flight is an **item** on your board with exactly one named
holder, a title that says what "done" means, and, when useful, a one-line note
with the next action. "I'll start X" is not a state: either X is an item with a
holder now, or it is not in flight. The board is the list Owner reads without
asking you; check it (and `read_last_turn` on a worker whose state is unclear)
before you message a lane for status.

**Making and assigning work**

- `item("title", holder="lane-name", group="RL training")` creates and assigns
  in one call; the holder is woken (`assigned`). `item([...])` creates several.
- `item_set(n, holder=...)` reassigns, `add_holder` / `remove_holder` adjust.
  Workers set their own status: `done`, `waiting` with `eta`, `blocked`.
- Dependencies: `item_set(15, blocked_by=[14])`. Only real open items on the
  same board, no cycles. An item blocked only by open items is exempt from the
  idle and stale flags, but every chain ends at an item that is not, so a
  stopped chain always surfaces at its end. Waiting on anything outside the
  board uses free-text `blocked_on`, which is not exempt.

**What the board tells you**

`board()` (slim by default) shows each holder's live state, for example
`#14 active "fuse SEJD" [RL] @rl-scale-out[idle 24m] ⚑holder_idle`. The engine
raises these flags and pushes you once per window with every open flag:

| Flag | Meaning |
|---|---|
| `unassigned` | open with no holder for a few minutes |
| `holder_gone` | a holder's session exited |
| `holder_idle` | every holder sat at its prompt for 20 min (on per board) |
| `holder_waiting_on_human` / `holder_errored` | a holder needs a person, or failed |
| `stale` | nobody touched it for 2 h (longer for a long declared job) |
| `overdue` | waiting past its ETA plus a grace; the holder is asked first |
| `check_back` | a free-text block reached its check-back time |
| `blocker_dropped` | something it waited on was dropped; decide what it needs now |

Done, dropped and blocked items arrive batched (one wake per few minutes).

**Resolving flags (each one call)**

| `item_resolve(n, action, ...)` | Effect |
|---|---|
| `"nudge"`, `message=` | asks the holder to update, close or hand it back; snoozes the flag |
| `"reassign"`, `holder=` | gives it to another session |
| `"launch"`, `engine=`, `task_id=` | starts a new session with the item as its prompt and makes it the holder (your engine by default; `task_id` gives it that task's worktree, otherwise it joins your checkout) |
| `"block"`, `blocked_by=` or `blocked_on=` + `check_back=` | marks it legitimately waiting |
| `"drop"`, `reason=` | cancels it |

Every flag must be resolved. An unresolved flag re-wakes you every 30 minutes,
and if flags sit for an hour while you are idle, errored or gone, Owner is
alerted. The board header's `health` (unresolved count, oldest age) is the
number Owner watches.

## 4. Long jobs are declared, not inferred

A job you expect to run longer than 20 minutes (a build, a benchmark, a backtest,
a long test suite) is declared as waiting, with an ETA and a one-line note: set its item to
waiting, `item_set(n, "waiting", eta="40m", note="full daemon suite")`; for
continuous subtasks also set `metadata.continuous_stage="waiting"` with
`next_action`. A channel post is optional. An orchestrator does
not guess activity from a spinner, a shell or a stale monitor; it trusts the
declaration and follows up when the ETA passes.

## 5. Know what wakes whom

| Action | Wakes |
|---|---|
| DM (`chat_send(dm=...)`) | the recipient(s) |
| `chat_send(mentions=["<participant id or exact name>"])` | each mentioned participant |
| `chat_send(mention_here=True)` | current channel members |
| `start_session` / `send_input` (auto monitor) | you, when the worker's turn ends (`notify_until="final"`: when it calls `report_done` or exits) |
| `monitor_sessions(...)` | you, on the condition you set |
| `item(..., holder=X)` / `item_set(n, holder=X)` | X, when assigned |
| board flags and closures | the board's orchestrator, batched (one wake per few minutes; unresolved flags re-wake every 30 min) |
| `notify_user(message=..., urgency=...)` | Owner, through the TUI alert, if the urgency meets Owner's availability; otherwise held until it does (emergency also pushes to Owner's phone) |
| `@Name` in a message body | that participant, **only** if it names exactly one current member of the conversation; otherwise nobody, and the send returns a warning |
| Tags, channel membership, a pinned message | nobody |

Anything that must make someone act uses a DM or a mention. Read
`mentions_resolved` and `warnings` on every send: a warning means someone you
named was not notified (not a member, unknown or ambiguous name, `@here`,
`@Owner`). Use `chat_people(query=...)` and participant IDs when in doubt; IDs
survive renames.
After dispatching or prompting workers, end your turn and let the monitor or the
mention wake you; do not poll in a loop.

## 6. Owner requests are separate and honest

- Owner questions, results and logs are three different things. Keep progress
  logs in the channel, results in files or the handoff, and send Owner only
  genuinely open questions.
- An Owner question states the options, your recommendation and who is blocked
  until it is answered. One question per decision; batch independent questions
  in one request.
- Use `notify_user(message=..., urgency=...)` with a concise reason, a link to
  the message or file, and an honest urgency (`fyi`, `decision`, `blocking`,
  `emergency`) when Owner action is needed. Ask only when it really needs
  Owner; decisions inside the authorized task are yours to make.
- Check `ping().owner_availability.level` first: `away` (emergencies only),
  `around` (blocking and above), `focused` (decisions and above), `on-call`
  (everything); null means Owner has not set one. A request below the bar comes
  back `delivery="held"`: keep working on what you can; you are woken when it
  is released.
- Do not DM or mention Owner for visibility. The `needs-owner` tag marks a
  nonurgent item for later review and does not notify.

## 7. Read your inbox efficiently; never hand-stamp times

- On every wake: `chat_read(inbox=True, unread_only=True)` (newest first and
  slim by default), follow `next_cursor` with the same query, and acknowledge
  each page's receipt.
- After a long absence, act on the newest pages, then clear what has been
  superseded with `chat_read(inbox=True, mark_read_before="<RFC3339>")`.
- CM's `created_at` on every message is the authoritative time. Never write
  your own estimate of the time into a post; refer to the message's time or to
  an ETA you computed from it.

## 8. Recovering after compaction or a restart

Rebuild state in this order before acting:

1. Your own last posts in the coordination channel(s) (what you promised).
2. The replies to them and your unread inbox (what changed).
3. The board (`board()`, section 3), `list_sessions` for your workers, and
   `read_last_turn` on any worker whose state is unclear.

Then continue; do not repeat finished summaries or re-dispatch work that has a
live holder.

## 9. Messaging manners

Quick replies are fine; usual posts are one to three short paragraphs, with a
file reference for anything longer. Reply when you are acknowledging actionable
work or supplying a decision; avoid acknowledgement ping-pong. Messaging does
not grant merge, deploy or session-control permissions; those still come from
Owner or the task's authorization.
