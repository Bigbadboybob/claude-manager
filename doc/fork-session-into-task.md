# Fork a session into a task

This continues a Claude or Codex conversation in a new or existing task.
The original session is left as it is. Owner presses `A-F` on a session
row. Agents call the MCP `fork_session` tool. Both use one daemon RPC,
`session.fork`, on the host that owns the source session, so local and
cloud sessions behave the same.

There is no "move" option. A fork always leaves the source running.

## Choosing the target

- **New task** (`task_name`). The daemon creates a planning task in branch
  mode, the same way as `create_subtask(worktree_mode="branch")`. By default
  it is a subtask of the source session's task. In the TUI the Parent row can
  pick any task, or "top-level, same project". In the MCP the equivalents are
  `parent_task_id` and `top_level=true`. The task gets its own
  `cm-sub/<chain>-<id>` worktree. The branch is cut at the source worktree's
  committed `HEAD` (`base="source"`, the default) or at the project's trunk
  (`base="trunk"`).
- **Existing task** (`task_id`). No new task is created. The fork joins that
  task's workspace exactly as `start_session(task_id=…)` does. This includes
  re-creating a reaped worktree. Sharing a checkout with live sessions is
  allowed without a prompt; the result lists them in
  `workspace_shared_with`. A task that has never run gets a worktree minted
  by the `start_session` mint path, cut at `base`.

**Scope.** The task the fork lands in, or the new task's parent, must be in
the caller's own task tree. Being able to reach the source session is not
enough. Taskless agents, and top-level tasks, need global permissions. The
operator (TUI) is not restricted.

## The launch

The new session is a normal tracked agent session. It is bound to the
target task and gets the usual MCP configuration, hooks and environment. Its
launch command is the engine's native fork:

- **Claude.** The command is
  `claude --resume <source id> --fork-session --session-id <new id>`. CM sets
  the new conversation id at launch, so the row is bound to the fork, not the
  source, from the start. Claude copies the history into the new worktree's
  project directory. Resuming from another directory is supported when it is
  combined with `--fork-session`.
- **Codex.** The command is `codex fork <source id>`, run through CM's native
  Codex launcher. Codex creates a new thread and rollout file. The daemon's
  transcript detector (and the `/proc` watcher) binds the row to that new
  rollout. The launcher also passes `-c tui.resume_cwd="current"` and
  `--cd <worktree>` to the frontend. Without them, Codex 0.160 asks which
  working directory to use, and its default is the source session's
  directory.

**Folder trust (Codex).** Before every Codex spawn the daemon adds the
working directory to `~/.codex/config.toml` as a trusted project, as it does
for Claude in `~/.claude.json`. This matters for forks: CM's remote Codex
frontend (`codex --remote … --cd <worktree>`) checks trust for the exact
directory and does not fall back to the trusted main checkout of a linked
worktree, as a plain `codex` does. A new worktree therefore opened at a
"Folder access — Trust this folder?" screen (verified on codex-cli 0.160.1).
Before 2026-10-07 the pre-trust missed CM's native launcher argv and was not
called on the holder spawn path, so it never ran.

**No first prompt.** A fork starts at its composer with nothing typed into
it. Owner gives it its first message. The TUI form has no Prompt field. An
agent may still pass `prompt=` to `fork_session`; that text is delivered as
before and arms the usual completion monitor (`prompt_source: "caller"`;
otherwise `"none"`). An earlier version typed a default note into every fork.
On Codex that note's Enter answered the folder-trust screen and the fork
started blank (2026-10-07), so it was removed.

What that note said is now in the result and on the TUI status line:
- `forked_from`: the source session's uid, label, transcript id, worktree
  and `branch`;
- `branch` and `worktree_path`: where the fork now works;
- `commits_not_carried`: commits on the source that the fork's checkout
  lacks (`git log --oneline <fork HEAD>..<source HEAD>`, at most 10, with
  `commits_not_carried_truncated` set when there are more). The status line
  shows their short SHAs.

**Durability.** Owner accepted this: a Claude fork writes its transcript
only when it receives its first message. Until then there is nothing on disk
to resume, so a fork that was never prompted does not survive a daemon
restart or reboot. Codex creates its fork thread at launch.

**Uncommitted edits.** These never come along, because the cut is a commit.
The result reports `uncommitted_left_behind` and `uncommitted_files`, and
the TUI status line warns about them.

**Failures.** If the spawn fails for a new task, everything is rolled back:
the planning row, the workspace, the auth edges, the worktree and its
zero-commit `cm-sub/` branch. An existing task's workspace is never removed.

**Retries.** The RPC can take a while, because the host provisions a
checkout before it replies. Each call therefore carries a `request_id`. The
MCP tool generates one per call; the TUI uses the new session's uid. A retry
with the same key returns the first fork instead of creating a second one.
The MCP tool waits up to 180 s.

**Older hosts.** A host whose brain predates `session.fork` answers
`unknown_method`. Both the TUI and the tool report this as "needs a brain
deploy".

## Why not snapshots

The older agent-memory feature (`A-b` save, `A-z` catalog, Seed field on
`A-n`/`A-s`) copies a session's transcript into `~/.cm/agent-memories/<name>/`.
That copy stays until it is deleted by hand, and each seed makes another
copy. Snapshots remain available as reusable templates. Forking writes
nothing of its own beyond the engine's normal transcript for the new
conversation.
