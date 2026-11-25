# Tasks become board items only (S8)

Status: design draft, 2026-11-25 (Swarm-Coord). Follows
[one-session-hierarchy.md §9](one-session-hierarchy.md#9-after-this-tasks-become-board-items-only)
and closes org-model close-out piece 5 (S7–S8,
[org-model.md](org-model.md) §13). Nothing here is built yet.

## 1. Where we are

The one-session hierarchy shipped (98e77d55, then reassign in fa72f402 on
2026-11-25). Sessions are placed, grouped, sectioned and controlled by the
org chart: member, manager, spawner. `session_scope = "manager_chain"` is the
default on every host, and `task_tree` is the rollback.

What still ties a session to a task is plumbing:

| Link | Used for today | Code |
|---|---|---|
| Task row → worktree | `start_session(task_id=…)`, `create_subtask` branch mode and `fork_session(task_name=…)` mint `cm-sub/<slug>-<task7>` | `daemon/src/worktree.rs::mint_task_worktree*` |
| Task row → brief | Prompt auto-delivery when `task_id` is explicit and `prompt` is empty | `mcp_start_session` (`prompt_source="task"`) |
| `agent_task_edges` | Lets a proposer spawn on the task it filed. The rollback scope's descendant check | `daemon/src/state.rs`, `control/auth.rs` |
| Task-bound workspace | Reaped-worktree self-heal, terminal-task sweep (rollback mode only), Alt+d cleanup | `worktree::ensure_worktree_materialized`, `terminal_sweep.rs` |
| Task sub-view | A second sidebar layout grouped by `parent_task_id` | `tui/src/app/tree.rs` |
| Task-object auth | `update_task`, `create_subtask` and `mark_subtask_done` stay task-scoped in both modes | `control/auth.rs` |

## 2. End state

- **Work is an item.** It has a brief, a holder (member or team), status,
  blockers and a board. A Planning task is the same row seen as backlog (O6,
  O7).
- **A checkout belongs to a member.** New checkouts are
  `cm-ws/<handle>-<ws7>` (org-model §5). `home` is the office, `stream` is a
  writing assistant's own checkout, and `ephemeral` closes with its work. The
  `cm-sub/` and `cm/` parsers stay forever, so old branches keep resolving.
- **A spawn names its work by item, not task.** Calls look like
  `start_session(item=N, …)`, `fork_session(item=N, …)` and
  `create_subtask(…)`. The last one becomes "create an item, optionally with a
  worker".
- **Control is the manager chain only.** Task-object checks become item
  checks: holder, manager chain, board admin.
- **The Task sub-view is a filter, not a layout.** Selecting a task or item
  highlights the sessions that hold or worked on it inside the org tree.

## 3. Steps

Each step ships and rolls back on its own. Only step 5 is irreversible.

1. **Item-first spawn (additive).** `start_session(item=N)` and
   `fork_session(item=N)` mint a member checkout. The member is the caller's,
   or the item holder's for a lead acting on its own board. The branch is
   `cm-ws/<handle>-<ws7>`, cut from the repo the item's project names. The
   brief comes from the item description. `task_id=` keeps working unchanged.
   The result reports `workspace_kind` and `item`.
2. **`create_subtask` → item.** Branch mode creates an item under the
   parent's item (`part_of`) and, if asked, an assistant with a `stream`
   checkout. The task row is written through the existing adapter, so old
   clients and the Planning view still see it (§16.1 "adapter for old clients").
3. **Task sub-view as filter.** Alt+v cycles Status → Org, and the old Task
   layout goes. `/` or a picker selects an item or task and dims every row
   that doesn't hold it. This is a laptop TUI release only.
4. **Item-scoped auth.** `update_task`, `mark_subtask_done` and
   `set_subtask_status` authorize on item holder, manager chain or board admin,
   and log where the old task check would have answered differently (shadow).
   It turns authoritative after a week of clean logs, the same pattern as S7.
5. **Retire task scope (irreversible, announced in #cm-general first).**
   - Remove `session_scope = "task_tree"`, `agent_task_edges` and the
     task→workspace binding as an auth input.
   - Keep the binding only as a display hint for old rows.
   - Drop the terminal-task sweep; Alt+d cleanup already keys on workspace.

   Existing task-bound sessions keep running; nothing is rebound.

## 4. What does not change

- Board items, groups, epics, holders and `place` (S6 holders already key on
  member).
- Planning stays a backlog view over the same rows.
- Reaped-worktree self-heal keeps working. It resolves the branch from the
  workspace record first, then from the old task naming.
- Backtests and cloud workers are neither members nor items (org-model §17).

## 5. Risks

- **Old clients that write tasks.** A Codex session started before step 1
  keeps its tool list. The adapter must accept `task_id` calls until those
  sessions are gone; it never removes a tool or adds a required parameter
  (`mcp_version.py` compatibility rule).
- **Rollback window.** Steps 1–4 roll back by config or by redeploy. Step 5
  is the boundary, so it needs a week on step 4 with no unexpected shadow
  differences, plus Owner's go.
- **Mid-operation spawns.** A child spawned while a reassign or promote runs
  can keep its old attribution. This was seen in fa72f402. Item-first spawn
  should read the member at spawn time rather than from the parent's stamp.

## 6. Open questions for Owner

1. Should a lead's `start_session(item=N)` on an item held by one of its
   reports spawn under the report (holder) or under the lead (caller)? The
   draft says holder, because work lands in the office that owns it.
2. Should the Task sub-view go away at step 3, or stay one more release as a
   fallback beside the filter?
3. Is "a week of clean shadow logs, then Owner's go" the right gate for
   step 5, the same as S7?
