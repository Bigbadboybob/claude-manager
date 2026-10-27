# Org model: members, teams, epics and items

Status: **final draft, review round 4 folded** (Swarm-Coord, 2026-10-25).
Owner-directed; Owner has decided O1–O5 (§15). Inputs are listed in §18.
The interviews are a purposive sample (eight sessions, mostly the trader org
plus CM and scraper triage), not an org-wide vote. EP's answers are folded in;
the Codex implementation worker never replied.

## 1. Summary

CM was built when a session was a task: Owner wrote a task, a session did it
and went away. Today most sessions are long-lived employees. They lead
divisions, hold board items for days, review each other and answer
questions. The planning board, the task `prompt` field, worktrees named after
tasks, and reporting lines borrowed from the task tree all still assume the
old shape.

This design separates **who**, **where** and **what**:

- **Who** is a **member**: a durable employee with a handle, a manager, a
  team and a profile. A member works through a **main session** and,
  optionally, **assistant sessions**. Members are reused across epics because
  their sessions carry useful context; the main session is only replaced when
  it must be (context exhausted, crash with no resume, host move).
- **Where** is a **workspace**: a checkout on a host. A member has a home
  workspace (its office). Assistants are as durable as main sessions (Owner,
  2026-10-25): they belong to the office, and an assistant that writes code
  gets its own checkout in the office (a `stream` workspace) that lives as
  long as the assistant does.
- **Who works with whom** is a **team**: a lead, members, and optionally a
  channel and charter. Divisions, initiatives and epic teams are teams. Every
  member except Owner has exactly one **manager**, stored and cycle-checked.
- **What** is an **item**: work with a title and a description (no
  `prompt`). Board groups are first-class and nest. An **epic** is a group
  with a goal and an end; an epic with a **point of contact (PoC)** is run by a
  real team that reports to the PoC. When an epic finishes, its team is kept
  for the next epic, split up, or retired member by member (§7.6).

Owner-facing asks land first. v1 is S0–S5 (§13.1): prompt removal, org core
(members, managers, teams, roster), epics and groups with PoC teams, backlog →
board placement, and profiles and discovery. Changes to who may control whose
sessions come later, in shadow first.

### 1.1 Owner's terms and this design

Owner's brief described two levels: the workspace (an office with a leader
and some assistants) and the session (one agent). This design keeps that
shape and puts the durable identity on the leader, the member:

| Owner's term | This design |
|---|---|
| workspace = office, leader + assistants | a **member** (the leader) with its **home workspace** and **assistant sessions** |
| session = a specific agent | a member's **main session**, or an **assistant session** |
| workspace description | the **member profile** (+ an optional workspace `purpose`) |
| session description | derived "doing now" line: held item, last `report_done`, workspace purpose |
| tasks on the planning board | planning **tasks** in the backlog, **placed** onto boards as items |
| projects on the board | **epics**: board groups with a goal, an end and optionally a PoC team |
| groups on the board | first-class **groups**, which nest |

"Epic" rather than "project", because in CM "project" already means a repo.

## 2. Problems with today's model

| Today | Breaks when |
|---|---|
| The employee is a session uid (`agent:<daemon_id>:<uid>`) | a fresh replacement, a fork or a host move gives the "same person" a new uid; holds flag `holder_gone`, names and DMs don't follow. Revive, resume and A-R keep the uid and are fine |
| Reporting lines are `tasks.parent_task_id` plus the lead rule (Owner/agent pin, else earliest live agent) | a task is work, not a person; an unpinned lead is whoever spawned first; an agent can re-parent a task under itself to gain a boss (org-chart known limits) |
| The task `prompt` is separate from the description | two texts drift; `propose_task` copies the name into `prompt`; real worker contracts (continuous triage) live only in the prompt |
| Finishing a task ends its sessions (terminal sweep, `mark_subtask_done`) | finishing one piece of work fires a long-lived employee |
| Permission scope is the caller's task subtree | an employee that moves to new work silently changes what it may control |
| Worktrees are keyed by task (`cm-sub/<slug>-<taskid7>`) | an office has to pretend to be a task to exist |
| Board groups are free-text headings | no lead per group, no levels, no team behind a workstream, no end |
| Nobody knows who knows what | chat archaeology (44k messages in `#gpu-utilization`); EP acts as a human message bus |

Two things in CM already have the right shape and are the precedent:
continuous tasks (the `ct/<slug>` channel, `<slug>-orchestrator` name and
queue belong to the task; sessions come and go under it) and board holders
(keyed by participant, not by task).

## 3. Vocabulary

UI and docs use: **member**, **main session**, **assistant**, **workspace**,
**team**, **manager**, **item**, **board**, **group**, **epic**, **PoC**.
"Incarnation" and "occupancy" are schema terms only.

| Term | Meaning |
|---|---|
| **Member** | a durable employee: handle, title, manager, primary team, profile. Owner is a member (`owner`) |
| **Main session** | the one session acting as the member (at most one) |
| **Assistant** | a session working for a member: reviewer, helper, bench runner, `detective-*`, workflow participant. Not a member; acts under its own name "for" the member |
| **Workspace** | a checkout on a host. Kind `home` (an office), `stream` (a member's extra durable checkout) or `ephemeral` (an assistant's; closes with its work) |
| **Team** | lead + members + optional channel and charter. Kinds `division`, `team`, `initiative` |
| **Manager** | the member's one boss. Stored, cycle-checked |
| **Task** | a planning-board row: the backlog. Stays a task in v1 |
| **Item** | a unit of work on a board; either board-native or linked to a placed task |
| **Group** | a node in a board's item tree; groups nest |
| **Epic** | a group with a goal, a status and an end; optionally a PoC and a team |
| **PoC** | an epic's lead; the lead of the epic's team |

An **initiative** is a team with its own board (cross-repo, longer-lived); an
**epic** is a group *within* a board, run by a team.

## 4. Members and sessions

### 4.1 Identity

- A member has a stable id (`mem_<ulid>`) and a **handle** unique in its org
  (`[a-z0-9-]{2,40}`, e.g. `ep`, `board-steward`, `bug-triage`). Stable
  references are org-qualified (`main/ep`) or the member id; a bare handle
  resolves only within the caller's org and errors on ambiguity. Retired
  handles are never reused (permanent tombstone alias).
- The session uid stays the identity of a session, and the chat participant
  id `agent:<daemon_id>:<uid>` is unchanged. A member *references* pids and
  never replaces them.
- **Occupancy** (schema: `incarnations`): one row per (member, session uid,
  role), with a monotonic `occupancy_generation` per member (distinct from the
  existing transcript `generation` field in manifests). A same-uid model or effort
  change, revive or resume stays the same row with a change event. At most one
  open `main` row per member.
- Every write that records a pid today also records `member_id` and
  `acting_as = main | assistant` (§6.4), stamped by the daemon. Readers never
  join through occupancy history to find who did something.

### 4.2 Which sessions are members (Owner decision O4)

Members are created deliberately and rarely. Everything else is an assistant.

| How the session starts | Result |
|---|---|
| Owner A-n, or planning "place → hire new member" | new member; Owner picks manager and team (default: Owner, no team) |
| `start_session(hire={handle, title, team?})`, `fork_session(hire=…)` | new member; manager = the caller's member; primary team = the given team if the caller leads it, else none |
| Promote an assistant | the assistant becomes the main session of a new member managed by the old member |
| `start_session(member="<handle>")` | fills a vacant member as its main session (a replacement); needs that member's manager chain or Owner |
| Continuous scheduler respawn | new main session of the continuous member (§9.1) |
| Revive, A-R, Alt+s Resume, daemon/brain restart | same uid, same occupancy row; nothing changes |
| Any other agent spawn: plain `start_session`, `isolated=true`, `create_subtask` + spawn, `fork_session` without `hire`, `item_resolve(launch)`, `start_session(task_id=…)` | assistant of the caller's member, durable like a main session (§4.3). `isolated=true` (or `create_subtask` branch mode) gives it its own checkout, recorded as a `stream` workspace of the member's office. A spawn by a session that belongs to no member keeps today's **ephemeral** workspace |
| Owner A-s (add a fresh agent session to a workspace) | auto-placed as a new member under Owner; shell, continuous and workflow rows remain excluded |
| Owner A-F fork | assistant of the source's member unless Owner ticks "new member" |
| Any agent spawn by a continuous member's session (its orchestrator, or anything that orchestrator spawned) | a **worker** of that member: stamped for attribution only, never durable. `mark_subtask_done`, the terminal sweep and the reaper treat it exactly as today, and its checkout is never promoted to `stream` (Swarm-Coord, 2026-10-25: continuous hosts run near their worktree cap and triage close-out reclaims checkouts) |
| A fresh top-level agent session spawned by the TUI/operator (no spawner) | automatically a new `agent` member under Owner; Owner may move it later. Placement is durable across an API outage and does not apply to restores/resumes |
| A restored/resumed session with no spawner | not a new hire; its existing member/assistant identity is preserved |
| Bash sessions, backtest watch views, cloud dispatch workers | not members and never assistants. Bash/watch views belong to the workspace's member for display only, and only Owner closes, kills or restarts a shell. Cloud workers stay as today (§9.4) |

Assistants are not temporary. Finishing an item or a task does not end an
assistant: it stays in its member's office with its context and is the first
choice for the member's next piece of work (reuse before spawning). An
assistant ends only when it is deliberately closed (§4.3, **Close an
assistant**). An assistant can also be promoted to its own member; CM
suggests it for an assistant alive 3 days that holds items as owner. Today's long-lived lanes
(msgfix, org, items, …) are members from the S2 roster.

### 4.3 Lifecycle

| Event | Effect |
|---|---|
| **Hire** | create the member; the spawn becomes its main session |
| **Reassign** | the member moves to other work (a new epic, a new team); its main session keeps running with its context. This is the normal end of an epic (§7.6) |
| **Replace** | only when the main session cannot continue (context exhausted, unrecoverable crash, host move, Owner asks for a fresh start): the main row closes with a reason; a new main row opens with `predecessor` and `occupancy_generation + 1`; handover runs (§7.5) |
| **Fencing** | a session whose main row was closed by a replacement can no longer act as the member: member-scoped writes from it are refused (`not_main_session`), checked by occupancy generation at the API. If revived, it comes back as an assistant, with a notice |
| **Vacate** | the main session is dead and unreplaced for 24 h: the member is `vacant` and its items flag `member_vacant` to its manager; routing goes to its deputy, else its manager. The main row stays open; reviving that session clears the vacancy |
| **Suspend** | parked on purpose, or vacant 14 days with no holds (automatic): no flags, hidden from `who` and the directory |
| **Move** | change primary team and/or manager; Owner or the current manager chain; holds stay. A non-Owner may only move a report under itself or under a member of its own subtree; any other destination is Owner's (`needs_owner`) until an accept step exists |
| **Close an assistant** | deliberate: its member's main session, the member's manager chain, or Owner (`org(action="close_assistant")`, Owner's A-w). An agent's `kill_session` on an assistant has the same effect without that check, since `kill_session`'s own scope applies. Its holds hand back to its member; its `stream` checkout closes with it once the reaper's protections pass. Task/item completion, `mark_subtask_done`, the terminal sweep and Owner's A-D on its task never close an assistant. If it dies any other way (an operator kill, a crash or memory cap, a natural exit), its stream checkout goes back to normal reaper eligibility after 7 days with no live assistant in it; a revive within that window keeps it |
| **Retire** | Owner or the member's manager chain, with a reason. Reports move to the manager; teams it leads lose their lead; holds hand back to the manager through #21's `hand_back_holds`. Its sessions are untouched (Owner, O5: sessions are durable): the main session keeps running and its main row stays open, so nothing is killed. Handle tombstoned; history permanent. Owner can reinstate: the pre-retire status comes back, plus the leads of teams still without one; moved reports stay. Closing a member's home workspace asks "retire member X?" (default yes when it holds no open items) |

Only Replace closes a main row. Working memory (transcript,
compaction, `agent_memory`) belongs to the session and is not copied.
Continuity on a replacement comes from the board, the profile, the standing
rules (§8.4) and a **handoff record**: the durable checklist of §7.5, plus an
optional note from the outgoing session. The checklist is the baseline; the
note is an optimisation, because a killed session cannot write one.

### 4.4 Names and messaging (v1)

- `@ep` and `chat_send(dm="ep")` deliver to the member's current main session,
  creating or reusing an ordinary DM with that session's pid. No message
  history moves between sessions in v1.
- Qualified body mentions (`@main/ep`) need a tokenizer change (today `/` ends
  the token); until it ships (S6), qualified references are accepted only in
  explicit `mentions=[…]` and `dm=` arguments.
- The handle → main-session mapping lives in the messaging layer, written only
  by the API and replicated to every daemon like names are today. When the API
  is unreachable, `@handle` resolves to the last known main session.
- Assistants show under their own codename with a suffix: `rev1 (for ep)`.
  `@ep` never resolves to an assistant.
- A continuous member's handle is a normalized form of its task id
  (`bug-triage`), with `bug-triage-orchestrator` kept as an alias forever.
- Channel memberships and follows belong to the member and are re-applied to
  each new main session.
- **Later (not v1):** member mail (§16).

### 4.5 Staffing policy (which engine to hire)

Owner's guidance on which engine to use changes with the work and with
account limits ("we've been spawning lots of Claudes, but Codex limits are
higher now"), and agents forget it unless it reaches them when they spawn. It
is a small, standalone feature with no dependency on the rest of this design,
so it can ship first; hires (§4.2) use it once members exist.

- **One Owner-editable policy per org** (central, with a version), containing:
  - **rules by kind of work**, starting with Owner's: Codex for math, design,
    architecture-heavy work and maximizing specific metrics; Claude for
    large-scale coding and work that fans out to many agents (better subagent
    management); each rule may also name a model. Busywork uses Codex Luna
    (`gpt-6-luna`); legacy engine-only rules remain valid;
  - a **default engine** for work that matches no rule;
  - an optional **current preference** with a reason and optional expiry.
    Omitted/null expiry means indefinite (Owner: "for the foreseeable future").
- **Precedence**, stated in every `staffing_note`: an explicit `type` or
  `model` from the caller wins (the policy only comments); then the current
  preference if its strength is `strong`; then the matching rule; then a
  `lean` preference; then the default. Owner's "Codex limits are higher" is a
  `strong` preference: it tips even fan-out coding to Codex until it expires
  or Owner changes it. Model order is explicit spawn model, task default
  (including inherited defaults), compatible work-rule model, host default.
  A task default overriding the rule returns a `staffing_note`.
- **Delivery, so nobody has to remember:**
  - `start_session`, `fork_session` and `hire` return a one-line
    `staffing_note` when the chosen engine goes against the policy or the
    current preference;
  - `start_session` may omit `type`; the policy picks the engine from an
    optional `work_kind` hint (`math`, `design`, `architecture`, `metric`,
    `coding`, `fanout`, `busywork`), else the default. A-n, A-s and the A-f launch dialogs
    preselect the cached policy's engine when opened;
  - daemons cache the policy, so a spawn never waits on the API: they use the
    last policy seen, else today's default, and `ping` serves it from the
    daemon (headless continuous orchestrators included);
  - `ping` returns the policy digest and version; a change pushes a one-line
    notice to sessions that spawned anything in the last 3 days;
  - `doc/ORCHESTRATION.md` §1, the agent guide and the `orchestrate-swarm`
    skill point at the live policy instead of restating it.
- **Owner edits it** in a Staffing panel in F9 Global Settings (rules, default,
  per-rule models, preference with reason and optional expiry) or with `cm-op staffing.set`.
- **Later:** CM suggests a preference from account usage ("Claude is at 85% of
  its weekly limit; shift to Codex?"), and a hire records the engine, model and
  reason in its `org_event`, so the policy can favour reusing a member's
  existing session over starting a new one.

## 5. Workspaces

A workspace is infrastructure: a checkout on a host, its branch, its local
venv/target/caches. It is not an org unit, has no reporting line, and being in
one grants nothing (no deploy rights, no authority over other sessions there).

- **Kinds.** `home` (a member's office; a replacement respawns there),
  `stream` (a member's extra durable checkout, including each code-writing
  assistant's own checkout; it closes only when its assistant is closed or
  the member retires), `ephemeral` (a session that belongs to no member;
  closes after its work closes, the session ends, and the reaper's existing
  protections pass: no live process, not dirty, evidence released).
- The kind is stored in the daemon's local workspace record and manifest from
  S0b. Existing workspaces start **unclassified**, which behaves exactly as
  today. From S0b, workspaces minted for `isolated=true` / branch-mode spawns
  are recorded `ephemeral` at mint (an explicit fact about that spawn, not a
  guess). The S2 roster (and later adoptions) mark members' workspaces `home`,
  centrally and on the owning daemon; Owner can mark any
  workspace durable (pinning already exists and counts). Existing workspaces
  are never classified from a branch prefix or spawn origin.
- Owned by a member (`office_of`), or by a team for shared integration
  checkouts.
- Assistants that only read, review or run commands may share the office;
  assistants that write code get their own `stream` checkout in the office by
  default, so an office never implies a shared dirty checkout.
- **Records and branches (S6).** New member workspaces get a central record
  and a local durable record, both storing id, branch, base SHA, host and path.
  Creation reserves the id under an idempotency key before any filesystem or
  spawn work and moves `creating → ready | failed`. Creating a member workspace
  needs the API; assistants' checkouts keep today's task-derived
  path and need nothing new. New member workspaces use `cm-ws/<handle>-<ws7>`
  branches; the stored branch wins over inference, a handle edit never renames
  a branch, and the `cm-sub/` and `cm/` parsers stay forever.
- Profile: an optional one-line `purpose` plus derived facts (repo, branch,
  host, path, sessions, dirty state).

## 6. Teams, managers and authority

### 6.1 Teams

`teams` evolves today's `initiatives` table in place (same ids, slugs,
boards and channels).

- `kind`: `division` (permanent), `team` (standing; epic teams are this kind),
  `initiative` (time-bounded). Existing rows keep their kind.
- `parent_team_id` (acyclic, checked on write), `lead_member_id` (nullable: a
  leadless team shows its parent's lead as stand-in), `charter`,
  `standing_rules`, optional `board`, `channel`, `docs_path`, `org_id`,
  `status` (`active`, `standby`, `archived`).
- `coordinator_task_id` becomes nullable in S2, before the first taskless team
  exists, together with the API create/response model changes; old clients
  keep receiving the stored id where there is one.
- A team outlives its epics: it can run several epics at once, move to the next
  one, or wait on standby (§7.6).
- Projects (repos) stay attributes of tasks, items and workspaces, not org units.

### 6.2 Managers and reporting lines

Each member except Owner has exactly one `manager_member_id`. The boss is the
manager. It is stored rather than derived from teams because members lead
several teams, PoCs are set on other teams' boards, and a derived rule
produces zero, two or cyclic bosses in those cases.

- **Invariants:** no cycles (checked in the same transaction as the write);
  Owner is the root; a manager is a non-retired member; a vacant manager has a
  stand-in up the chain.
- **Defaults:** a hire's manager is the hirer's member. Joining a team as a
  primary member sets manager = that team's lead unless told otherwise.
  Leading a team never changes your own manager.
- **Teams and managers agree by default, not by force.** The org chart groups
  by team and draws manager lines; a member whose manager is not its primary
  team's lead gets a dotted marker (matrix cases).
- **Who may change it:** Owner; the member's current manager chain; a lead
  accepting a member offered by that member's manager chain. Every change is an
  `org_event` with a reason.
- `parent_task_id` keeps meaning "this task breaks down that task" and nothing
  else. Re-parenting work can no longer change anyone's boss.
- **What reporting means, by stage:** from S2 the manager tree is shown (F6,
  `org`) and used for routing, vacancy flags and profiles. It gains
  session-control power only at S7 (§6.3).

### 6.3 Management scope (who may control whose sessions)

Target rule:

- A member's main session may control its own assistants and the sessions of
  its reports (transitively down the manager chain).
- An assistant may control only the sessions it spawned itself.
- Team membership (primary, secondary, guest) never extends scope; only the
  manager chain does. A non-lead's scope therefore shrinks to its own
  assistants and stops accumulating with every item it touches, which answers
  the interviewees' "authority per item, not per role" concern.
- `global_perms` keeps short-circuiting, and never crosses an org boundary.

Rollout:

- **S5 shadow:** the old task-subtree rule is authoritative; the new rule is
  computed beside it and every disagreement is logged. Purely observational.
- **S6 bootstrap rules:** S6 creates sessions the old rule handles badly (a
  hire's sessions in a taskless workspace). These get named, feature-gated
  allow rules: `hirer_controls_hire` and `main_controls_own_assistants`. They
  are listed in the log and are the only enforcement change before S7.
- **S7 cutover, per host:** after a week of shadow logs with only expected
  differences, the new rule becomes authoritative. Daemons cache a
  manager-chain snapshot with the bindings and refresh it every 5 minutes.
  During an API outage they are **fail-static**: relations in the last
  snapshot keep working until the API answers; only org changes (hire, move,
  PoC) and relations created after the snapshot are refused. Central writes
  are fenced immediately by occupancy generation; local session control is
  fenced at the next snapshot refresh.
- **Rollback from S7** is to *shadow with the bootstrap rules kept*, not to the
  bare old rule, because S6-era sessions exist that the old rule cannot place.
  `agent_task_edges` and task bindings are retained through S7; retiring them
  (S8) is the irreversible boundary, announced in advance.

### 6.4 Permission matrix

Who acts is three separate things: the **signer** (the pid that made the
call), **attribution** (whose record and evidence it is), and
**authorization** (what check allowed it).

| Action | Main session | Assistant | Manager chain | Team lead | Board orchestrator / steward | Owner |
|---|---|---|---|---|---|---|
| control sessions | own assistants + reports' sessions (target rule) | own spawns | yes (via main session) | only if also the manager | — | yes |
| hold / update an item as the member | yes | only items delegated to it (`via`), for work transitions and receipts | — | — | — | yes |
| rule / approve as the member | yes, display and attribution only in v1 | **never** | — | — | — | yes |
| create / edit / place items on a board | board team members; holders on their items | as its member, on delegated items | — | yes | board-wide | yes |
| edit board settings | — | no | — | own team's board | yes | yes |
| set an epic's PoC | — | no | — | on own team's board | no | yes |
| hire | yes (manager = self) | no | — | into own team | no | yes |
| move / retire a member | — | no | yes | accept only | no | yes |

- `via` authorizes execution on delegated work only; it never confers approval
  capability. An assistant's writes are attributed to its own pid and shown
  "on behalf of ep"; work transitions and receipts on delegated items also
  credit the member at reduced weight.
- Board permissions are enforced centrally (the API), so there is no per-host
  skew. Today's "holding any item on a board grants board-wide edit" stays
  authoritative until S7; the new rule runs in shadow beside it.

### 6.5 Approval authority

Reporting is not approval authority (EP's rule, kept). v1 adds no grants
registry and no new approval enforcement. "May decide" in v1 shows:

- existing content-bound gate grants and waivers (`gate_db`), with their
  grantor;
- Owner rulings;
- the item `approver` role, shown as a **decision contact** marked
  `authority_unverified` unless one of the two above backs it. It is an
  assignment to make a decision, not a permission.

Gate authority follows members from S6: grantor and gate-editor allowlists
key on member id (the historical pid is kept on each grant), so a replacement
main session can issue grants and revoke its predecessor's. Existing grants
keep their content binding; retirement leaves them valid until expiry, and the
manager chain or Owner may revoke them.

A grants table with delegation lineage is a follow-up with the approval
queue, seeded explicitly by Owner (e.g. EP = prod GO, SEJD = law review).

## 7. Tasks, items, epics and boards

### 7.1 Link-first placement (v1)

Tasks and items stay separate tables in v1 (O3, §15). The backlog **is** the
planning task list, as today.

- `propose_task` and `create_subtask` keep creating tasks (`propose_task` into
  the backlog, as today). Every existing task tool keeps working.
- **Placing** a task on a board (from the planning view or `place(task, board,
  group=…, holder=…)`) creates an item linked to it (`items.task_id`; at most
  one *live* link per task). Re-placing on the same board reactivates the
  archived link; placing on a different board creates a new item and leaves
  the old one archived and resolvable. Placing a task that is
  auto-dispatchable (`is_cloud ∧ project IS NULL`), a backtest or a continuous
  task row is refused. Placing a `draft` task (one Owner created as a draft) accepts it, as A-a does.
- **Unplacing** archives the link and sets the task back to `backlog`. It is
  refused while the item has live incoming or outgoing `blocked_by` edges
  (clear or re-point them first; blockers never silently become links). An
  archived item is read-only: its number resolves for history, and writes
  through it are refused, so a stale caller can't revive the old assignment.
  "Unplace and re-place on epic F" is one step.
- Board-native items (the `item` tool) stay as today, with no task. They
  cannot be unplaced.
- Moving items between boards, backlog boards and item aliases are not in v1
  (§16).

**Field ownership while a task is placed:**

| Field | Owner | Notes |
|---|---|---|
| title, description, metadata, project, `parent_task_id`, execution defaults (models, worktree mode) | **task** | the item shows them; editing on the board writes the task. One copy, so legacy `get_task`, `start_session(task_id=…)` delivery and continuous metadata PATCHes keep reading and writing the same thing |
| status, holders and roles, group/epic, owning team, `blocked_by`, ETA, flags | **item** | the task's `status` mirrors the item (table below) |

**Status mirror** (written in the same API transaction as the item change):

| Item | Task status |
|---|---|
| `open` (unheld) | `backlog` |
| `active`, `waiting` | `running` |
| `blocked`, `blocked_on_owner` | `blocked` |
| `done` | `done` |
| `dropped` | `done` (with `metadata.dropped = true`) |
| link archived (unplaced) | `backlog` |

**Legacy status writes to a placed task** are translated, not refused, and
attributed to the session that caused them (not the daemon's API token):

| Legacy write | Item transition |
|---|---|
| unchanged status (the old editor always sends one) | ignored |
| `done` | item `done` |
| `blocked` with a reason or blocker (new optional `reason` / `blocked_by` params on `set_subtask_status` and `update_task`) | item `blocked` with that trigger |
| `blocked` alone (today's fix-ready call) | item `blocked` with `blocked_on = "legacy status write: no reason given"`, `check_back` = 24 h (strict boards require one; the label says it is a default), and a `legacy_blocked` flag to the holder to fill in the real trigger. No trigger is invented |
| `backlog` | unplace (subject to the unplace rules) |
| `running` (incl. spawn promotion from `start_session(task_id=…)`) | item `active`; if unheld, the spawned session (pre-S6) or its member becomes owner; if held, it becomes a contributor |

Only writes with no item equivalent are refused, with `placed_on_board` and
the replacement call.

**Finishing work never kills an employee.** Two protections, both in S3:

- **Board-originated closes never arm the terminal sweep.** The mirror writes
  `done` with `metadata.terminal_via = "item"`, and the sweep skips
  non-ephemeral sessions for such tasks, whatever their workspace kind.
- **Every teardown path protects members.** The periodic sweep *and* explicit
  teardown (`mark_subtask_done`'s `sweep_task_sessions` and its branch-mode
  worktree removal, which run before the status PATCH) skip a member's main
  session and never remove a `home`, `stream` or pinned workspace, for both
  task- and item-originated completion. They only unassign. Ephemeral and
  unclassified workspaces keep today's teardown. Retiring a member and closing
  its workspace stay separate, explicit operations.

**Lock order.** Every transaction that may write tasks or initiatives (place,
unplace, mirrored status, board edits that write task text, team and epic
changes) takes the task-change advisory lock first (`_serialize_task_writes`),
then board, task and initiative row locks, as `dispatch/db.py` already
requires.

### 7.2 What an item carries

- Title and description (the work contract: scope, acceptance criteria,
  constraints). For a placed task they are the task's (§7.1); placing requires
  a non-empty description. Description edits after placement bump a revision;
  holders get a "description changed by X" notice instead of silently new
  instructions on the next delivery.
- Typed `metadata`. For placed tasks it is the task's metadata (continuous
  triage's stage, reviewed commit, next action and authorization stay where
  they are). Board-native items get their own.
- Status: today's board vocabulary.
- Holders with a role: `owner` (does it; the coordination owner interviewees
  asked for), `contributor`, `reviewer`, `approver`. Exactly one `owner` while
  the item is `active`, `waiting` or `blocked` and held; zero is fine for
  `open`, unheld, closed or unplaced work. Hand-back removes the owner and
  otherwise keeps today's rules: on a strict board the item stays `active`,
  unheld, and flags `unlaunched`; elsewhere it goes `open`. Transferring
  ownership is one atomic write. Holding, reviewing or
  witnessing never changes anyone's manager.
- `group` (and so its epic, §7.3) and `team` (the owning team: defaults to the
  epic's team, else the board's team; settable independently, as EP asked, so
  a topic group can mix teams).
- Backtests (`kind=backtest`) are never placed on boards; their status,
  done-PATCH VM teardown, preemption requeue and `archived` lifecycle are
  unchanged (§9.4).

**Migrating existing holders:** an item with one holder gets role `owner`.
An item with several holders keeps them all as `holder` (a legacy role that
counts as co-owner) and gets a one-time steward flag to pick the coordinator;
the single-owner rule applies only to writes after migration.

### 7.3 Groups, epics and points of contact

Board groups become first-class (`groups`: board, name, parent group, order,
description, `kind`).

- `kind='heading'`: a plain topic heading, as today. Nests.
- `kind='epic'`: a body of work with a goal, acceptance criteria, a `status`
  (`active`, `done`, `cancelled`), an optional PoC and a team. Epics can
  contain headings and sub-epics, which gives work its levels.
- **An epic with a PoC is run by a real team** (Owner, O2): setting a PoC
  creates a `kind='team'` team led by the PoC, or links an existing team the
  PoC leads (for example a team coming off its last epic). The team's parent is
  the team of the nearest enclosing epic, else the board's team. Members are
  staffed onto it: hired by the PoC, assigned by Owner, or moved from other
  teams with their manager chain's consent. Primary members of the team report
  to the PoC.
- **Staffing is explicit.** Most of today's groups are topic labels with people
  from several teams crossing them, so CM never infers membership or managers
  from who holds an item. When a PoC is set, CM shows the epic's current
  holders and offers each one as a primary member (a move, needing that
  member's manager chain or Owner) or a guest (works on the epic, keeps its
  manager). Guests get item roles and the team channel, never management.
- **Routing for an item:** its approver-role holder (for decisions) → its
  owning team's lead → the epic's PoC → the board team's lead. Unassigned work
  and epic-level flags go to the PoC first.
- A PoC who sits outside the board team's tree, or who would create a cycle,
  needs Owner or that PoC's manager chain.
- An epic's items stay on the parent board by default, so one board shows the
  whole plan.
- Migration: `items.grp` free text becomes one `heading` group per distinct
  value per board. Promoting a heading to an epic and setting its PoC are
  deliberate acts by Owner or the board team's lead.

> **Interview evidence.** Of eight respondents to the group question, all
> described today's groups as topic labels or filing headings, and several
> preferred a PoC that only routes. Owner has ruled that a PoC leads a real
> team (O2). The design keeps both true by making epics explicit (headings stay
> headings), staffing explicit, guests possible, and an item's owning team
> independent of its group. Cases from the interviews: Latent (a natural epic
> team under Latent-Forecast-Lead, with external law reviewers as guests),
> "Batching & capacity" (a heading with holders from several teams),
> Batch-Loop (answers to Backfill-Lead on #1751 and to EP on #1797; it can be a
> guest on one and a member of the other).

### 7.4 Assigning and delivering work

- Holders are pids until S6, then members. Assigning pushes to the member's
  main session, which may delegate to an assistant (`via` the member). From S6
  a replacement main session inherits holds and pending pushes retarget to it;
  `holder_gone` becomes `member_vacant`.
- The active-item cap of strict boards counts per acting session (main or
  assistant), not per member.
- **Launch permission for owners (S3).** The owner-role holder of a placed
  item may `start_session(task_id=…)` / `item_resolve(launch)` an assistant for
  that task even when today's scope rule would refuse (the caller is taskless,
  or the task sits in another tree). This is a separate, revocable
  `launch_grants` entry (holder session or member → task), written by the API
  when the owner changes, persisted by the daemon, and checked explicitly in
  the launch authorization path, including for taskless callers. It is not a
  task-parent or creator edge, so a TUI tree push or restart does not drop it.
  It is logged like the §6.3 bootstrap rules and grants launch only, not
  control of other sessions on the task.
- Delivering an item to a session composes the brief from the item's title,
  description, epic and goal, board and links; the team's charter link; and the
  member's and team's standing rules (§8.4). `start_session(task_id=…)` keeps
  working and delivers the same brief; `start_session(item=…)` is added in S6.
  The spawn message (`start_session(prompt=…)`) stays: it is a message to a
  session, not a field of the work.
- Closing an item unassigns its holders. It never kills a main session or
  closes a home or stream workspace. Ephemeral workspaces close per §5.
- When an item is created or claimed, or a workspace opened for work, CM returns
  a `possible_duplicate` hint for open items with overlapping title tokens or
  paths.

### 7.5 Handover

Replacement and reassignment run through a durable, idempotent checklist:
`replacing → reconciling → ready`.

- The checklist lists what CM knows is attached: held items, pending pushes,
  open reviews, monitors, declared jobs (`items.job`), stream and ephemeral
  workspaces with their dirty state, and (later) leases. Ad-hoc locks and
  external resources CM cannot see (a bb-l4 `cell.lock`, a VM) must be declared
  on the item to be listed; unknown resources are shown as "not tracked".
- The incoming main session (or, on reassignment, the new holder) accepts or
  releases each entry. Until it does, those items show `handover` on the board
  and route to the manager. Nothing transfers silently.

### 7.6 Finishing an epic

An epic ends when its PoC, the board team's lead or Owner closes it (`done`
or `cancelled`). Closing builds a **close plan**, authorizes it, then applies
it. The plan is shown to the closer, posted to the team channel, and
persisted, so it is resumable and idempotent: each step records the expected
revision of what it changes and is re-checked before it runs.

**Preconditions.** No active sub-epics (close them first, or include them in
the same plan). The final "nothing left open" check runs under the board lock
that item writers use.

**1. Open items.** Each open item is moved to another epic or heading on the
board, unplaced back to the backlog, unplaced and re-placed on an epic
elsewhere (placed tasks), or dropped with a reason. Bulk actions apply to all
of them at once ("move all to heading X", "unplace all"). Items with live
`blocked_by` edges move together with their blockers or are re-pointed first.

**2. Team and members (one table).** A row for the team and one per member:

| Row | Choices | Default |
|---|---|---|
| team | **keep and reassign** to another epic · **standby** (no epic; lead flagged after 7 days) · **archive** (once no members remain) | keep |
| each primary member | **keep** · **move** (to another team or manager) · **retire** | keep (the session and its context stay) |
| each guest | **end guest membership** · keep as guest on the team | end |
| each assistant | **keep** (it still holds an item that was carried forward, has a declared job, or has a dirty workspace) · **end** | keep if it has remaining work, else end |

- A team that still runs other active epics only **detaches** from this one;
  standby and archive are offered only when this was its last epic, and the
  team's channel, staffing and parent are untouched.
- Reassigning to an epic in the same team tree is free. Reassigning into a
  different team tree (another board's team, another division) changes the
  team's parent and is a move for the lead: Owner or the lead's manager chain,
  with an offer to change the lead's manager to match.
- Every choice needs exactly the authority it needs outside a close (§6.2,
  §6.4). Choices the closer cannot authorize become **pending**: the epic
  still closes, the affected members stay on the team until the manager chain
  or Owner accepts or withdraws, and the closer is told who must decide.
  Moves into another lead's team are pending until that lead accepts.
- **Retire during a close needs the member's manager chain or Owner, never
  just the closer**, because it is the one choice that throws away a
  session's context (O1).
- Assistants are ended only after their remaining assignments are checked
  again at apply time; one that picked up carried-forward work in the meantime
  is kept.

**3. Workspaces.** Stream workspaces opened for the epic are offered for
closing once their branches are merged or archived; home workspaces stay;
ephemeral workspaces close per §5.

**4. Record.** The epic becomes one past-work unit on every member who held an
item in it (with their roles) and on the team's profile. The epic's own
discussion stays in the team channel, which is named after the team, not the
epic, so it still makes sense when the team is reassigned. A team that is
archived archives its channel.

`org(view="team")` and F6 show a team's current and past epics, so "who has
done this kind of epic before" is answerable.

## 8. Profiles

### 8.1 Four facts kept apart

| Fact | Source | Example |
|---|---|---|
| **worked on** | evidence: items closed as owner, epics, commits authored | "closed sejd#1248; epic Latent v2; 31 commits under `tui/src/app/orgchart.rs`" |
| **reviewed** | evidence: reviewer holds, receipts | "reviewed 12 law fixes" |
| **owns now** | live: items held as owner, epics and teams led, declared jobs, OWNER.md files, VM labels | "PoC of Latent; holds #1751" |
| **may decide** | §6.5 sources, each with its source and supersession | "gate grant `source_change` on sejd; approver on #1797 (unverified)" |

### 8.2 Fields

| Field | Written by | Limit |
|---|---|---|
| `title` | manager chain or Owner | ≤ 60 chars |
| `mandate` (what the member is for) | manager chain or Owner | ≤ 400 chars |
| `focus` | **derived**: owner-role holds, epics, recent `report_done` reasons, workspace purposes | — |
| `focus_note` | self; expires after 7 days. The manager chain or Owner may pin it instead (`focus_pinned`), which wins over the derived focus | ≤ 120 chars |
| `on_call` | manager chain or Owner (with a window); self may only clear it | — |
| `ask_me_about` | self; checked against evidence | ≤ 8 tags |
| `not_my_area` (`deploys → swarm-coord`) | self or manager | ≤ 5 entries |
| `deputy` | manager | a member |
| `availability_note` | self; expires | ≤ 80 chars |
| everything else (manager, team, reports, assistants, workspaces, host and how to reach it, engine/model, past work, expertise, contacts) | derived | — |

Seven of eight respondents, EP included, wanted role and focus set by the
lead, not the agent; deriving `focus`, with a manager pin, settles that. Authored values are stored as `{value,
set_by, via, set_at}` with history in `profile_events` and compare-and-set
writes. Team profiles: `charter` and `standing_rules` (lead/Owner), lead, and
derived members, epics (current and past), open items and evidence.
Workspace profiles: `purpose` only.

### 8.3 Evidence and past work

Evidence rows are keyed by an immutable source key (item event id, commit
SHA + repo, receipt id, epic id, ruling id) plus the attribution role, so
ingestion is idempotent.

| Producer | Status | Stage |
|---|---|---|
| items closed / handed off; reviewer and approver holds | exists (`item_events`, `closed_by`); credit the owner-role holder, not whoever clicked done | S5 |
| epics closed | new with epics | S3/S5 |
| gate receipts | exists (`item_receipts`) | S5 |
| `report_done` reasons | exists (tombstones); daemon → API forwarder to build | S5 |
| commits authored | **to build**: a `prepare-commit-msg` hook (chained like CM's post-checkout hook) adds `CM-Member:` / `CM-Session:` trailers; a hub-side `git log` cursor per repo ingests them. Credits authors, not landers. Evidence starts at hook install; an optional low-confidence backfill from branch → workspace is allowed | hook S1, ingestion S5 |
| rulings answered / superseded | **external prerequisite** (doc/rulings.md has no code yet) | when rulings ship |
| OWNER.md files, VM labels | **to build**, owns-now only: a per-host scanner, a minimal parse contract (a session-id line), text treated as untrusted, mapped to members via occupancy | later |
| mention edges | **to build**; routing evidence only, never proof of expertise | later |

**Past work** is the reverse-chronological list of epics, closed items,
authored merged commits, receipts and (later) rulings, folded where they link.
Corrections (a revert, a reopened item, a superseded ruling) are recorded
against the original evidence and shown, not deleted.

**Expertise score** (provisional weights, tuned against the §10 metrics):
ruling answered 5, item closed as owner 3, authored commit on a matching path
2 (capped per branch), review 2, routed question answered 2. Self tags are a
separate lower tier that only breaks ties among members with no evidence.
Anti-gaming: dropped items count 0; items closed by the same holder within 2
minutes of creation count half; a routed question counts only once answered;
in teams with at least three eligible members, no member gets more than half
of the team's routed questions (smaller teams use the load penalty only).

No language model on the read path. v1 matching is token overlap using the
messaging whole-word index.

### 8.4 Standing rules and their delivery

- Rules live on the member and on the team (≤ 10 one-liners each, or a runbook
  path), each with its source: Owner rule, ruling id, or lead default.
  Assistants inherit their member's and team's rules.
- They are delivered in every item brief (§7.4), prepended as a short digest
  (hash plus list) to every agent spawn message, and returned by the first
  `ping` after a spawn, resume or respawn.
- A change pushes a one-line notice to the member's main session.
- S1 moves continuous worker contracts out of task prompts into the continuous
  record's standing instructions.

### 8.5 Keeping it current

1. **Derive first.** Focus, current work, past work, expertise, availability
   and org position are computed.
2. **Nudge on drift, not on a schedule.** `report_done`, closing an item, a
   resume or respawn, and the first `report_done` after a long gap return a
   one-line `profile_hint` only when computed drift crosses a threshold. This
   does not depend on observing compaction, which Codex does not expose.
3. **Show staleness.** `ask_me_about` tags without evidence show "unverified";
   a `mandate` older than 60 days whose work has shifted raises
   `profile_drift` to the manager at most once per two weeks. Closing an epic
   prompts the manager to review the mandate of each kept member.
4. **Owner overrides** from the org chart and A-e.
5. **Suggestions, never edits, from a cheap model** (later).

### 8.6 Safety

Profiles, item titles and descriptions, `report_done` reasons, ruling text and
handoff notes are written by agents and read by other agents. Every
agent-written string in tool output is wrapped as `{"untrusted_text": …}` with
a fixed docstring warning. Tags are limited to `[a-z0-9 ./_-]{1,40}`, fields
are length-capped, `why` refs are ids plus titles of at most 80 chars, and
routing never depends on authored text alone. There is no imperative-phrase
blocklist (it rejects legitimate text and misses paraphrases).

## 9. How existing things map

### 9.1 Continuous tasks

- A continuous task becomes a member with `kind='continuous'`, mapped by
  `(daemon_id, continuous record id) → member_id` with a separate nullable
  planning-task FK (continuous ids are host-local strings that may have no
  planning row).
- Handles are normalized (lowercase, `[a-z0-9-]`, collision-suffixed); the
  original slug and `<slug>-orchestrator` stay as permanent aliases. State
  directories, queues, `ct/<slug>` channels and names are never renamed. The
  continuous record keeps its one workspace across runs; it is the member's
  home workspace.
- In Persistent mode the same session stays main. In Fresh mode each fire opens
  a new main row and closes the previous one; a still-alive previous session is
  fenced and kept only for its own run's `report_done`. The scheduler's
  run-sequence and `report_done` ownership checks are unchanged.
- Its workers are assistants; workers that hold items across runs can be hired
  into the continuous member's team.

### 9.2 Initiatives and divisions

They are teams (the same rows). `lead_member_id` comes from the S2 roster
(today's lead of the coordinator task as the proposal). Boards anchored by
`root_task_id` get a team created for them. Initiative project approval is
unchanged.

### 9.3 Subtasks, forks, workflows

- `create_subtask` keeps creating a child task (and an ephemeral workspace in
  branch mode), worked by an assistant.
- `fork_session` makes an assistant unless `hire=` is passed.
- Workflow participants are assistants of the member whose workspace runs the
  workflow.

### 9.4 Backtests and cloud workers

Unchanged. The new lifecycle behaviour keys on the workspace kind (`home` /
`stream` / `ephemeral`) and on a task being placed; unclassified workspaces
and `kind=backtest` keep today's behaviour exactly, including done-PATCH VM
teardown, preemption requeue, `archived` and the archive sweep. Cloud dispatch
workers stay one-shot task runners.

### 9.5 The Owner

Owner is a member (`handle='owner'`, `kind='owner'`, no sessions), the root of
the manager tree and a valid PoC. Owner availability (F7) feeds derived
availability. `who` does not return Owner except for topics where an Owner
ruling is the only authority; agents reach Owner through `notify_user` or the
Owner agenda.

## 10. Discovery tools (MCP)

```
org(view="me")                    # my member, manager chain, reports, assistants, teams, epics, standing rules
org(view="member", member=…)      # four facts + chain + reports + host/reachable_here + doing-now
org(view="team", team=…)          # lead, members, guests, sub-teams, current and past epics, charter
org(view="directory", query=None, team=None, project=None)
who(topic, intent="explain"|"owner"|"approve", scope="team"|"org", available=False, limit=5)
    -> [{member, handle, title, team, score, why[≤3 checkable refs], availability, load,
         host, reachable_here, authority_unverified?}], teams[{team, lead, epics, score}]
owner_of(path=None, component=None, repo=None)      # authored commits, claims, and open items whose links/job name the path (S5)
who_decided(query=None, item=None, board=None)      # once rulings exist
profile_set(member=None, …, expected_revision, request_id)   # write
place(task, board, group=None, holder=None)          # write, S3
epic(action="create"|"set_poc"|"close", …)           # write, S3; close runs the §7.6 checklist
ask(topic=None, to=None, text, item=None)            # write, later
```

- Compatibility: today's `org(view="reports")` maps to `org(view="me").reports`,
  `view="division"` to `view="team"`, `action="set_lead"` to a team-lead or
  manager edit. The old names stay as aliases.
- `chat_people` rows gain `member`, `title`, `team` and the doing-now line.
- **Intent filters first, then ranks.** `owner` considers only current
  owner-role holders, PoCs and leads. `approve` considers gate grants and
  Owner rulings; approver-role holders are returned only as decision contacts
  with `authority_unverified`. An unavailable authority is shown with its
  deputy, never replaced by an expert without authority. `explain` ranks by
  expertise × availability (live idle 1.0, working 0.8, needs_human/errored
  0.3, vacant → deputy) × a load penalty.
- **Custodians of artifacts.** `owner_of(path=…)` also answers "who holds this
  artifact right now" (e.g. a set of signed manifests): the owner-role holders
  of open items whose `links` or declared `job` name the path, then OWNER.md
  files once that scanner ships. EP's example took three candidates, about
  five minutes and one wrong read to find a manifest's custodian.
- **Evidence.** Each row carries up to three refs an agent can check with
  existing tools. For code topics `owner_of` points at the author of the path.
  Discovery finds the person; agents still read the implementation.
- **Scope and visibility.** CM has no project ACLs; boards and channels are
  readable org-wide. `who` uses only evidence the caller could already read:
  boards, public channels and channels it belongs to, never DMs. Unreadable
  evidence is not counted at all. Owner sees everything.
- **Metrics.** v1: a `who` call followed within 30 minutes by a DM or mention
  to a returned member that gets a reply; `possible_duplicate` hints accepted;
  evidence coverage per team. Later, for `ask`: answered on first route,
  re-routes, duplicate work.
- **`ask` (later)** routes a question to the best eligible member or to a team
  inbox whose lead triages it; it reuses rulings question records once they
  exist and caps open inbound questions per member (default 5). This is the
  direct fix for "EP as the human message bus".

## 11. Owner's view (TUI)

- **Org chart (F6)** is drawn from teams and managers: team boxes with the lead
  on top and the team's current epics as chips, members under it, manager
  lines between members, dotted markers for matrix cases, guests marked,
  assistants folded under their member as a count with state glyphs
  (expandable; ended ephemeral ones hidden), vacant members drawn empty with
  their stand-in, standby teams greyed. Cards show handle and title; the info
  pane shows the four facts, mandate, focus, current items and inbound question
  count. `e` edits the profile; `/?topic` runs `who` and highlights matches.
- **Sessions sidebar** gains a Team sub-view (team → member → sessions,
  folding as today). The Task sub-view stays selectable until S8.
- **Planning view** is the backlog, as today. `A-f` places a task on a board,
  with "assign to member" and "hire a new member for it". `A-f` opens a
  chooser with Launch preselected (Enter launches as today; `p`/↓ places). Placed tasks show a
  board chip and their item status. A read-only "on boards" section lists
  epics and board-native items, so everything in flight is visible from one
  place.
- **Board overlay (Alt+B)** shows headings and epics as a folding tree, with a
  PoC chip and team on each epic; `g` on a group edits it (promote to epic, set
  PoC, close epic, which opens the §7.6 checklist).
- **Roster screen** (S2): the proposed members (member, handle, team, manager,
  kind, home workspace) for Owner to approve or edit in one pass, and later the
  place to hire, move and retire.
- **A-e** on a session edits its member's profile; on a team header, the
  team's charter, rules and lead.
- **F9 → Staffing** edits the staffing policy (§4.5).

## 12. Persisted state

New fields are added without a manifest-schema bump (no `--hard` deploy) and
without changing the holder wire schema.

**S0b preservation bridge.** Today's records are typed projections
(`ManifestEntry`, the monolith `SessionRecord`, `holder-proto`'s reexec
record), and an old holder reparses rollback records into its own typed shape,
so unknown fields would be dropped. S0b adds one **nested**, opaque `org`
object to each of them (not serde-flattened into the record, so nothing
collides with existing top-level fields such as the transcript `generation`),
round-tripped unchanged by every path:

| State | Where | Carried in `org` |
|---|---|---|
| durable manifest + tombstones | `daemon/src/manifest.rs`, `session.rs` | `org.member_id`, `org.occupancy_id`, `org.occupancy_generation`, `org.acting_as`, `org.workspace_kind`, `org.binding_epoch` |
| reexec record (split and monolith) | `daemon/src/reexec.rs`, `holder-proto/src/reexec_manifest.rs` | same |
| holder adoption and rollback | `daemon/src/holder_mode.rs`, `holder/src/holder.rs` | same; needs the S0b holder (`daemon.upgrade_holder`) |
| resume identities | `daemon/src/resume_identity.rs` | `member_id` |
| TUI manifest and snapshot pushes | `tui-sessions.json`, `task.update_tree` | `member_id`; old TUI snapshots must not overwrite member/binding relations (`binding_epoch` guard) |
| continuous state | `~/.cm/continuous-tasks/<id>/` | unchanged; the member mapping lives centrally |

Rules:

- **Capabilities.** Daemons advertise each role separately, and the API
  activates a feature for a host only when that daemon, its holder and its
  configured rollback pin advertise what it needs:

  | Capability | Means | Needed by |
  |---|---|---|
  | `org_preserve_v1` | round-trips the `org` object (S0b) | anything that populates org fields |
  | `lifecycle_guard_v1` | terminal sweep honours `terminal_via=item` and workspace kinds; local `home` marks applied (S3) | central status mirroring for tasks with sessions on that host |
  | `org_marks_v1` | applies and acks the `home`/`stream` workspace marks the heartbeat reply pushes (S2) | the central home-mark push |
  | `org_stamp_v1` | stamps `member_id` / `acting_as` on writes (S4) | member attribution |
  | `occupancy_v1` | maintains occupancy rows and generations on spawn, revive and Fresh fires; standby; fencing (S6) | inherited holds, replacement |

- **Brain-only capabilities.** `org_marks_v1` and `lifecycle_guard_v1` run
  in the brain; the holder only carries the `org` object they write. The
  holder vouches for them by advertising `org_preserve_v1`, while the rollback
  pin must list them itself. A host therefore advertises them from the second
  deploy of a build that has them, so a rollback never lands on a brain
  without them while the host still advertises them.
- **Compatibility floor.** The S0b build (brain and holder) is the minimum
  rollback pin once any org field is populated. Old pid-based calls keep
  working throughout v1 unless they invoke a new member-scoped operation.
- Fixtures cover a pre-feature binary (fields absent, nothing breaks) and a
  bridge-capable old binary (fields preserved), across adopt, reexec, revive,
  resume and rollback, asserting member id, occupancy, both generation
  counters (set to different values) and acting role survive independently.
- The API is authoritative for member bindings; daemons cache them with the
  manager-chain snapshot. After a brain restart the daemon admits member-scoped
  calls from the cache (fail-static, §6.3) and refreshes from the API.
- Both task→workspace reconstruction paths (`state.rs` rehydration and the
  `methods.rs` restore path) are gated on `binding_epoch`.
- **Offline spawns.** If the API is unreachable when a member's main session
  would start, the session starts as an **unbound standby**: no work delivered,
  no member-scoped capabilities, no writes to the member's home workspace. It
  becomes main only after an atomic occupancy claim succeeds (generation
  compare-and-set on `occupancy_generation` at the API); a losing session stays an assistant and never
  executes the disputed assignment. The API checks the caller's occupancy generation on
  every member-scoped write, so a stale main on another host is fenced the
  moment a replacement commits.

## 13. Migration

Each stage deploys alone, is reversible (except where stated), and keeps old
clients working. Permission changes run in shadow before they enforce.

| Stage | What | Notes |
|---|---|---|
| **S0a guards** | NULL-safe `prompt` readers (`/workers`, CLI, `worker/startup.sh`); TUI adoption keeps taskless sessions; `propose_task` keeps filing into the backlog (Owner, 2026-10-25: proposals must not all wait for an A-a accept), and its docstrings and the agent guide stop saying "draft" | API/CLI/TUI only; ships before S1 |
| **S0b bridge** | the §12 `org` preservation object and `org_preserve_v1` on brain and holder; `daemon.upgrade_holder` on cm-manager, cm-sessions and Owner's laptop; `workspace_kind` in local records (all existing = unclassified; new isolated mints recorded `ephemeral`) | needed before S2's `home` push and S3; does not block S1. **The holder upgrade runs only on Owner's explicit call** (planned into a deploy, not run without it) |
| **S1 prompt → description** | inventory (real instructions in prompts, empty descriptions, continuous contracts → continuous standing instructions); the compatibility table below; one marker-gated backfill appends real prompts under `## Instructions`; description-only delivery after the backfill and every dispatch path read description; commit trailer hook installed | the `prompt` column is never dropped (`sql/003` alters it on every start); the MCP param stays as a deprecated alias |
| **S2 org core** | `members`, `incarnations`, `team_memberships`, manager tree, team columns (`coordinator_task_id` nullable + API models, `status`, `standing_rules`); Owner-approved roster (continuous members, initiative/division leads, long-lived lanes; dedupe by pid, coordinator and continuous ownership; handle collisions get a suffix, never steal an alias); an Owner-only **adopt** action (roster screen and `org(action="adopt")`) that binds a live session to a new member, so members can be added after the roster; roster and adopted workspaces marked `home` centrally **and pushed to each owning daemon**, which confirms; `org`, `ping`, A-i and F6 read members and managers | holds stay pid-keyed; nothing enforces |
| **S3 epics, groups, placement** | `groups` with headings and epics; PoC as a member id with a real team; staffing flow; epic close plan (§7.6); link-first placement (§7.1) with field ownership, status mirror, translated legacy writes, `terminal_via=item` and the lock order; item roles, single owner, holder migration; description revisions; launch grants for owner-role holders; `possible_duplicate`; the planning view's read-only "on boards" section. **Lifecycle guard:** the terminal sweep skips `terminal_via=item` closes for non-ephemeral sessions; the sweep and `mark_subtask_done` teardown both skip members' main sessions and never remove `home`/`stream`/pinned workspaces; ephemeral and unclassified workspaces keep today's teardown | per host: daemons ship and advertise `lifecycle_guard_v1` first; central mirroring for a task turns on only when every daemon holding its sessions advertises it. Rollback with placed tasks: mirroring off, links kept |
| **S4 daemon stamping** | daemons stamp `member_id` and `acting_as` (`org_stamp_v1`); standing-rules delivery (§8.4) | occupancy rows and fencing wait for S6 (`occupancy_v1`) |
| **S5 profiles, discovery, shadow scope** | `profile_set`, `org(view=member/team/directory)`, `who` over items, epics, receipts, `report_done`, commits and tags; staleness display; manager-chain scope and new board-edit rule computed and logged beside the old rules (observational) | end of v1 |
| **S6 members become real** | occupancy rows and generations on spawn, revive and Fresh fires, fencing at the API (`occupancy_v1`); spawn defaults and `hire` (§4.2); `@handle` → main session; qualified body mentions; holds by member with every consumer updated together (holder checks, flags, capacity, pushes retargeted, orchestrator/steward, gate grantor and editor allowlists); handover checklist; offline standby; member workspace records and `cm-ws/` branches; `start_session(item=…)`; models resolve member before task; bootstrap allow rules (§6.3) | consumers roll before inherited holds turn on |
| **S7 scope cutover** | per host, after a week of shadow logs with only expected differences: manager-chain scope and the new board-edit rule authoritative | rollback = shadow with bootstrap rules (§6.3) |
| **S8 cleanup** | retire `agent_task_edges` and task bindings for scope (irreversible, announced); Task sub-view becomes a filter; full convergence of tasks into items (O6, §16.1 preconditions) | |

### 13.1 v1 cut line

- **Standalone, any time:** the staffing policy (§4.5).
- **v1 (S0–S5):** all five of Owner's asks in visible form: no prompt field;
  members with profiles, managers and teams; workspaces as offices; epics and
  groups with PoC teams and a defined ending; backlog → board placement;
  discovery (`org`, `who`). Reporting lines are real for display, routing and
  vacancy; session control still uses today's rule.
- **v1.5 (S6):** members survive replacement (holds, pushes, gate authority,
  handles follow them), explicit hiring defaults, handover.
- **Later (S7–S8 and §16):** scope cutover, full convergence, member mail,
  grants registry, `ask`, OWNER.md/VM-label/mention evidence, moves between
  boards.

### 13.2 S1 compatibility table

| Request | Result |
|---|---|
| CREATE with description only | stored as given |
| CREATE with prompt (old client, incl. today's `propose_task` which sends both) | a non-empty prompt that differs from the name and isn't contained in the description is appended to the description under `## Instructions`, so it is delivered |
| PATCH with description only | stored; revision bumped |
| PATCH with prompt empty, equal to the name, or contained in the description that **will be stored** (the request's description if it has one) | prompt ignored |
| PATCH with a different, non-empty prompt | `409 prompt_deprecated`: "put instructions in the description" (a person typed it; it is not silently dropped) |
| responses | `prompt: ""` (not an echo, which old composers would deliver twice) |

Composers (daemon `compose_task_launch_prompt`, TUI `compose_launch_prompt`,
dispatch) skip a prompt that is empty, equal to or contained in the
description. Old clients without a revision keep last-write-wins on the
description, as today. Acceptance cases: old-editor round trip, an
instruction deleted from the description, a prompt-only PATCH, an old-client
CREATE after the backfill whose worker-only prohibition must appear in the
delivered brief, API/daemon version skew.

### 13.3 Acceptance cases

Item behaviour preserved through placement and member holds: mention-driven
wakes, holder history, ruling and waiver trails, check-back and ETA flags;
GO/waiver grants stay bound to their release content, not to an employee;
same-uid revive and model change; revive after 24 h vacancy; Persistent and
Fresh continuous fires; replacement while a push or a gate grant is pending,
then issuing and revoking with the successor; a manager finishing its own item
while its reports keep working, then managing them after a brain restart;
placing and closing a long-lived lane's task in S3 with the lane and its
children staying alive; closing an epic with each team disposition; old
TUI/editor and old hub/daemon combinations; API outage during a brain restart
and during a spawn, with two hosts racing a fill; API started twice on a
migrated database; backtest success, preemption and archive.

Other hazards (details in the impact map): `sql/005` re-creates the status
CHECK on every start, so new statuses need 005 edited plus a later "replace
only if missing" block; backfills run once behind `migrations_applied` as
single statements.

## 14. Schema sketch (planning DB)

```sql
-- teams: evolve initiatives in place (a `teams` view aliases the table)
ALTER TABLE initiatives ADD COLUMN IF NOT EXISTS parent_team_id UUID REFERENCES initiatives(id);
ALTER TABLE initiatives ADD COLUMN IF NOT EXISTS lead_member_id TEXT;
ALTER TABLE initiatives ADD COLUMN IF NOT EXISTS charter TEXT NOT NULL DEFAULT '';
ALTER TABLE initiatives ADD COLUMN IF NOT EXISTS standing_rules JSONB NOT NULL DEFAULT '[]';
ALTER TABLE initiatives ADD COLUMN IF NOT EXISTS team_status TEXT NOT NULL DEFAULT 'active';  -- active|standby|archived
ALTER TABLE initiatives ADD COLUMN IF NOT EXISTS org_id TEXT NOT NULL DEFAULT 'main';
ALTER TABLE initiatives ALTER COLUMN coordinator_task_id DROP NOT NULL;   -- S2
-- kind CHECK grows to ('initiative','division','team'); acyclicity checked by trigger

CREATE TABLE members (
  id TEXT PRIMARY KEY,                       -- mem_<ulid>
  org_id TEXT NOT NULL DEFAULT 'main',
  handle TEXT NOT NULL, kind TEXT NOT NULL,  -- agent | continuous | owner
  status TEXT NOT NULL,                      -- active | vacant | suspended | retired
  manager_member_id TEXT REFERENCES members(id),   -- NULL only for owner; acyclic
  primary_team_id UUID REFERENCES initiatives(id),
  occupancy_generation INT NOT NULL DEFAULT 0,  -- bumps on each replacement; fencing
  profile JSONB NOT NULL DEFAULT '{}',       -- authored fields {value,set_by,via,set_at}
  standing_rules JSONB NOT NULL DEFAULT '[]',-- [{text, source}]
  revision INT NOT NULL DEFAULT 0,
  defaults JSONB,                            -- engine, model, effort, home workspace
  created_by TEXT, created_at TIMESTAMPTZ NOT NULL DEFAULT now(), retired_at TIMESTAMPTZ,
  UNIQUE (org_id, handle)                    -- retired handles stay (tombstones)
);
CREATE TABLE member_aliases (org_id TEXT, alias TEXT, member_id TEXT, PRIMARY KEY (org_id, alias));
CREATE TABLE continuous_members (daemon_id TEXT, continuous_id TEXT, member_id TEXT NOT NULL,
  planning_task_id UUID, PRIMARY KEY (daemon_id, continuous_id));
CREATE TABLE incarnations (                  -- occupancy
  id BIGSERIAL PRIMARY KEY, member_id TEXT NOT NULL REFERENCES members(id),
  role TEXT NOT NULL,                        -- main | assistant | standby
  occupancy_generation INT, pid TEXT NOT NULL, daemon_id TEXT NOT NULL, session_uid TEXT NOT NULL,
  workspace_id TEXT, predecessor BIGINT REFERENCES incarnations(id),
  started_at TIMESTAMPTZ NOT NULL, ended_at TIMESTAMPTZ, end_reason TEXT
);
CREATE UNIQUE INDEX incarnations_one_main ON incarnations (member_id)
  WHERE role = 'main' AND ended_at IS NULL;
CREATE UNIQUE INDEX incarnations_open_pid ON incarnations (pid) WHERE ended_at IS NULL;
CREATE TABLE team_memberships (team_id UUID, member_id TEXT, role TEXT,   -- primary | guest | deputy
  since TIMESTAMPTZ, until TIMESTAMPTZ, PRIMARY KEY (team_id, member_id, role));
CREATE TABLE groups (id UUID PRIMARY KEY, board_id UUID REFERENCES boards(id),
  parent_group_id UUID REFERENCES groups(id), name TEXT, description TEXT, ord INT,
  kind TEXT NOT NULL DEFAULT 'heading',      -- heading | epic
  goal TEXT, epic_status TEXT,               -- active | done | cancelled
  team_id UUID REFERENCES initiatives(id),   -- the epic's team; lead = PoC
  closed_at TIMESTAMPTZ, closed_by TEXT, close_record JSONB);  -- §7.6 dispositions
CREATE TABLE workspaces (id TEXT PRIMARY KEY, idem_key TEXT UNIQUE, daemon_id TEXT, path TEXT,
  repo TEXT, branch TEXT, base_sha TEXT, kind TEXT,  -- home | stream | ephemeral   (S6)
  office_of TEXT REFERENCES members(id), team_id UUID, purpose TEXT,
  state TEXT, created_at TIMESTAMPTZ, closed_at TIMESTAMPTZ);
ALTER TABLE items ADD COLUMN IF NOT EXISTS task_id UUID REFERENCES tasks(id);
ALTER TABLE items ADD COLUMN IF NOT EXISTS link_archived_at TIMESTAMPTZ;   -- before the index
CREATE UNIQUE INDEX IF NOT EXISTS items_task_link ON items (task_id)
  WHERE task_id IS NOT NULL AND link_archived_at IS NULL;   -- one live link per task (new index; none exists today)
ALTER TABLE items ADD COLUMN IF NOT EXISTS description TEXT;        -- board-native items only
ALTER TABLE items ADD COLUMN IF NOT EXISTS metadata JSONB;          -- board-native items only
ALTER TABLE items ADD COLUMN IF NOT EXISTS group_id UUID REFERENCES groups(id);
ALTER TABLE items ADD COLUMN IF NOT EXISTS team_id UUID REFERENCES initiatives(id);
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS description_rev INT NOT NULL DEFAULT 0;
ALTER TABLE item_holders ADD COLUMN IF NOT EXISTS role TEXT;        -- owner|contributor|reviewer|approver|holder(legacy)
ALTER TABLE item_holders ADD COLUMN IF NOT EXISTS member_id TEXT;   -- authority key from S6
ALTER TABLE item_holders ADD COLUMN IF NOT EXISTS via_member_id TEXT;
-- member_id + acting_as beside every pid column: item_events, item_receipts, item_grants,
-- boards.orchestrator/steward, gate editor/grantor allowlists (S6)
CREATE TABLE profile_events (id BIGSERIAL, subject TEXT, field TEXT, prev JSONB, new JSONB,
  actor JSONB, via TEXT, at TIMESTAMPTZ);
CREATE TABLE org_events (id BIGSERIAL, subject_type TEXT, subject_id TEXT, type TEXT,
  actor JSONB, prev JSONB, new JSONB, reason TEXT, at TIMESTAMPTZ);
  -- hire, replace, move, retire, poc_set, epic_closed, team_standby, team_split
CREATE TABLE evidence (id BIGSERIAL, source_key TEXT, role TEXT, member_id TEXT, pid TEXT,
  kind TEXT, ref TEXT, project TEXT, visibility TEXT, topic_tokens TEXT[], paths TEXT[],
  weight REAL, corrected_by TEXT, at TIMESTAMPTZ, UNIQUE (source_key, role, member_id));
```

New tables get their own change feed (members, teams, groups, workspaces) so
viewers never poll.

## 15. Decisions

### 15.1 Owner decisions

- **O1. Members are the durable employees — decided (Owner, 2026-10-25).**
  Members are reused across epics because their sessions carry useful
  context. Why a member rather than the workspace: a member survives its
  checkout being closed, rebuilt or reaped; several members can share a
  checkout (co-tenancy is normal); and a host move replaces the checkout. Cost:
  a third concept beside Owner's two levels, a roster to approve once, and a
  member id beside every pid.
- **O2. An epic's PoC leads a real team — decided (Owner, 2026-10-25).** Not
  routing-only. Staffing is explicit (§7.3).
- **O3. Link-first for tasks and items in v1 — decided (Owner, 2026-10-25):**
  link now, decide on a merge later using the evidence listed below.

  Today there are two kinds of work row: **tasks** (the planning view's
  backlog: what Owner files and launches) and **board items** (what agents
  create on boards to track work in flight; most items today). The question
  is whether to keep both and link them, or merge them into one.

  | Owner does | Link-first (recommended) | Merge now |
  |---|---|---|
  | files an idea | planning backlog, as today (a task) | a backlog board (an item) |
  | puts it into epic Latent on the sejd board | `A-f` place: a chip in the planning view, an item on the board | move it |
  | an agent tracks work on a board | an item, visible on the board (and in the planning view's read-only "on boards" section, see below) | the same single list |
  | moves work to another board | unplace and re-place, one step | move |
  | marks it done | `A-d` in either place acts on the item | one row |
  | sees everything EP holds | `org`, F6 or the board; the same either way | same |

  - *Link-first* keeps every task tool and the planning view working as they
    are; the board owns status and holders of a placed task, and the task keeps
    its text and metadata (§7.1). Cost: two kinds of row remain; placed tasks
    show in both places; a later merge would move the text and metadata a
    second time (only the status/holder authority carries over).
  - *Merge now* gives one list everywhere and free moves. Cost: the largest
    migration in the design (API, dispatch, planning view, board engine, every
    task tool behind an adapter). It delays placement and anything built on
    it; members (S2), discovery (S5) and epics on board-native items do not
    depend on it.
  - *Cheap middle step, included in S3:* the planning view gains a read-only
    "on boards" section listing epics and board-native items, so Owner has one
    place to look without merging tables.
  - *Deciding later* uses: how often Owner moves work across boards, how often
    a single list is missed despite the "on boards" section, and the friction
    of placed tasks appearing twice.
- **O4. Only deliberate hires are members — decided (Owner, 2026-10-25).** One-shot workers
  (reviewers, bench runners, subtask workers, `detective-*`) are assistants:
  folded under their member on F6 as a count, with no handle, profile or
  vacancy flag, and not returned by `who`. Scale: about 180 agent-minted
  subtask worktrees were active in the last 7 days across cm-sessions and
  cm-manager (directory mtime; an upper bound on new ones). Promoting an
  assistant keeps its session and context, so choosing "assistant" loses
  nothing if a worker turns out to be valuable (O1).
  - *Alternative at its strongest:* every new workspace's leader is a member,
    retired automatically when its ephemeral workspace closes (so no vacancy
    noise). Costs: handle and roster churn, a box per worker on F6, and
    profiles and evidence fragmented across short-lived members.
  - *Recommendation:* deliberate hires (§4.2), with promotion (CM suggests it
    for an assistant alive 3 days that holds items as owner).
  - *A one-off that needs its own worktree* is an assistant with an
    **ephemeral workspace** (`isolated=true` or branch-mode `create_subtask`,
    as today): its own checkout and `cm-sub/…` branch, owned by the member it
    works for. It closes when its work is done, the assistant has ended, and
    the reaper's protections pass (no live process, not dirty, evidence
    released); the branch is kept, as today. If the branch is still needed
    after the assistant ends (e.g. waiting on review), the member keeps it as
    a `stream` workspace. If the assistant turns out to be long-lived, it is
    promoted: it becomes a member, keeps its session and context, and its
    ephemeral workspace becomes that member's home workspace.
  - *Superseded in part (Owner, 2026-10-25, O5):* assistants "should be just
    as durable as the main sessions". An assistant's own checkout is a
    `stream` workspace of its member's office, not ephemeral, and nothing
    automatic closes the assistant or its checkout (§4.2, §4.3).
- **O5. Assistants are as durable as main sessions — decided (Owner,
  2026-10-25).** An assistant belongs to its member's office and keeps its
  session, context and checkout until it is deliberately closed by its
  member, the member's manager chain or Owner. Completing a task or item,
  `mark_subtask_done` and the terminal sweep never close it; the member
  reuses it for its next work. `ephemeral` remains only for sessions that
  belong to no member. Cost: idle assistants and their checkouts stay
  resident, so reuse comes before spawning, and the board's free capacity
  lists idle assistants.
- **O6. Full convergence: a task is a board item — decided (Owner,
  2026-10-27).** O3's link-first stays the v1 path, but it is no longer the end
  state. Sessions are reused across work, so binding work to a session is the
  wrong shape: a task becomes a board item, and it is not coupled to a session.
  Launching a session records its work as a board item (S6,
  `start_session(item=…)`), and S8 merges the two kinds of row, with §16.1's
  preconditions as its checklist. The planning API stays as the surface for
  that work. Trigger: on 2026-10-27 an org-chart move (QRM-DeepDecay under
  Joint-Decision) did not reach the sidebar, which still nested by
  `parent_task_id`.

### 15.2 Design decisions (override any)

- D1. The manager is stored and cycle-checked; teams group, and agree with
  managers by default.
- D2. Management scope follows the manager chain only; team membership never
  extends it; rollout is shadow-first with named bootstrap rules.
- D3. Signer, attribution and authorization are separate; assistants never
  rule or approve as the member.
- D4. `focus` is derived, with an expiring self note; title and mandate are set
  by the manager chain.
- D5. No member mail in v1: `@handle` reaches the current main session, and
  continuity comes from the handoff record and standing rules.
- D6. The `prompt` column stays physically (nullable, unused); only the wire
  field is deprecated.
- D7. No grants registry in v1; "may decide" shows gate grants, Owner rulings
  and approver roles marked unverified.
- D8. Discovery counts only evidence the caller could already read.
- D9. Retired handles are never reused; references are org-qualified.
- D10. A team outlives its epics; finishing an epic keeps, parks or splits the
  team explicitly.

## 16. Open questions

1. **Full convergence** — decided by O6 (2026-10-27); what remains open is how. Preconditions: one canonical work id
   everywhere; the single status authority already in place (link-first gives
   this); cross-board `blocked_by` by stable id; receipt and grant provenance
   independent of the item's current board; moves that refuse to cross live
   dependencies or scoped gate evidence; an adapter for old clients that write
   tasks; a rollback path.
2. **Member mail.** A conversation kind whose audience is a member, readable by
   later main sessions on demand (never auto-injected), 30-day window. Needs a
   versioned audience type in the store and hub validator, replacement-safe
   cursors and watches, and readers deployed before writers. Legacy pid DMs
   stay private.
3. **Grants registry.** Who seeds it and when; it comes with the approval queue.
4. **Standby teams.** Should a team on standby be offered to new epics
   automatically (e.g. "Latent team free; new epic X matches its past work")?

## 17. Non-goals

- No performance scoring or HR metaphors beyond hire/move/retire.
- No transfer of transcripts or private memory between sessions.
- No concurrent main sessions per member; one manager each (guest memberships
  are the dotted lines).
- Projects (repos) do not become org units.
- No new permission enforcement in v1.
- Backtests and cloud dispatch workers are not members or items.
- No cross-org federation now; `org_id` and qualified references keep the door
  open (a guest from another org gets item roles and channel access, never
  management scope).

## 18. Inputs

- Brief and Owner answers: `~/.local/share/swarm-coord/org-model/brief.md`
- Brainstorms: `…/brainstorm/entity-model.md`, `profiles-discovery.md`,
  `impact-map.md` (file:line references for the change sites)
- Interviews (Swarm-Gardener): `…/interviews/summary.md`, verbatim replies in
  `interview-log.md`
- Reviews: `…/reviews/codex.md`, `claude.md` (round 1), `codex-r2.md`,
  `claude-r2.md` (round 2), `codex-r3.md`, `claude-r3.md` (round 3),
  `codex-r4.md`, `claude-r4.md` (round 4)
