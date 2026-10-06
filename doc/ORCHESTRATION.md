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

Each unit of work in flight has exactly one named holder (a session), a one-line
statement of what "done" means, and a next action. "I'll start X" is not a
state: either X has a holder now, or it is not in flight. Keep the list in one
place that Owner can read without asking you (until board tools ship: a pinned
message or a file in the initiative's shared directory, updated when something
changes, not on a timer). Check that list, `list_sessions` and the workers'
transcripts before you message a lane for status.

Work-item board tools (`item`, `board`) are being added by the same initiative;
this section will describe them when they ship.

## 4. Long jobs are declared, not inferred

A job you expect to run longer than 20 minutes (a build, a benchmark, a backtest,
a long test suite) is declared as waiting, with an ETA and a one-line note:
post it in the coordination channel or thread ("waiting: full daemon suite,
ETA 19:40Z, then rebase and hand off"), and for continuous subtasks also set
`metadata.continuous_stage="waiting"` with `next_action`. An orchestrator does
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
3. The work list (section 3), `list_sessions` for your workers, and
   `read_last_turn` on any worker whose state is unclear.

Then continue; do not repeat finished summaries or re-dispatch work that has a
live holder.

## 9. Messaging manners

Quick replies are fine; usual posts are one to three short paragraphs, with a
file reference for anything longer. Reply when you are acknowledging actionable
work or supplying a decision; avoid acknowledgement ping-pong. Messaging does
not grant merge, deploy or session-control permissions; those still come from
Owner or the task's authorization.
