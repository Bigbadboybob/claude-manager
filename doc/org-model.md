# Org model: members, teams and work

Status: **draft for review** (Swarm-Coord, 2026-10-25). Owner-directed.
Inputs: Owner's brief, three brainstorms, interviews with org leaders and
workers, and the impact map, all under
`~/.local/share/swarm-coord/org-model/`.

## 1. Summary

CM was built when a session was a task: Owner wrote a task, launched it, a
session did it and went away. Today most sessions are long-lived employees.
They lead divisions, hold board items for days, review each other and answer
questions. The planning board, the task `prompt` field, worktrees named after
tasks, and reporting lines borrowed from the task tree all still assume the
old shape.

This design separates **who** from **what**:

- **Who** is a **member**: a durable employee with a handle, a profile, a
  team and a boss. Sessions are the member's **incarnations**. Killing,
  respawning, forking off, swapping engine or model, or moving host replaces
  an incarnation; the member, its work and its history stay.
- **Where** is a **workspace**: the member's office (one checkout on one
  host). Its leader is the member; other sessions in it are the member's
  **assistants**.
- **Who works with whom** is a **team**: a lead plus members. Divisions,
  initiatives and staffed board groups are all teams. Reporting lines come
  from team membership, not from the task tree.
- **What** is **work**: one work entity with a title and a description (no
  `prompt`). Work starts in a backlog and is put on a board; boards belong to
  teams, and board groups (which nest) give work its levels. A group with a
  **point of contact** is backed by a team that reports to that member.

Everything ships in stages that each deploy alone, keep old clients working,
and run old and new rules side by side where behaviour changes (§12).

## 2. Problems with today's model

| Today | Breaks when |
|---|---|
| The employee is a session uid (`agent:<daemon_id>:<uid>`) | a respawn, a fresh replacement, a fork, an engine/model swap or a host move gives the "same person" a new uid; holds go `holder_gone`, names and DMs don't follow |
| Reporting lines are `tasks.parent_task_id` plus the lead rule | a task is work, not a person; the "boss" is whoever spawned first on the parent task; an agent can re-parent a task under itself to gain a boss (org-chart known limits) |
| The task `prompt` is separate from the description | two texts drift; `propose_task` copies the name into `prompt`; agents are told to keep them separate and often don't |
| Finishing a task ends its sessions (terminal sweep, `mark_subtask_done`, reaper) | finishing one piece of work fires a long-lived employee |
| Permission scope is the caller's task subtree | an employee that moves to new work silently changes what it may control |
| Worktrees are keyed by task (`cm-sub/<slug>-<taskid7>`) | an office has to pretend to be a task to exist |
| Board groups are free-text headings | there is no owner per group, no levels, no team behind a workstream |
| Nobody knows who knows what | agents do chat archaeology (44k messages in `#gpu-utilization`) and route everything through EP |

Two things in CM already have the right shape and are the precedent:
continuous tasks (the `ct/<slug>` channel, the `<slug>-orchestrator` name and
the queue belong to the task; sessions come and go under it) and board holders
(keyed by participant, not by task).

## 3. Vocabulary

| Term | Meaning | Replaces |
|---|---|---|
| **Member** | a durable employee: handle, title, profile, primary team, grants. Owner is a member too | "the session" as identity; the lead rule |
| **Incarnation** | one session filling a member, on some host; `primary` or `assistant` | — (a session row gains `member_id` and `role`) |
| **Assistant** | a session working for a member inside its office: reviewers, subagent-style helpers, secretaries. Not its own member | same-task workers, `detective-*` spawns |
| **Workspace** | an office: a checkout on a host, its branch and local resources, used by one member (or shared by a team) | the task↔workspace binding |
| **Team** | a lead member plus members, with optional board, channel and charter. Kinds: `division`, `team`, `initiative` | initiatives, divisions, "a root task with reports" |
| **Work** | a unit of work with a title and description; in a backlog or on a board, in a group, held by members | planning tasks *and* board items (they converge, §7) |
| **Board** | a team's plan: groups of work with holders, statuses and flags | per-initiative / per-root-task boards |
| **Group** | a node in a board's work tree; groups nest; a group may have a point of contact | the free-text `items.grp` |
| **Point of contact (PoC)** | the member who leads a group; setting one backs the group with a team led by that member | — |

Rules of thumb: a *member* has a boss and a profile; a *session* has a PTY; a
*workspace* has a checkout; a *team* has a lead; *work* has a holder.

## 4. Members and sessions

### 4.1 Identity

- A member has a stable id (`mem_<ulid>`) and a unique **handle**
  (`[a-z0-9-]{2,40}`, e.g. `ep`, `board-steward`, `bug-triage`). The handle is
  its durable chat name and is reserved, generalizing today's `-orchestrator`
  reservation.
- At most one live **primary** incarnation per member (a unique index). Any
  number of live **assistant** incarnations.
- The session uid stays the identity of one incarnation. The chat participant
  id `agent:<daemon_id>:<uid>` is unchanged; a member *references* pids and
  never replaces them.
- Every write that today records a pid (item holders, item events, receipts,
  grants, rulings, messages) also records `member_id`, stamped by the daemon.
  Readers never join through incarnation history to find who did something.

### 4.2 Which sessions are members

| How the session starts | Default |
|---|---|
| Owner creates a workspace (A-n) or launches work into a new one | new member, reporting to the team Owner picks (default: Owner) |
| An agent spawns into a **new** workspace (`isolated=true`, `create_subtask` branch mode, `fork_session(task_name=…)`) | new member ("hire") reporting to the caller's member, placed in the caller's team |
| An agent spawns into **its own** workspace (plain `start_session`, `fork_session(task_id=<own>)`) | assistant of the caller's member |
| A continuous scheduler respawn | new primary incarnation of the continuous member |
| A-R revive, Alt+s Resume, daemon restart | same session uid, so the same incarnation (nothing changes) |
| Bash sessions, workflow participants, backtest watch views | not members; assistants of the workspace's member, hidden from the org chart |

`start_session(member="new"|"self"|"<handle>", hire={handle, title, team})`
overrides the default. `member="<handle>"` fills a vacant member (a
replacement) and requires authority over it (§6.3).

### 4.3 Lifecycle

| Event | Effect |
|---|---|
| **Hire** | create the member (handle, title, team, charter, defaults); the spawn becomes its first primary |
| **Adopt** | migration only: bind an existing live session to a new member as primary; copy its holds to the member |
| **Replace** | old primary ends (`killed`, `replaced`, `engine_swap`, `model_swap`, `host_move`, `context_exhausted`); new primary starts with `predecessor`; handle, holds, team, grants, channels and member mail carry over |
| **Vacate** | primary ends with no replacement for `vacant_s` (default 24 h): member is `vacant`; its items flag `member_vacant` to the boss; routing goes to its deputy, else its boss |
| **Suspend** | parked on purpose (paused continuous, frozen lane): no vacancy flags |
| **Promote** | an assistant becomes the primary of a new member |
| **Move** | change primary team (and so boss); holds stay; profile `focus` nag |
| **Retire** | holds hand back to the boss or are dropped by rule; grants revoked; handle stays reserved as an alias for 30 days; history permanent |

A member's working memory (transcript, compaction, `agent_memory`) belongs to
the incarnation and is not copied. Continuity comes from the board, rulings,
the profile, member mail and an optional **handoff note** the outgoing primary
writes on `report_done`/kill and the successor gets in its first prompt.

### 4.4 Names and messaging

- `@ep` and `chat_send(dm="ep")` resolve to the member and are delivered to
  its current primary. A message addressed to a member is **member mail**: its
  history is readable by later primaries. A DM addressed to one session's pid
  (today's form) stays private to that incarnation.
- The current primary shows as `ep`. Assistants show as `ep/<codename>`.
  Sessions that are not members keep free codenames.
- A continuous member's handle is the task slug (`bug-triage`), with
  `bug-triage-orchestrator` kept as an alias forever. The scheduler fills the
  seat instead of renaming each instance.
- Channel membership, follows and watches belong to the member and are
  re-applied to each new primary.

## 5. Workspaces

A workspace is infrastructure: a checkout on a host, its branch, its local
venv/target/caches, and later host-local leases. It is **not** an org unit
and has no reporting line, and being in one grants nothing (no deploy rights,
no authority over its other sessions' work).

- Owned by one member (`office_of`), or by a team for shared integration
  checkouts (`team_id`, `office_of = null`).
- A member has one **home** workspace (where a replacement respawns and its
  long-lived context lives) and may open **stream workspaces**: one isolated
  checkout per concurrent work stream or item, bound to that work. Interviewed
  workers were clear that branches belong to the stream, not the person, so
  the "office" is the member's context, not a single tree.
- Assistants that only read, review or run commands share the office.
  Assistants that write code get their own stream workspace by default
  (`start_session(isolated=true)` keeps doing this), so an office never
  implies a shared dirty checkout.
- The workspace record moves into the planning DB (`workspaces` table) so it
  is cross-host and has a stable id independent of any task. It **stores** its
  branch. New workspaces get `cm-ws/<handle>-<ws7>` branches; existing
  `cm-sub/<slug>-<taskid7>` and `cm/<slug>` names keep resolving forever.
- Workspace profile is minimal: an optional one-line `purpose` (defaults to
  the owner's focus) and derived facts (repo, branch, host, path, sessions,
  owned paths from commits). Profiles that matter for routing live on members
  and teams.
- Closing a workspace is its own operation (Alt+Shift+w today). Finishing
  work never closes a workspace, except for workspaces created as
  **ephemeral** (one-shot workers, reviewers), which close when their single
  work item closes.

Why not make workspaces teams: shared resources (compute leases, budgets,
boards, channels, docs) are team-scoped and span hosts and repos; a checkout
is host- and repo-local. Keeping them apart lets a team have offices on two
hosts and lets a member move hosts without a reorg.

## 6. Teams, reporting lines and authority

### 6.1 Teams

`teams` evolves today's `initiatives` table in place (same ids, slugs,
boards and channels):

- `kind`: `division` (permanent), `team` (standing), `initiative`
  (time-bounded). Existing rows keep their kind (`initiative` or `division`).
- `parent_team_id` (teams nest; a division is usually a root),
  `lead_member_id` (nullable: a team without a lead shows its parent's lead as
  stand-in), `charter` (what the team is for), optional `board`, `channel`,
  `docs_path`, `org_id`.
- `coordinator_task_id` stays readable for compatibility but no longer decides
  who leads; it becomes nullable once every team has `lead_member_id`.
- Projects (repos) stay attributes of work and workspaces, not org units. One
  team routinely works across repos.

### 6.2 Reporting lines

Each member has exactly one **primary team**. Its boss is derived, never
stored separately:

- an ordinary member reports to its primary team's lead;
- a team's lead reports to the parent team's lead;
- a root team's lead reports to Owner.

Secondary memberships (`team_memberships.role = member | deputy | guest`)
give dotted lines and cross-team work without a second boss. Holding work on
another team's board does not change who you report to.

This removes the task tree from the org chart: `parent_task_id` keeps meaning
"this work breaks down that work" and nothing else. Re-parenting a task can no
longer change anyone's boss.

### 6.3 Authority

Reporting is not approval authority (EP's rule, kept). Two separate things:

- **Management scope** (who may control whose sessions): a member's
  incarnations may control their own assistants, and the sessions of members
  in teams they lead (transitively). `global_perms` keeps short-circuiting.
  This replaces "the caller's task subtree" (§12, dual-run first).
- **Grants** (who may approve what): explicit `grants(member, class, scope
  team|board, can_delegate, ruling_id)`. Classes are a registry
  (`source_change`, `env_change`, `deploy`, `compute`, `hire`, …). A lead can
  delegate only what it holds with `can_delegate`. Grants are the substrate for
  the approval queue and for rulings-as-grants (doc/rulings.md); v1 records
  and shows them and enforces only `hire`.

Who may hire: any member may hire reports into teams it leads, and assistants
for itself (today any tasked agent can create subtasks, so this is not new
power). Owner may restructure anything. Creating a new top-level team or
division is Owner-only.

## 7. Work, backlogs and boards

### 7.1 One work entity

Planning tasks and board items converge into one entity. Today they overlap
(both have a title, status, holder-ish fields, links, history) and the split
is the reason "take a task from the backlog to the board" is hard.

- **Fields:** `title`, `description` (markdown, the only text; no `prompt`),
  `status`, `holders` (members), `group`, `parent` (work decomposition),
  `project`, `links`, `blocked_by` / `blocked_on`, `eta`, flags, history.
  Execution details that are about a run, not the work (`worker_vm`,
  `ttyd_url`, `session_id`, `resume_metadata`) move to the session/workspace
  records.
- **The description is the work contract.** It carries scope, acceptance
  criteria and constraints; placing work on a board requires a non-empty
  description. Standing rules about *how a member works* (prohibitions,
  cleanup duties, review routing) live on the member or team
  (`standing_rules`, charter, policy docs), not copied into every work row.
  The spawn message (`start_session(prompt=…)`) stays: it is a message to a
  session, not a field of the work.
- **Structured state is kept.** Work keeps a typed `metadata` (stage,
  reviewed commit, next action, authorization, receipts) so richer state
  machines such as continuous triage's survive the move; the board renders
  what it knows and shows the rest verbatim.
- **Roles per work item.** Holders carry a role: `owner` (does it, the
  default), `contributor`, `reviewer`, `approver`. Authority runs per item
  (a reviewer may judge a fix while another member decides whether it ships);
  holding, reviewing or witnessing work never changes anyone's boss.
- **Statuses:** the board vocabulary (`open`, `active`, `waiting`, `blocked`,
  `blocked_on_owner`, `done`, `dropped`) plus `draft` and `backlog` for
  unplaced work. Planning-view `running` becomes "on a board and held".
- **Kinds that are not work:** backtests (`kind=backtest`) keep their own
  lifecycle and never go on boards. Continuous tasks become continuous
  members (§9.1); their schedule/queue records keep their ids.

### 7.2 Backlog → board

- A **backlog** is where unplaced work lives: per team, plus per project for
  work with no team yet. Owner's planning view shows backlogs; `propose_task`
  files into one (as `draft`).
- **Placing** work on a board moves it (same id, same history, a `placed`
  event); it is not copied. `place(work, board, group=…, holder=…)` from the
  planning view (`A-f` becomes "place on board", with "hire a new member for
  it" and "assign to an existing member" as options) or from MCP.
- Unplacing returns it to the backlog. A strict board (`backlog_allowed:
  false`) refuses unheld work, as today.

### 7.3 Groups and points of contact

Board groups become first-class (`groups` table): `board`, `name`,
`parent_group` (nesting gives work its levels), `order`, `description`,
optional `team`.

- A group **with a point of contact** is backed by a team: setting a PoC on a
  group creates (or links) a `kind='team'` team whose lead is the PoC and
  whose parent is the board's team. Members who hold work in the group are
  offered membership; the PoC's hires join it by default. Removing the PoC
  keeps the team (lead vacant) until Owner or the parent lead dissolves it.
- A group **without** a PoC is a plain topic heading, as today.
- **Promotion is deliberate.** Interviews showed most current groups are topic
  labels with people from several teams crossing them (e.g. "Batching &
  capacity"), while a few are real teams (Latent). So setting a PoC is an
  explicit act by Owner or the board's lead, and team membership is explicit:
  CM offers "add these holders to the team?" but never infers a boss from the
  group of someone's latest item.
- **A work item's owning team is its own field.** `team` defaults to the
  nearest staffed ancestor group, else the board's team, and can be set
  independently, so a topic group can mix work owned by different teams (EP
  asked for exactly this).
- A sub-team's work stays on the parent board as its group by default, so one
  board shows the whole plan; a team may get its own board when it grows (the
  group then links to it).
- The PoC is the group's routing target: questions, unassigned-work pushes,
  and flags on the group go to the PoC before the board orchestrator. The
  board orchestrator is the board team's lead.
- `items.grp` free text is migrated by creating one group per distinct value
  per board, with no PoC.

### 7.4 Assigning and picking up work

- Holders are members. Assigning pushes to the member's current primary,
  which may hand it to an assistant (the assistant becomes a co-holder with
  `via` = the member). A replacement primary inherits holds with no reassign.
  `holder_gone` becomes `member_vacant`.
- **Handover is explicit.** When a member hands work on, or an incarnation is
  replaced, CM lists what is attached to it: running jobs, held locks and
  leases, stream workspaces with their dirty state, open reviews, monitors.
  The successor accepts or releases each; nothing transfers silently.
- Picking up work never changes the session's permissions (scope comes from
  the member's teams, §6.3) and never changes its workspace.
- Delivering work to a session composes the brief from the work's title,
  description, group, board and links. `start_session(work=…)` replaces
  `start_session(task_id=…)` prompt auto-delivery (`prompt_source="work"`).
- Closing work unassigns holders; it never kills a session or closes a
  workspace (except ephemeral ones, §5).

## 8. Profiles

### 8.1 Content

Members and teams have profiles. Authored fields are few, short, typed and
carry provenance; everything else is computed.

| Field | Who writes it | Limit |
|---|---|---|
| `title` | lead or Owner | ≤ 60 chars |
| `mandate` (what the member/team is for) | lead or Owner | ≤ 400 chars |
| `focus` (what it is on this week) | self | ≤ 200 chars + ≤ 5 tags |
| `ask_me_about` | self | ≤ 8 tags |
| `not_my_area` (route away: `deploys → swarm-coord`) | self or lead | ≤ 5 entries |
| `deputy` | lead | a member |
| `standing_rules` | lead | ≤ 10 one-liners or a runbook path |
| `availability_note` | self | ≤ 80 chars, expires |
| current work, availability/load, engine/model, host, office, team, boss, reports, incarnations, past work, expertise, contacts | derived | — |

Each authored value is stored as `{value, set_by, via, set_at}` with history
in `profile_events` and compare-and-set writes (`expected_revision`). Edit
precedence: Owner > the lead chain above the member > the member itself (only
for its self fields).

A profile keeps four facts apart, because interviewees kept distinguishing
them and conflating them causes misrouting:

| Fact | Source | Example |
|---|---|---|
| **worked on** | evidence: closed work, merged commits | "closed sejd#1248, 31 commits under `tui/src/app/orgchart.rs`" |
| **reviewed** | evidence: reviewer holds, review posts, receipts | "reviewed 12 law fixes" |
| **owns now** | live: work held as `owner`, staffed groups led, leases | "PoC of Latent; holds #1751" |
| **may decide** | grants with scope and the ruling behind them | "`source_change` on sejd, R-sejd-41" |

### 8.2 Past work and expertise (derived)

- An append-only `evidence` table, keyed by `member_id` and incarnation pid,
  is fed by hooks that already fire: work closed (not dropped), rulings
  answered, `report_done` reasons, gate receipts, merges to main (per-path
  commit counts via the workspace's stored branch), and daily mention-edge
  counts from the messaging hub.
- **Past work** is the reverse-chronological list of closed work, merged
  branches and answered rulings, folded where they link to each other.
- **Expertise score** for a topic: weighted, time-decayed evidence (ruling
  answered 5, work closed 3, merged commit on a matching path 2, question
  routed and answered 2, inbound mention 1, self tag 1 flat). A self-claim can
  never outrank real evidence.
- Corrections count: a reverted merge, a reopened item or a superseded ruling
  is recorded against the original evidence (shown, and down-weighted), not
  deleted. Rulings carry `superseded_by` links.
- No language model on the read path. v1 matching is token overlap using the
  messaging whole-word index; embeddings per closed unit are a later option.

### 8.3 Keeping it current

The hard part, so the design keeps the authored surface tiny and checks it:

1. **Derived first.** Current work, past work, expertise, availability and
   org position are computed, so they cannot go stale.
2. **Nudge on drift, not on a schedule.** `report_done`, closing work and the
   first `ping` after compaction or resume return a one-line `profile_hint`
   only when computed drift crosses a threshold (e.g. `focus` has no support in
   the last 5 closed units). About 30 tokens, only when needed.
3. **Show staleness.** A `focus` with no supporting evidence for 7 days is
   shown greyed with "last evidence 9d ago"; an `ask_me_about` tag with no
   evidence is shown "unverified" and down-weighted. A `mandate` older than 60
   days whose work has shifted raises a `profile_drift` flag to the lead, at
   most once per two weeks, on the existing board-flag path.
4. **Owner overrides** from the org chart and A-e.
5. **Suggestions, never edits, from a cheap model** (later): stored as pending
   diffs that expire unless the member or lead accepts them.

### 8.4 Safety

Profile text is written by agents and read by other agents. Tool output wraps
it as `{"untrusted_text": …}` with a fixed docstring warning; tags are limited
to `[a-z0-9 ./_-]{1,40}`; markdown, links and imperative patterns aimed at
the reader are rejected; routing never depends on authored text alone.

## 9. How existing things map

### 9.1 Continuous tasks

A continuous task becomes a member with `kind='continuous'` whose handle is
the task slug. Its definition, schedule, queue, `ct/<slug>` channel and state
directory keep their ids (never renamed). The scheduler is its hiring manager:
each respawn is a replacement incarnation. Its ephemeral workers are
assistants; workers that hold work across runs can be hired as members of the
continuous member's team. The org chart's separate "Continuous" org becomes a
team (or a filter).

### 9.2 Initiatives and divisions

They are teams (the same rows). Each gets `lead_member_id` backfilled from
today's lead of its coordinator task. Initiative project approval
(`initiative_projects`) is unchanged.

### 9.3 Subtasks, forks, workflows, backtests

- `create_subtask` creates child **work** (a decomposition). Creating a
  worktree for it becomes a separate step: hiring a member (new workspace) or
  giving the work to an assistant (same workspace). The old one-call shape
  stays as a compatibility wrapper.
- `fork_session` forks into a new member (with `hire=`) or an assistant
  (default when forking into one's own workspace). A task is optional.
- Workflow participants are assistants of the member whose workspace runs the
  workflow.
- Backtests are unchanged (§7.1).

### 9.4 The Owner

Owner is a member (`handle='owner'`, `kind='owner'`, no incarnations), the
root of reporting lines and a valid PoC. Owner availability (F7) feeds the
derived availability. Owner is never returned by `who` unless the topic
matches a grant only Owner holds; agents reach Owner through `notify_user` or
the Owner agenda.

## 10. Discovery tools (MCP)

All read-only unless noted; all scope-filtered (§10.2).

```
org(view="me")                    # my member, team, boss chain, reports, assistants, grants
org(view="member", member=…)      # profile + chain + reports + current work + incarnations
org(view="team", team=…)          # lead, members, sub-teams, board, groups with PoCs, charter
org(view="directory", query=None, team=None, project=None)   # flat searchable list
who(topic, intent="explain"|"owner"|"approve", scope="team"|"org", available=False, limit=5)
    # explain: who knows it (worked on / reviewed); owner: who holds it now;
    # approve: who may decide or unblock (grants)
    -> [{member, handle, title, team, score, why[≤3 checkable refs], availability, load}],
       teams[{team, poc, score}], rulings[{id, text, by}]
owner_of(path=None, component=None, repo=None)               # v2: from merged commits + claims
who_decided(query=None, work=None, board=None)               # thin wrapper over rulings questions
profile_set(member=None, focus=…, ask_me_about=…, …, expected_revision, request_id)   # write
ask(topic=None, to=None, text, work=None)                    # write, v2: routes to a member or a team/PoC inbox
```

`chat_people` rows gain `member`, `title`, `team` and `focus`.

### 10.1 Ranking and evidence

`who` ranks by expertise score × availability (live idle 1.0, working 0.8,
needs_human/errored 0.3, vacant → its deputy) × a load penalty (open inbound
questions, active work), plus a structural boost for the PoC or team whose
group/mandate owns the topic. Results group by member, never by session.
Every row carries up to three refs an agent can check with existing tools
(`sejd#1248 done 2d`, `R-sejd-41 answered`, `main@c3d4e5f
tui/src/app/orgchart.rs`). That is the trust mechanism.

For code topics, `owner_of` points at the writer of the code path (from merged
commits on it), not at whoever last talked about it. Discovery finds the
person; agents still read the implementation before relying on a claim.

Success is measured by routing, not profile completeness: answered-on-first-
route rate for `ask`, re-routes, and duplicate work started on the same item
or path.

### 10.2 Scope and privacy

`who` defaults to the caller's team tree plus public channels. `scope="org"`
widens to members the caller can already see on boards or in channels.
Evidence from projects or conversations the caller cannot read is counted but
redacted ("3 units in another project"). Owner sees everything.

### 10.3 Routing questions (v2)

`ask` routes a question to the best member, or to a team inbox whose PoC/lead
triages it and may hand it to an assistant. It reuses rulings question
records, so duplicates merge and answers become evidence for the answerer.
Each member has an inbound cap (default 5 open); over the cap, `ask` routes to
the next candidate and says why. This targets the "EP as the human message
bus" problem directly.

## 11. Owner's view (TUI)

- **Org chart (F6)** is drawn from teams and members: team boxes with the lead
  on top, members under it, assistants folded under their member, vacant
  members shown empty with the stand-in. Cards show handle and title; the
  info pane shows mandate, focus (with age, greyed when stale), top expertise
  topics, current work and inbound question count. `e` edits the profile;
  `/?topic` runs `who` and highlights the matches.
- **Sessions sidebar** gains a Team sub-view (team → member → sessions,
  folding as today). The current Task sub-view stays selectable until the
  migration completes.
- **Planning view** shows backlogs per team/project. `A-f` places work on a
  board.
- **Board overlay (Alt+B)** shows groups as a folding tree with the PoC chip
  on each staffed group; `g` on a group sets its PoC.
- **A-e** on a session edits its member's profile; on a team header, the
  team's charter and lead.

## 12. Migration

Each stage deploys alone, is reversible, and keeps old clients working.
Behaviour changes run the old and new rules together and log disagreements
before cutting over.

| Stage | What | Notes |
|---|---|---|
| **S0 guards** | NULL-safe `prompt` readers (`/workers`, CLI, `worker/startup.sh`); confirm `propose_task` really files `draft` (impact map finding 6); gate daemon task→workspace binding rehydration by schema version; TUI adoption keeps taskless sessions | no behaviour change |
| **S1 prompt → description** | first an inventory: rows whose prompt holds real instructions, rows with empty descriptions, and continuous tasks whose worker contract lives in the prompt; continuous contracts move to the continuous record's standing instructions. Then the API folds `prompt` into `description` and echoes `prompt=description`; MCP keeps `prompt` as a deprecated alias (schema-compat rule); launch delivery uses description only; one marker-gated backfill appends real prompts (≠ name, not already contained) under a `## Instructions` heading | independent of the rest; ships first |
| **S2 lifecycle decoupled** | spawn no longer promotes a task to `running`; terminal sweep, `mark_subtask_done` and `worktree_reaper.py` unassign instead of killing, except ephemeral workspaces; VM teardown split from status | per-host flag; continuous/backtest unchanged |
| **S3 identity, additive** | `members`, `incarnations`, `teams` columns, `team_memberships`, `workspaces`, `profile_events`, `evidence`; optional `member_id` on manifest/session/holder/event rows (`#[serde(default)]`, shipped one release before any reader); backfill members from continuous tasks, initiative/division leads and long-lived live sessions; `lead_member_id` from today's lead rule | read-only: `org`, `ping`, A-i, F6 show members; nothing enforces |
| **S4 profiles + discovery v1** | `profile_set`, `org(view=member/team/directory)`, `who` over work, rulings and tags; staleness display; `chat_people` fields | |
| **S5 behaviour, dual-rule** | spawn defaults (§4.2); `@handle` member mail; holds by member; new workspaces store `cm-ws/…` branches; models resolve member before task; management scope = old ∪ new with disagreement logs | |
| **S6 work converges** | groups table + PoC teams; place/unplace backlog work on boards; planning tasks readable as work; `start_session(work=…)`; `create_subtask` stops minting worktrees by default (wrapper keeps the old shape) | the old Task sub-view stays |
| **S7 cut over** | scope on the new rule only; org edges only from teams; `agent_task_edges` retired; `prompt` column dropped after a release with no writers (announced MCP reconnect for the removed parameter); Task sub-view becomes a filter | |

Hazards the stages are built around (details in the impact map):

- `sql/005` re-creates the task status CHECK on every start: new statuses need
  005 edited plus a later "replace only if missing" block.
- Backfills run once behind `migrations_applied`, as single statements (every
  task row update hits the change feed).
- Persisted daemon state crosses brain versions: new fields optional and
  defaulted, readers one release later; never a manifest-schema bump that
  forces `--hard`.
- The auto-dispatch predicate (`backlog ∧ is_cloud ∧ project IS NULL`) must not
  start matching ordinary work.
- `cm-sub/*` and `cm/*` branch parsers stay forever; continuous slugs,
  channels and names are never renamed.
- Changing management scope silently widens or narrows what live
  orchestrators can drive: dual-run and log first.

## 13. Schema sketch (planning DB)

```sql
-- teams: evolve initiatives in place
ALTER TABLE initiatives ADD COLUMN IF NOT EXISTS parent_team_id UUID REFERENCES initiatives(id);
ALTER TABLE initiatives ADD COLUMN IF NOT EXISTS lead_member_id TEXT;
ALTER TABLE initiatives ADD COLUMN IF NOT EXISTS charter TEXT NOT NULL DEFAULT '';
ALTER TABLE initiatives ADD COLUMN IF NOT EXISTS org_id TEXT NOT NULL DEFAULT 'main';
-- kind CHECK grows to ('initiative','division','team'); a view `teams` aliases the table.

CREATE TABLE members (
  id TEXT PRIMARY KEY,                -- mem_<ulid>
  org_id TEXT NOT NULL DEFAULT 'main',
  handle TEXT NOT NULL, kind TEXT NOT NULL,          -- agent | continuous | owner
  status TEXT NOT NULL,               -- filled | vacant | suspended | retired
  primary_team_id UUID REFERENCES initiatives(id),
  profile JSONB NOT NULL DEFAULT '{}',  -- authored fields {value,set_by,via,set_at}
  revision INT NOT NULL DEFAULT 0,
  defaults JSONB,                     -- engine, model, effort, home workspace
  continuous_task_id UUID REFERENCES tasks(id),
  created_by TEXT, created_at TIMESTAMPTZ NOT NULL DEFAULT now(), retired_at TIMESTAMPTZ,
  UNIQUE (org_id, handle)
);
CREATE TABLE incarnations (
  id BIGSERIAL PRIMARY KEY, member_id TEXT NOT NULL REFERENCES members(id),
  role TEXT NOT NULL,                 -- primary | assistant
  pid TEXT NOT NULL UNIQUE,           -- agent:<daemon_id>:<uid>
  daemon_id TEXT NOT NULL, session_uid TEXT NOT NULL, engine TEXT, model TEXT,
  workspace_id TEXT, predecessor BIGINT REFERENCES incarnations(id),
  started_at TIMESTAMPTZ NOT NULL, ended_at TIMESTAMPTZ, end_reason TEXT
);
CREATE UNIQUE INDEX incarnations_one_primary ON incarnations (member_id)
  WHERE role = 'primary' AND ended_at IS NULL;
CREATE TABLE team_memberships (team_id UUID, member_id TEXT, role TEXT,  -- member|deputy|guest
  since TIMESTAMPTZ, until TIMESTAMPTZ, PRIMARY KEY (team_id, member_id, role));
CREATE TABLE workspaces (id TEXT PRIMARY KEY, daemon_id TEXT, path TEXT, repo TEXT,
  branch TEXT, office_of TEXT REFERENCES members(id), team_id UUID, ephemeral BOOL,
  purpose TEXT, status TEXT, created_at TIMESTAMPTZ, closed_at TIMESTAMPTZ);
CREATE TABLE groups (id UUID PRIMARY KEY, board_id UUID REFERENCES boards(id),
  parent_group_id UUID REFERENCES groups(id), name TEXT, description TEXT,
  ord INT, team_id UUID REFERENCES initiatives(id));
CREATE TABLE grants (member_id TEXT, class TEXT, scope_team UUID, scope_board UUID,
  can_delegate BOOL, granted_by TEXT, ruling_id TEXT, granted_at TIMESTAMPTZ, revoked_at TIMESTAMPTZ);
CREATE TABLE profile_events (id BIGSERIAL, subject TEXT, field TEXT, prev JSONB, new JSONB,
  actor JSONB, via TEXT, at TIMESTAMPTZ);
CREATE TABLE org_events (id BIGSERIAL, subject_type TEXT, subject_id TEXT, type TEXT,
  actor JSONB, prev JSONB, new JSONB, reason TEXT, at TIMESTAMPTZ);   -- hire, replace, move, ...
CREATE TABLE evidence (id BIGSERIAL, member_id TEXT, pid TEXT, kind TEXT, ref TEXT,
  project TEXT, topic_tokens TEXT[], paths TEXT[], weight REAL, at TIMESTAMPTZ);
-- add member_id beside every pid column: item_holders, item_events, item_receipts, ...
```

Each new table gets its own change feed (members, teams, workspaces, groups)
so viewers never poll.

## 14. Decisions made in this draft (override any)

1. The durable employee is a **member**, separate from the session; sessions
   are incarnations. (Alternatives rejected: keep the session as identity with
   successor links, which leaves every current pain; one generic node/edge
   graph, which moves the invariants into code.)
2. Reporting lines come from **teams** (one primary team per member), not a
   per-member `reports_to` and not the task tree.
3. Workspaces are **offices, not teams**; shared resources attach to teams.
4. Planning tasks and board items **converge** into one work entity, in stages.
5. A board group with a PoC **is** a team led by the PoC.
6. A new workspace means a new member; a session in your own workspace is your
   assistant.
7. Member mail (addressed to `@handle`) is readable by later primaries;
   pid-addressed DMs stay per incarnation.
8. Profiles: authored fields are few and capped; everything else is derived;
   no model writes profiles.

## 15. Non-goals

- No performance scoring or HR metaphors beyond hire/move/retire.
- No transfer of transcripts or private memory between incarnations.
- No concurrent primaries per member; no second boss (dotted lines only).
- Projects do not become org units.
- No permission enforcement beyond management scope and the `hire` grant in
  this design; the approval queue is a follow-up that builds on grants.
- Backtests are not work items.

## 16. Open questions

Tracked here until resolved.

1. **Naming.** "member" vs "seat"/"position" for the durable employee. This
   draft uses member because it also covers Owner and reads naturally in
   "team members".
2. **Fork default.** A fork into its own workspace is an assistant; with
   `hire=` it is a new member. Should a fork into a *new* workspace default to
   a hire (as drafted) or to an assistant with its own stream workspace?
3. **Member mail retention.** How far back a new primary can read member mail
   (all of it, as drafted, or a window).
4. **Assistants on the org chart.** Folded under their member (as drafted) or
   hidden entirely.

## 17. Inputs

- Brief: `~/.local/share/swarm-coord/org-model/brief.md`
- Brainstorms: `…/brainstorm/entity-model.md`, `profiles-discovery.md`,
  `impact-map.md` (file:line references for every change site)
- Interviews (Swarm-Gardener): `…/interviews/summary.md`, verbatim replies in
  `interview-log.md`
- Reviews: `…/reviews/`
