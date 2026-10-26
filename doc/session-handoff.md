# Session handoff (#25)

`session.handoff` moves a CM session to another engine, model or effort at
the same CM uid. The chat identity, org seat (member, main/assistant
occupancy), task, workspace, global permissions and parent edge all stay.
Shells are never handed off. Continuous sessions are refused: their engine
and model are the continuous task's settings, which the scheduler applies on
each fire. Workflow participants are refused too (their run drives them;
stop the workflow first).

## Three kinds, cheapest first

| Request | Kind | What happens |
|---|---|---|
| Same engine, nothing to change | `rotate` | A transcript rotation ([transcript-rotation.md](transcript-rotation.md)): Claude `--fork-session`, Codex `codex fork`. |
| Same engine, another model or effort | `repick` | Stop, then resume the same conversation with the new `--model` / `--effort`. |
| Another engine | `switch` | The handoff below. |

### A switch

`daemon/src/handoff.rs`:

1. At a safe boundary (the same rules as a rotation), CM asks the outgoing
   agent to reply with a handoff document between `<cm-handoff>` lines:
   - the goal and who asked;
   - where things stand;
   - open items and holds by board and number;
   - waits and their ETAs;
   - decisions, standing rules and Owner rulings;
   - branches, commits and files;
   - the exact next step.

   CM writes the reply to `~/.cm/handoffs/<uid>/<unix ts>.md` (mode 0600,
   outside any repo), so a sandboxed engine needs no write access. No
   document within 10 minutes cancels the handoff, and nothing changes.
2. CM stops the old engine (`killed_by = "handoff"`). Monitors and the TUI
   treat the stop like a rotation's: the session is coming straight back.
3. CM spawns the new engine fresh at the same uid. The old engine's model and
   effort do not carry over: the request's, else the task or config default.
4. Its first message tells it to read the document, then re-read its board
   items, standing rules and inbox from CM. The old engine's monitors died
   with it (they live in its MCP server), so the notice also tells it to
   re-arm any watch it still needs.
5. CM confirms the takeover from the new engine's transcript: the message's
   marker followed by an assistant reply, within 5 minutes. If the new
   engine errors first (a bad model, auth, a rate limit), CM falls back at
   once. Otherwise, on a timeout, it stops the new engine, resumes the old
   engine's conversation and tells it.

A stop that has not finished 20 s after the kill is not reported as "left as
it was". Owner is alerted, and CM keeps watching the process for 10 minutes:
if it exits, CM resumes the original conversation; if it doesn't, Owner gets a
second alert.

The old and new processes never overlap: a CM uid is one registry slot and
the MCP auth identity (Swarm-Coord ruling on #25). Nothing is lost on a
failure, because the old conversation stays resumable. The lineage
`{from_engine, from_id, to_engine, to_id, file}` is kept in the
resume-identity archive (`handoffs`).

On a member's session the seat stays put, because incarnations are keyed by
pid. The API's heartbeat sees the engine change on that pid and records an
`engine_switch` org event (`prev` / `new` engine and role).

## Triggers

- **TUI:** **Alt+Shift+E** on a live Claude/Codex row opens a dialog: Engine
  (←/→), the shared Model row (Shift+←/→ for effort), and a line saying
  which kind it will be. It is sent with `now`: the idle and Owner-quiet
  waits are skipped, but anything else in flight (a turn, a tool call, a
  background job, a scheduled wakeup, an undelivered notice) refuses it at
  once, and the status line says why. Try again when the session is idle.
- **Operator:** `session.handoff {uid, to_engine?, to_model?, to_effort?,
  reason?, now?, wait_secs?}`; `{uid, status: true}` reads the latest record.
  Without `now` it waits for the boundary, by default for up to 6 h. With
  `now` the boundary is checked at once, and the request waits at most 10
  minutes for the host's slot (another handoff or rotation) before it is
  dropped.
- **Owner's bulk use:** `session.handoff_many {uids? | from_engine?, …}`,
  operator-only. It takes named `uids`, or every agent session on
  `from_engine`; a selection never includes shells, continuous sessions or
  workflow participants. Each session waits for its own boundary.
- **Agent:** the MCP tool `request_handoff(to_engine?, to_model?, to_effort?,
  reason?, session_uid?)`, never with `now`. Without `session_uid` it hands
  off the caller's own session. With it, another session the caller has
  authority over (below).

These operations are deliberately separate:

- `request_handoff` preserves the current CM identity and waits for that
  session's safe boundary. It needs the outgoing session to answer when the
  engine changes, so a rate-limited or errored source may remain queued.
- `fork_session` starts a new session with a copied engine transcript and a
  new task/workspace. It leaves the source running and does not preserve the
  source identity. It can use a live session or an exit tombstone when CM
  still has a usable transcript; it does not wait for another source turn.
- `start_session` is a fresh worker spawn. It has no source conversation and
  its task prompt is the only context CM supplies automatically.

If a request is uncertain, inspect its returned receipt or query the session
before retrying. A handoff request can be cancelled by the operator while it
is still queued:

```sh
scripts/cm-op session.handoff_cancel '{"uid":"<session uid>"}'
```

Cancellation is operator-only, affects only the named queued request, and
returns `Conflict` once that handoff has started. It does not stop a running
handoff, rotate a session, or kill a worker. A missing or already-finished
request returns `NotFound`.

When a fork cannot read the retained source transcript, the error means that
there is no fork context on this host; wait for a transcript to be bound or
use `start_session` for a fresh worker. When an engine switch is required for
the same CM identity, use `request_handoff` rather than `fork_session`.

## Handing off (or rotating) another session

`request_handoff(session_uid=…)` and `rotate_transcript(session_uid=…)` (RPC
`uid`) target another session under authority CM already enforces for
controlling sessions, nothing looser:
- the session-caller scope `send_input` and `kill_session` use (the caller's
  task tree and workspace, managed-by descendants; everything with global
  permissions);
- or, as a lead, a session of a member below the caller in the org: the
  member's main session, assistants or workers, by the planning API's
  manager-chain check, the one move and retire use (`POST
  /org/members/{ref}/authority`, which also confirms the target is still
  that member's session; the member's own main also counts for its
  assistants and workers).

Anything else is refused as unauthorized, before anything about the target
is said. Shells, continuous sessions and workflow participants are refused
as targets (to a caller with authority over them). The request waits for the
target's own safe boundary (agents never get `now`). The record carries
`requested_by` ("label (uid)"), and the target's notices say who asked.

Handoffs run in parallel, any number per host (Owner, 2026-10-26); only one handoff or rotation runs per session. An operator's request (Owner's, `handoff_many`) skips the idle and Owner-quiet waits but still waits out a turn, a job or an unsent draft. Automatic transcript rotations stay one at a time per host. `list_sessions` rows and
`resolve_authorized_session` carry the `handoff` record, and a `turn_end`
monitor absorbs the turns CM's own prompts cause: two for a switch (the
document and the takeover), one for a repick.

A brain restart mid-handoff is covered by the same in-flight marker a
rotation uses:
- a session that is not live is resumed on its old engine;
- a live session still on its old engine is left alone;
- a live session whose handoff is already in the lineage (it finished just
  before the restart) is left alone;
- otherwise the new engine gets its takeover notice again and is confirmed
  exactly as above, falling back to the old engine if it doesn't answer.

## Deploying

The daemon and the TUI ship together, laptop TUI included. An older TUI does
not mirror the engine change onto the row, so a switched session would keep
its old engine mark and keys until the TUI is upgraded.

## Verified (scratch sessions, isolated HOME, real engines)

Claude haiku → Codex `gpt-6-luna` → Codex rotation → Claude haiku, all at
uid `ts-0e2e-25`:
- The Codex successor read the document, re-read its CM state, completed the
  next step and recalled the code word.
- The Codex rotation went from 315 KB to 29 KB with recall intact.
- The way back respawned `claude --model claude-haiku-4-5 --effort low` with a
  fresh session id, and recall held.
