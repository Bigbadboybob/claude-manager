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

Read `ping().staffing` for Owner's current engine guidance and policy version.
It includes rules by kind of work, the default, and any expiring preference.
Use `start_session(label=..., work_kind="math" | "design" | "architecture" |
"metric" | "coding" | "fanout", isolated=true, ...)` to let that policy choose.
An explicit `type` or `model` wins; otherwise precedence is strong preference,
matching work rule, lean preference, default. A `staffing_note` explains a
choice against the policy or preference. Forks keep their source engine and
report the same note. See [staffing policy](staffing-policy.md).

`isolated=true` (or a `task_id` with its own worktree) keeps workers
out of each other's checkouts; share a checkout only when you mean to.

Pick the model too. Every spawn names one: the `model=` you pass, else the
task's default (`metadata.models`, inherited by subtasks), else the host's
`[models]` engine default (Claude `claude-opus-5-5`, Codex `gpt-6.1-sol`).
Give routine, well-specified work a cheaper model (`model="gpt-6-luna"`), and
set a task default with `create_subtask(model=...)` so a whole subtree
inherits it. Effort works the same way (`effort=`, default `xhigh`; levels
low/medium/high/xhigh, plus max on Claude): lower it for mechanical work.
See [explicit model and effort](models.md).

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
  Workers set their own status: `done`, `blocked` (with `blocked_by`, or
  `blocked_on` + `check_back`), and for a long job the form in section 4.
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
| `holder_idle` | every holder sat at its prompt for the board's `idle_s` (20 min by default; sejd 30 min), not while a declared job runs (**off by default**; enabled per board) |
| `holder_waiting_on_human` / `holder_errored` | a holder needs a person, or failed |
| `stale` | nobody touched it for 2 h (longer for a long declared job) |
| `overdue` | waiting past its ETA plus a grace; the holder is asked first |
| `check_back` | check-back due: decide (re-block with a new check-back, activate, drop). It is not a sign the blocker cleared |
| `blocker_dropped` | something it waited on was dropped; decide what it needs now |
| `unlaunched` | strict boards: active with no live holder on it for 15 min (after an unblock, handback or assignment), outside a job's ETA: launch or reassign; `board()` lists who is free. Never escalated to Owner |
| `needs_triage` | strict boards: does not fit (waiting, open, a block with no trigger or check-back, a job with no ETA): make it active + ETA + job, blocked with a trigger, or drop it. Never escalated to Owner |
| `holder_overloaded` | strict boards: a session holds more active items than the board allows (`max_active_per_holder`, default 2) outside a job's ETA; block or hand back the extras. A warning, never escalated |

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
a long test suite) is declared on its item, with an ETA and a one-line note.

- **Strict boards** (`backlog_allowed: false`, e.g. sejd): active + eta (job
  noted). `item_set(n, "active", eta="40m", job="full daemon suite, pid
  41231")`. The item stays active because someone's job is running; its
  holder may sit idle meanwhile without being flagged, and at the ETA it is
  asked (`eta_passed`). These boards refuse `waiting`, `open` and unheld new
  items: anything else that cannot proceed is `blocked` with `blocked_by` or
  `blocked_on` + `check_back` (at most 72 h out), and future work is a
  planning task. A handback (`holder="none"`) leaves the item active and
  unheld, and you are told if nobody launches it within 15 minutes.
- **Other boards** keep the waiting form: `item_set(n, "waiting", eta="40m",
  note="full daemon suite")`. The active + eta + job form works there too.

For continuous subtasks also set `metadata.continuous_stage="waiting"` with
`next_action` (the task's stage, not the item's status). A channel post is
optional. An orchestrator does not guess activity from a spinner, a shell or a
stale monitor; it trusts the declaration and follows up when the ETA passes.
`board()` shows the holder's session state and the job side by side; a
session that is idle is not a job that stopped, and a check-back that is due
is not a blocker that cleared.

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
- `ping().owner_availability.level` is informational (`away`, `around`,
  `focused`, `on-call`, or null when unset). Notifications deliver at every
  level; availability never holds, drops or downgrades them. Keep urgency
  honest and respect explicit conversation mutes, DND and read state.
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

## 10. Direct reports are subtasks

Reporting lines follow the task tree. A task reports to its parent task, and the
parent's lead session is its boss; a task with no parent is standalone. A
division is a permanent initiative (`kind="division"`) whose root is its
coordinator task. When you start work that reports to you, make it your subtask
so it shows under you in the Sessions tree and the `org` tool:

- `create_subtask(...)` and then `start_session(task_id=...)`, or
  `start_session(isolated=true)`, which does both in one call;
- `propose_task(..., reports_to="me")` when Owner should triage the draft first.

A plain `propose_task` creates a standalone task, and `start_session` without
`isolated` or `task_id` adds a co-worker on your own task. Neither creates a
direct report.

To find your boss, call `org()`. It returns your task and its lead, the parent
task and its lead, the chain to the root, and your division. `org(view="reports")`
lists your reports, and `org(view="division")` shows the whole tree. A task's
lead is the session you pin with `org(action="set_lead")`. Without a pin, it is
the earliest-spawned live Claude or Codex session on the task. Reporting lines
say who to tell. They don't grant approval or merge authority.

Owner can lead too. A task pinned to `"owner"` is led by Owner, and a
division's root task with no live lead has Owner as stand-in (misc divisions
such as `pt-misc` and `cm-misc` have no session on their root, so Owner leads
them). `org()` then reports your `boss_lead` as `{"owner": true, "stand_in":
...}`: in an Owner-led division, Owner is your boss, so escalate to Owner
(`notify_user`, or the channel Owner reads) rather than looking for a lead
session. Only an orchestrator can make Owner a task's lead, or undo it
(`org(action="set_lead", session_uid="owner", task_id=...)` on a task below
its own, or with global permissions), and never on its own task; Owner does it
from A-e.
