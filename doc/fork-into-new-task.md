# Fork into new task

Continue a Claude or Codex conversation in a new task without disturbing the
original session. Owner presses `A-F` on a session row; agents call the MCP
`fork_session` tool. Both use one daemon RPC, `session.fork`, on the host that
owns the source session, so local and cloud sessions behave the same.

## What happens

1. The daemon resolves the source session (live, or a recent exit tombstone)
   and its bound conversation id. Bash sessions, and agents that have not
   finished a turn, cannot be forked.
2. It creates a planning task. The task is a subtask of the source's task
   unless a `parent_task` is given. It is created in branch mode, the same way
   as `create_subtask(worktree_mode="branch")`. The `cm-sub/<chain>-<id>`
   branch is cut at the source worktree's committed `HEAD` (`base="source"`,
   the default) or at the project's trunk (`base="trunk"`). Uncommitted
   edits in the source worktree are not carried over.
3. It starts a normal tracked agent session in the new worktree, bound to the
   new task, with the usual MCP configuration, hooks and environment. The
   launch command is the engine's native fork:
   - Claude: `claude --resume <source id> --fork-session --session-id <new id>`.
     The new conversation id is set by CM at launch, so the row is bound to the
     fork, not to the source, from the start. Claude copies the history into
     the new worktree's project directory. Resuming from another directory is
     supported when it is combined with `--fork-session`.
   - Codex: `codex fork <source id>` through CM's native Codex launcher. Codex
     creates a new thread and rollout file. The daemon's transcript detector
     (and the `/proc` watcher) binds the row to that new rollout. The launcher
     also passes `-c tui.resume_cwd="current"` to the frontend. Without it,
     Codex 0.160 asks which working directory to use, and its default is the
     source session's directory.
4. If spawning fails, the new task and worktree are rolled back.

The source session, its task and its transcript binding are not changed.

## Why not snapshots

The older agent-memory feature (`A-b` save, `A-z` catalog, Seed field on
`A-n`/`A-s`) copies a session's transcript into `~/.cm/agent-memories/<name>/`.
That copy stays until it is deleted by hand, and each seed makes another copy
in the new worktree. Fork into new task writes nothing of its own. Only the
engine's normal transcript for the new conversation is created.

## Limits

- Claude writes the forked transcript file lazily, when the first message is
  sent. If a fork is started without a prompt and the daemon restarts before
  anyone types in it, the restart cannot resume the fork, so it starts a fresh
  conversation.
- An existing `A-b` snapshot remains a separate, reusable template. Forking
  does not replace saved snapshots.
