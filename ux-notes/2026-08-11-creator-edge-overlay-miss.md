# Orchestrator loses auth to its own live workers mid-flight — creator-edge overlay miss (2026-08-11)

Context: I'm the ux-notes-triage orchestrator session. I created two branch subtasks via `create_subtask` (TUI-served route) and spawned a worker on each; both launches and a dozen subsequent `read_last_turn`/`send_input`/monitor operations worked. ~50 minutes in, the TUI was restarted. From that moment every call against BOTH workers answered `unauthorized ... outside caller's task subtree / workspace per TUI-mirror rule`, `list_sessions` stopped showing them entirely (even `include_exited=true`), and my re-armed monitor fired `- <uid> (error)` with the all-EXITED trailer — for a worker that was alive and mid-implementation the whole time. Items most severe first.

## 1. TUI-served create_subtask registers the workspace binding but NOT the creator auth edge

Post-incident, the daemon's persisted state (`~/.cm/daemon-sessions.json`) shows my subtasks present in `bindings` (task→workspace) and their sessions present under `workspaces`, but ABSENT from `agent_task_edges` (67 older creator edges survived — mine were never added). The overlay is documented as the thing that makes creator edges survive `task.update_tree` pushes and daemon restarts; the TUI-served `task.register_agent_subtask` path evidently records the binding + worktree but not the durable edge (or records it only in the replaceable tree). Auth worked initially only because the ordinary parent edges from creation were still in the daemon's task tree.

Suggestion: `task.register_agent_subtask` must write the `agent_task_edges` overlay entry (created-task → creator-task) exactly like the daemon-served path does, and persist it. A regression test: create subtask via the TUI route, clobber the tree with an update_tree push lacking the subtask, assert the creator can still resolve its worker.

## 2. A TUI restart's update_tree push silently revokes in-flight descendant auth

When the restarted TUI pushed its fresh task tree, the parent edges for my (agent-minted, still-running) subtasks vanished and every in-flight orchestration handle broke at once. Nothing surfaced this: no event, no error at push time, just `unauthorized` on the next call. If item 1 is fixed the blast radius shrinks, but the general shape — a push replacing the tree can only ever REMOVE auth from live sessions — deserves a guard: warn (or refuse to drop) tree nodes that currently have live bound sessions, or diff the pushed tree against live bindings and log loudly.

## 3. Monitor auth-failure fires read as death: `(error)` + "The watched session(s) have EXITED"

My monitor on the healthy worker fired `- <uid> (error)` and the trailer asserted the watched sessions have EXITED and I should "start a fresh session". Acting on that would have spawned a duplicate worker against a live one (in the same worktree!). An auth/resolve error while watching is not an exit: the fire should say the monitor LOST ACCESS to the session (with the error), explicitly distinguish that from exit, and not claim there's "no prompt left to send follow-ups to". Related: `list_sessions` hiding rows the caller lost auth to (correct) combined with the exited-shaped fire (wrong) made the false "it died" story internally consistent and thus convincing.

## 4. Smaller

- `read_last_turn`'s unauthorized message ("outside caller's task subtree / workspace per TUI-mirror rule") gives no hint that the tree itself may have changed out from under the caller. When the target uid appears in a workspace the caller created moments ago, a "task tree may have been re-pushed" breadcrumb would cut the diagnosis from an hour to a minute.
- Recovery today requires the operator (re-push a tree containing the subtasks, or grant global perms). An agent-side "re-assert my creator edge" — e.g. `get_task` shows `parent_task_id` = my task, so the planning API already proves the relationship — could let the MCP layer self-heal auth from the API's ground truth instead of trusting only pushed state.

## What worked (keep)

Host-side artifacts made recovery possible without any session access: transcripts under `~/.claude/projects/<encoded>/` told me the "dead" worker was alive and typing seconds ago; the worktree + committed branch made the finished worker's output fully usable. The `bindings`/`agent_task_edges` split in the persisted daemon state made the root cause legible from the outside — that observability is why this note has a diagnosis instead of a mystery.
