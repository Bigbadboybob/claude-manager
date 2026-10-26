# Transcript rotation (#24)

Claude Code rereads a session's whole transcript on every resume, and
compaction appends to the file without shrinking it. A transcript of 1–2 GB
therefore makes a session slow to start and laggy to drive. CM *rotates* such
a session: it restarts it at the same CM uid in a fresh transcript that holds
only the conversation after the latest compaction boundary.

## When it runs: at a turn end

An agent that is woken by messages and finished background jobs is idle for
only a moment after each turn, so a rotation cannot wait for minutes of idle.
CM starts a due rotation from the turn-end report itself (`session.turn_ended`
/ `session.agent_report`), about 300 ms after the turn ends. A session is
due when:
- its transcript is over `max_bytes` (the policy; a before-notice goes out on
  the first such turn end, and the rotation follows at the next one), or
- a rotation is armed for it: an agent's `rotate_transcript`, or an
  operator's forced one (Alt+T) that met a turn in progress. A forced
  request that is refused for a reason that clears by itself (the agent is
  mid-turn, a job is running, a wake is being delivered) returns
  `armed: true` and runs at the next turn end. An arm that finds no usable
  turn end in 6 h is dropped.

The 5-minute policy pass stays as a fallback.

**Wakes are held, not raced.** While a session is due and its turn-end
attempt was refused by something short-lived, and while it rotates, CM holds
its native wakes (chat messages, monitor fires): they stay `pending` in the
durable queue (`notifications/<uid>/hold`, which expires after 30 minutes) and
go to the new process once the rotation finishes, after its after-notice.
Rotation and handoff notices are exempt. A handoff holds wakes the same way
while it runs.

## How a rotation runs

`daemon/src/rotation.rs`:

1. Record an in-flight marker (`~/.cm/rotations-inflight.json`), then stop
   the process (`killed_by = "transcript-rotation"`). MCP monitors treat the
   stop as "still running" for up to 5 minutes. The TUI keeps the row (also
   a worker's section and placement) and reattaches it once the uid is back.
2. Respawn through the normal revive path as
   `claude --resume <old> --fork-session --session-id <new>`. The uid, chat
   name, org seat, task, workspace and global permissions are unchanged.
   For files over 5 MB, Claude copies only the chain after the last
   `compact_boundary` into the new file. A 1.7 GB transcript forked in 23 s
   (warm cache, 480 MB peak memory) to a 0.3 MB file on cm-sessions.
3. Deliver the after-notice. It tells the agent to re-read its board items
   and standing rules from CM, and its first message is what writes the new
   file. A `turn_end` monitor absorbs the turn this notice causes; it does
   not fire it as the worker's completion.
4. Confirm the rotation from the new transcript, not from keystrokes, within
   180 s plus 60 s per GB of source (fractional; at most 20 min). The fork
   took if it recorded the after-notice's marker or any new turn: a Claude
   fork's file is only written when its first message is recorded, whatever
   that message was; a Codex fork needs a new user row after its copied
   history.
5. If it is not (an API error, a rate-limit menu, an early exit), resume the
   old id unchanged and tell the agent. The policy then leaves the session
   alone for 6 h, doubling with each consecutive failure (at most a week;
   the counts persist in `~/.cm/rotation-failures.json` across restarts).
   Owner is alerted at once when a failed rotation leaves the session not
   running, and from the second consecutive failure otherwise.

The restored session's own `--dangerously-load-development-channels`
startup confirmation is accepted by CM for up to 3 minutes after launch, even
when Claude first draws its banner (a large resume or fork does). Before
this, a rotated fork could stop at that dialog until someone pressed Enter.

A brain restart between the stop and the confirmation leaves the marker. The
next brain resumes a session that is not live on its original transcript
(and tells it). A live session still on its original transcript is left as
it is. A live fork that has not confirmed gets the after-notice again and is
verified the same way: on success the lineage is recorded, otherwise it is
rolled back to the original transcript.

CM never writes into Claude's files. The old transcript stays as it was, and
the lineage (old id → new id) is kept in `~/.cm/daemon-resume-identities.json`
(`rotated`). The A-s Resume / A-e Transcript picker labels the old file
`(rotated → <new>)`.

## Triggers

- **Automatic** (`[transcript_rotation]` in `daemon.toml`, on by default).
  The first release rotates on size only:
  ```toml
  [transcript_rotation]
  enabled = true
  max_bytes = 800000000      # 800 MB; to be lowered once big rotations are timed
  max_compactions = 0        # 0 = off; N rotates after N compact boundaries
  idle_secs = 300            # agent idle at least this long
  owner_quiet_secs = 600     # Owner has not viewed or typed in it for this long
  check_secs = 300
  ```
  Each pass stats the transcripts and reads only files of at least a quarter
  of `max_bytes` for compactions (when counted), on a thread at nice 19 and
  idle I/O priority. One rotation runs at a time per host, with a 2-minute
  pause between them; each pass starts at most one. A before-notice is
  published once per transcript, and never to a session the boundary will
  refuse anyway (a workflow participant, a continuous session mid-run).
- **Operator:** `session.rotate_transcript {uid, force?, reason?}`, or
  `{uid, status: true}` to read the latest record. In the TUI, **Alt+T** on a
  Claude row asks first, naming the session and its transcript size. It is
  forced: it skips the idle and Owner-quiet waits, but anything in flight
  still refuses.
- **Agent:** the MCP tool `rotate_transcript(reason=...)` queues a rotation
  of the caller's own session only. It runs at the next safe boundary,
  whatever the transcript's size.

## Safe boundary

At a turn end (or when forced) the agent need not have been idle for
`idle_secs`, and queued wakes do not block (they are held); only a wake being
submitted to the old process in the last 30 s does. Owner-quiet and the
unsent-draft check apply to the automatic policy only, not to an agent's or
operator's request. Otherwise a rotation is refused or deferred unless all of
these hold:

- the session is live, Claude and not bash;
- it is idle (for `idle_secs` on the fallback pass), with no tool call or
  background job in flight. A job whose process is gone has exited and does
  not count, even if the agent's last snapshot still lists it;
- it has no pending scheduled wakeup (a session cron: CM does not carry
  crons across a fork, so a restart would lose it);
- on the fallback pass, it has no queued or recently submitted native
  notices (other than rotation's own). An unconfirmed receipt older than 10
  minutes never blocks, and one older than 24 h is retired (`cancelled`,
  `retired: "stale_receipt"`);
- Owner has not viewed or typed in it for `owner_quiet_secs`, and has not
  typed in it since its last turn ended (an unsent draft may be in the
  composer; a forced rotation skips this);
- it is not a workflow participant;
- it is not a continuous task with a run in flight (and continuous
  supervision does not respawn a session while it is being rotated);
- no other rotation is running on the host (checked and claimed under one
  lock, so Alt+T and a policy pass cannot both start).

A continuous task's fires (scheduled, queue admission, run_now) skip as busy
while its orchestrator is being rotated, so the scheduler never mints a second
orchestrator in the gap.

## Resume paths follow `continued-in`

Separately, when Claude itself moves a conversation (a background-session
handoff appends `{"type":"continued-in", …}` to the old file), every CM
resume path follows the chain to the newest file and rebinds the row:
daemon restore, revive, `session.revive`, A-s Resume, brain-restart
adoption, and the TUI's startup restore and A-O
(`daemon/src/transcript_chain.rs`). The scan mirrors Claude's reader: a
record followed by more conversation means the old file was resumed anyway
and is no longer superseded.

## Visibility

`list_sessions` rows carry `transcript_bytes` and `rotation`. The sidebar
badge shows `⧉1.7G` past 800 MB, and the A-i peek shows the transcript size.

## Codex

Codex rollouts grow the same way: on cm-sessions, 1,817 rollouts take 17 GB
and the largest is 3.2 GB. `codex fork` is the equivalent of Claude's fork,
and already compacts: forks of a 257 MB rollout with 52 compactions came out
at 7–9 MB. An explicit rotation (Alt+T, the operator RPC, an agent's
`rotate_transcript`, or `session.handoff` with nothing to change) rotates
Codex too: the fork's new rollout is bound by the detector, and the
rotation confirms once it records the after-notice's marker. The automatic
policy stays Claude-only for now. See [session handoff](session-handoff.md).
