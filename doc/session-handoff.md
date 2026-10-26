# Session handoff (#25)

`session.handoff` moves a CM session to another engine, model or effort at
the same CM uid. The chat identity, org seat (member, main/assistant
occupancy), task, workspace, global permissions and parent edge all stay.
Shells are never handed off. Continuous sessions are refused: their engine
and model are the continuous task's settings, which the scheduler applies on
each fire.

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
   items, standing rules, monitors and inbox from CM.
5. CM confirms the takeover from the new engine's transcript: the message's
   marker followed by an assistant reply, within 5 minutes. Otherwise it
   stops the new engine, resumes the old engine's conversation and tells it.

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
  which kind it will be. It runs once the session has nothing in flight.
  Owner asked, so the idle and Owner-quiet waits are skipped.
- **Operator:** `session.handoff {uid, to_engine?, to_model?, to_effort?,
  reason?, now?, wait_secs?}`; `{uid, status: true}` reads the latest record.
  Without `now` it waits for the boundary, by default for up to 6 h.
- **Owner's bulk use:** `session.handoff_many {uids? | from_engine?, …}`,
  operator-only. It takes named `uids`, or every agent session on
  `from_engine`; a selection never includes shells, continuous sessions or
  workflow participants. Each session waits for its own boundary.
- **Agent:** the MCP tool `request_handoff(to_engine?, to_model?, to_effort?,
  reason?)` hands off the caller's own session only, and never with `now`.

One handoff or rotation runs at a time per host. `list_sessions` rows and
`resolve_authorized_session` carry the `handoff` record, and a `turn_end`
monitor absorbs the turns CM's own prompts cause: two for a switch (the
document and the takeover), one for a repick.

A brain restart mid-handoff is covered by the same in-flight marker a
rotation uses:
- a session that is not live is resumed on its old engine;
- a live session that already switched gets its takeover notice again.

## Verified (scratch sessions, isolated HOME, real engines)

Claude haiku → Codex `gpt-6-luna` → Codex rotation → Claude haiku, all at
uid `ts-0e2e-25`:
- The Codex successor read the document, re-read its CM state, completed the
  next step and recalled the code word.
- The Codex rotation went from 315 KB to 29 KB with recall intact.
- The way back respawned `claude --model claude-haiku-4-5 --effort low` with a
  fresh session id, and recall held.
