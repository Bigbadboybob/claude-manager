# Transcript rotation (#24)

Claude Code rereads a session's whole transcript on every resume, and
compaction appends to the file without shrinking it. A transcript of 1–2 GB
therefore makes a session slow to start and laggy to drive. CM *rotates* such
a session: it restarts it at the same CM uid in a fresh transcript that holds
only the conversation after the latest compaction boundary.

## How a rotation runs

`daemon/src/rotation.rs`:

1. Stop the process (`killed_by = "transcript-rotation"`). MCP monitors treat
   this as "still running" for up to 5 minutes, so a monitor does not fire on
   it as an exit.
2. Respawn through the normal revive path as
   `claude --resume <old> --fork-session --session-id <new>`. The uid, chat
   name, org seat, task, workspace and global permissions are unchanged.
   For files over 5 MB, Claude copies only the chain after the last
   `compact_boundary` into the new file.
3. Deliver the after-notice. It tells the agent to re-read its board items
   and standing rules from CM. That first message is what writes the new
   file.
4. Confirm the rotation from the new transcript (a user/assistant row with a
   `parentUuid`), not from keystrokes, within 180 s.
5. If no new transcript appears (an API error, a rate-limit menu, an early
   exit), resume the old id unchanged and tell the agent. The policy then
   backs off for 6 h.

CM never writes into Claude's files. The old transcript stays as it was, and
the lineage (old id → new id) is kept in `~/.cm/daemon-resume-identities.json`
(`rotated`). The A-s Resume / A-e Transcript picker labels the old file
`(rotated → <new>)`.

## Triggers

- **Automatic** (`[transcript_rotation]` in `daemon.toml`, on by default):
  ```toml
  [transcript_rotation]
  enabled = true
  max_bytes = 300000000      # or…
  max_compactions = 200      # …this many compact boundaries
  idle_secs = 300            # agent idle at least this long
  owner_quiet_secs = 600     # Owner has not viewed/typed in it for this long
  check_secs = 300
  ```
  A before-notice is published once per transcript.
- **Operator:** `session.rotate_transcript {uid, force?, reason?}`, or
  `{uid, status: true}` to read the latest record. In the TUI, **Alt+T** on a
  Claude row (forced: it skips the idle and Owner-quiet waits, but still
  refuses mid-tool or mid-workflow).
- **Agent:** the MCP tool `rotate_transcript(reason=...)` queues a rotation
  of the caller's own session only. It runs at the next safe boundary,
  whatever the transcript's size.

## Safe boundary

A rotation is refused or deferred unless all of these hold:

- the session is live, Claude and not bash;
- it is idle for `idle_secs`, with no tool call or background job in flight;
- it has no pending, submitted or uncertain native notices (other than
  rotation's own);
- Owner has not viewed or typed in it for `owner_quiet_secs`;
- it is not a workflow participant;
- it is not a continuous task with a run in flight;
- no other rotation of it is in progress.

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
badge shows `⧉1.7G` past 300 MB, and the A-i peek shows the transcript size.

## Codex

Codex rollouts grow the same way: on cm-sessions, 1,817 rollouts take 17 GB
and the largest is 3.2 GB. `codex fork` is the equivalent of Claude's fork,
and already compacts: forks of a 257 MB rollout with 52 compactions came out
at 7–9 MB. CM's launcher already builds Codex fork argv. A Codex rotation
still needs rollout detection without a pinned id, handling for the owned
app-server, and its own e2e. It is left to the general `session.handoff`
(#25), which shares that spawn.
