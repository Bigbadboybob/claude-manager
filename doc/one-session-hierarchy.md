# One session hierarchy

Status: **approved** (Owner, 2026-11-23; see §8). Swarm-Coord, 2026-11-21. This brings
forward and finishes stages S7 and S8 of [org-model.md](org-model.md) §13.
Owner asked for it after phase 6 of [continuous-teams.md](continuous-teams.md).

## 1. Decision (Owner, 2026-11-21)

- **Sessions have one hierarchy: the org chart.** Members report to managers.
  Each member's sessions are its main session plus its assistants and workers.
- **Board items have their own, separate hierarchy:** boards, groups and
  epics. Planning tasks are what items are built on. Their `parent_task_id`
  shows only inside the board as "part of".
- **Session-control permission moves to the manager chain in the same
  change.**

The task tree (`parent_task_id`, a task's bound workspace,
`agent_task_edges`) stops deciding anything about sessions:
- where a session appears;
- which sidebar section it inherits;
- its leader mark;
- who may control it.

Tasks stay as plumbing. A row still mints a worktree and delivers a brief to
the session `start_session(task_id=…)` starts.

## 2. Why

Today the org chart (F6), routing and team blocks use members and managers,
while the sidebar, sections, leader marks and control scope use the task
tree. The two disagree whenever a lead's report was not spawned under the
lead's task. That is the normal case for adopted members, continuous teams
and anything started by Owner. Example: Decision Watch reports to
Reliability-Lead in the org, but it has no parent task there. Its "auto"
section therefore resolved to nothing, and it floated loose in the sidebar.

## 3. Placing a session

Every live session resolves to exactly one place, in this order:

1. **Its stamp.** `acting_as` is `main`, `assistant` or `worker`, set at spawn
   (`assistants::stamp_spawn`), on a fork (`fork_org`), by `org.adopt` and by
   the continuous scheduler.
2. **Its spawner chain.** `managed_by_uid`, walked up to the first session
   that has a stamp. The child becomes an assistant of that member, or a
   worker if that member is continuous. The spawner chain counts as part of
   the org: "spawned by" is how an assistant joins its member.
3. **Owner's sessions.** A session Owner launched (A-n, A-s, the TUI) with no
   spawner becomes an `agent` member under Owner. This already happens to fresh
   spawns through `auto_place_owner`. Shells and backtest watch views are
   never members. They display under the member whose workspace they're in,
   or under Owner if that workspace has no member.

Step 2 no longer falls back to the task tree. That removes
`orgchart_teams.rs` rules :615/:651 and `assistants.rs` `by_task` :580.

**One-time stamping pass** (at deploy, per host, idempotent): every live
session that has no stamp gets one. The pass uses today's full resolution,
task tree included, so nothing moves on day 1.
- A session that reaches no member gets the Owner rule.
- Restored and revived sessions without a marker get the same treatment,
  once.

After the pass, no session reaches the end of its spawner chain without
finding a member (`Chain::NoMember` goes away).

**Snapshot.** `org.snapshot` gains `assistants[]` per member
`{session_uid, acting_as, spawned_by, workspace_id, host}`, read from
`org_session_facts`. The TUI, daemons and F6 then read placement from one
place instead of rebuilding it from per-session stamps.

## 4. Sidebar (Sessions view)

The tree is the org chart.

```
▾ Section                        (Owner-created, A-N)
  ▾ ★ Member (main session)      ← the block is a member, not a workspace
      sessions of the main's office, then assistants/workers,
      grouped by checkout when it has more than one (office, stream)
    ▾ ☆ Report member            ← managers' reports nest under them
        …
  ┌ ⟳ continuous team box ┐      ← a continuous team's lead member, decorated as today
```

- **The block unit is a member.** A workspace becomes a sub-label on the
  session rows. A member with several checkouts shows them as small groups
  inside its block.
- **Reports nest under their manager**, to any depth. Owner's direct reports
  are top level.
- **Leader marks** count depth in the manager tree:
  - ★ for Owner's direct reports, ☆ one level down, then ✦ and ✧;
  - only members with a live report or a live assistant get a mark;
  - Owner keeps ♔.
- **Folding** works on any member block that has assistants or reports.
- The continuous team box keeps its header (`⟳ every 3h · next … · ok`).
  It also takes in the lead's whole subtree, not just one spawner level as
  today (`tree.rs:745`).
- Divisions (`divisions_by_task`) become teams: a team block groups its
  members.

### Sections

- **Sections are assigned per member.** The assignment is keyed by member id
  in the manifest, so a replacement main session keeps it. Owner can still
  assign a single workspace (a member's stream checkout, say). An explicit
  assignment always wins.
- **"auto" means the manager's section.** It walks up the manager chain
  until it finds an explicit assignment; otherwise the member is loose.
  Assistants and workers follow their member.
- **Migration of today's per-workspace assignments:**
  - an assignment on a workspace where a member's main session lives
    becomes that member's assignment;
  - an assignment on a workspace whose sessions all belong to one member
    stays as a workspace override;
  - the rest are shown to Owner once, as a list, and are not guessed.
- `set_session_section` keeps its name. It now resolves to the session's
  member, or to the workspace override if `workspace=true`. It is allowed for
  sessions in the caller's new scope (§5).
- A-n inside a section creates a member in that section, as today.

## 5. Control scope (S7, in this change)

The rule from org-model §6.3:

- A **main session** controls:
  - its own assistants and workers;
  - every session of every member below it in the manager chain.
- An **assistant or worker** controls only sessions it spawned itself,
  following its spawner chain.
- `global_perms` still bypasses this. Shells are controlled only by Owner.
- **Task objects keep task scope.** That covers `create_subtask`,
  `update_task`, `list_subtasks`, `set_subtask_status` and
  `mark_subtask_done`. These are board-item plumbing, the separate hierarchy
  in §1. `agent_task_edges` survive for them only.
- **Session operations use only the new rule.** These are:
  - `send_input`, `send_key`, `interrupt`, `kill_session`;
  - `read_session_output`, `read_last_turn`, `list_sessions`;
  - monitors, `start_workflow`, fork, `trigger`;
  - resume, rotate/handoff, `session_wake_schedule`, `set_session_section`.
  Today these call `caller_reaches_target` / `task_in_scope`
  (`auth.rs:278`, `:148`) and the TUI mirror `caller_authorized_for`.
- **Daemons cache the manager chain** together with the member and assistant
  snapshot. The cache refreshes every 5 minutes and also on every org-change
  event.
  - During an API outage it is fail-static: relations already in the cache
    keep working.
  - New org changes and relations created after the last snapshot are refused
    until the API answers.

**Rollout (short):**
1. **48 h union.** Allowed if either the new rule or the old rule allows
   it. Every request that only the old rule allows is logged (caller, target,
   method), and the log is reviewed in `#cm-general`.
   - The union never blocks anything that works today.
   - The existing read-side manager-chain code (`member_authority`,
     `read_scope_members`, `member_scope` in `session_roster.py`) becomes the
     shared implementation.
2. **Cut.** Drop the old rule for session operations, per host, once the
   only-old list is empty or explained.
   - Rollback is a config flag back to union.
   - Retiring task-tree scope for sessions is the irreversible S8 boundary.
     It's announced first.

## 6. What does not change

- `start_session(task_id=…)`, `create_subtask` branch mode,
  `fork_session(task_name=…)`: the row still mints the worktree and delivers
  the brief. The spawned session is placed by §3, not by the task.
- The spawner-chain walks in `capture_drain_sessions` and Claude-account
  recovery stay. They are part of the §3 placement.
- The board: items, groups, epics, holders, `place`. Holders already key on
  member (S6).
- The Planning view: it stays a backlog.

## 7. Work, in order

| # | Piece | Where | Ships |
|---|---|---|---|
| 1 | `assistants[]` in `org.snapshot`; stamping pass; drop the `by_task` fallback | API, daemon | API + brain deploy (hub first) |
| 2 | Manager-chain session scope: shared function, union mode with logging, then the cut flag | daemon `auth.rs`, `items/org.rs`; TUI mirror | brain deploy; the cut is a config change |
| 3 | Sidebar tree from org: member blocks, report nesting, leader marks, folding, continuous box subtree | `tui/src/app/tree.rs`, `draw.rs`, `orgchart_teams.rs` | laptop TUI release |
| 4 | Sections keyed by member, auto = manager's section, migration list for Owner | `tui/src/app/sections.rs`, `daemon/src/sidebar.rs` | same TUI release + brain |
| 5 | Docs: org-model §6.3/§13, sidebar-sections.md, org-chart.md, the agent guide | docs | with 3–4 |

Pieces 1 and 2 are independent of 3 and 4 and can run as parallel Codex Terra
lanes. Piece 3 depends on 1. Every piece is tested with
`scripts/cm-test-isolated` and lands on main. There is one combined deploy at
the end: hub first, then cm-sessions, then the laptop.

## 8. Owner's answers (2026-11-23)

1. **Block unit: member.** One sidebar block per member, with its checkouts
   as small groups inside it (§4).
2. **No union window.** The manager-chain rule is authoritative from the
   deploy. Step 1 of the §5 rollout is dropped; the config flag back to the
   old rule stays as the rollback.
3. **Owner's own sessions** are `agent` members directly under Owner, at the
   top level, outside sections unless assigned (§3 rule 3).

The daemon defaults to `session_scope = "manager_chain"` in `~/.cm/daemon.toml`.
To roll back session-control checks, set `session_scope = "task_tree"` and
call `daemon.reload_config` (or send SIGHUP); task-object checks continue to
use task scope in both modes. The daemon logs a caller, target, and method
when the manager rule refuses an operation the old rule would have allowed.
The local spawner chain stays live during API outages; membership, acting role
and manager relationships continue to come from the last complete snapshot.
The separate `session_roster` metadata view also keeps visibility of agents
sharing the caller's workspace, so co-tenants can see who is in the checkout;
that visibility does not grant session control.
In manager-chain mode, finishing a task does not close its sessions; retained
sessions and their live checkouts remain visible for deliberate cleanup. The
historical task-status sweep is restored by the `task_tree` rollback setting.

## Rollout

Deploy the hub `cm-manager` first, then `cm-sessions`, then `f-sessions`, and finally the laptop daemon and TUI. On each daemon, the default is `session_scope = "manager_chain"`. To roll back session-control checks, set `session_scope = "task_tree"` in `~/.cm/daemon.toml` and call `daemon.reload_config` or send SIGHUP.

Review the daemon logs for the one-time `leadless-team migration` list: these sessions need Owner review. The laptop TUI also shows a one-time **Sidebar section assignments needing review** list for migration conflicts and overrides that would split a block.

## 9. After this: tasks become board items only

Owner, 2026-11-23: sessions stop being linked to tasks at all; tasks are only
board items. This change (§1–§7) removes tasks from placement, sections, marks
and control. What is left of the link is plumbing (§6): a task row still mints
the worktree and carries the brief for `start_session(task_id=…)`,
`create_subtask` and `fork_session(task_name=…)`. The next change removes it:

- `start_session` / `fork_session` mint a member or worker checkout directly
  (`cm-ws/<member>/<slug>`), with the brief passed in the call or taken from a
  board item (`start_session(item=…)`, org-model S6).
- `create_subtask` becomes "create an item" plus an optional worker; the
  `parent_task_id` tree stays only as "part of" inside the board.
- `agent_task_edges`, task-bound workspaces and the Task sub-view retire
  (org-model S8). Existing task-bound sessions keep working; nothing is
  rebound.

It is designed after §7 ships, as its own short doc.
