# Org model: members, teams and items

Status: **revised draft, review round 1 folded** (Swarm-Coord, 2026-10-25).
Owner-directed. Inputs are listed in §18. The interviews are a purposive
sample (eight sessions, mostly the trader org plus CM and scraper triage),
not an org-wide vote; EP's main answers arrived late and are noted where
they changed something.

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
  optionally, **assistant sessions**. When the main session is replaced
  (respawn, fresh restart, engine change, host move) the member keeps its
  handle, work, reports and history.
- **Where** is a **workspace**: a checkout on a host. A member has a home
  workspace (its office) and may open stream workspaces for parallel work.
  Assistants that write code get their own ephemeral stream workspace.
- **Who works with whom** is a **team**: a lead, members, and optionally a
  board, channel and charter. Divisions, initiatives and staffed board groups
  are teams. Every member except Owner has exactly one **manager**, stored
  and cycle-checked.
- **What** is an **item**: work with a title and a description (no
  `prompt`). Work starts in a backlog and is **placed** on a board. Board
  groups are first-class and nest, which gives work its levels. A group with
  a **point of contact (PoC)** is a team whose members report to that PoC.

Owner-facing asks land early (S1 prompt removal, S2 groups with PoCs and
backlog → board placement); the identity and permission changes follow in
stages that each deploy alone and run in shadow before they enforce (§13).

### 1.1 Owner's terms and this design

Owner's brief described two levels: the workspace (an office with a leader
and some assistants) and the session (one agent). This design keeps that
shape but puts the durable identity on a third object, the member:

| Owner's term | This design |
|---|---|
| workspace = office, leader + assistants | a **member** (the leader) with its **home workspace** and **assistant sessions** |
| session = a specific agent | a member's **main session**, or an **assistant session** |
| workspace description | the **member profile** (+ an optional workspace `purpose`) |
| session description | derived "doing now" line: held item, last `report_done`, stream workspace purpose |
| tasks on the planning board | **items** in a backlog, placed onto boards |
| groups on the board | first-class **groups**; with a PoC they are **teams** |

Why the member and not the workspace is the employee (decision D1, §15):
interviewed workers run several checkouts at once (one per stream), a host
move replaces the checkout, and continuous orchestrators respawn into fresh
state each run. Each of those would end a workspace-keyed employee. The
workspace stays what it physically is, a checkout.

## 2. Problems with today's model

| Today | Breaks when |
|---|---|
| The employee is a session uid (`agent:<daemon_id>:<uid>`) | a continuous respawn, a fresh replacement, a fork or a host move gives the "same person" a new uid; holds flag `holder_gone`, names and DMs don't follow. Revive, resume and A-R keep the uid and are fine |
| Reporting lines are `tasks.parent_task_id` plus the lead rule (Owner/agent pin, else earliest live agent) | a task is work, not a person; an unpinned lead is whoever spawned first; an agent can re-parent a task under itself to gain a boss (org-chart known limits) |
| The task `prompt` is separate from the description | two texts drift; `propose_task` copies the name into `prompt`; real worker contracts (continuous triage) live only in the prompt |
| Finishing a task ends its sessions (terminal sweep, `mark_subtask_done`) | finishing one piece of work fires a long-lived employee |
| Permission scope is the caller's task subtree | an employee that moves to new work silently changes what it may control |
| Worktrees are keyed by task (`cm-sub/<slug>-<taskid7>`) | an office has to pretend to be a task to exist |
| Board groups are free-text headings | no owner per group, no levels, no team behind a workstream |
| Nobody knows who knows what | chat archaeology (44k messages in `#gpu-utilization`); EP acts as a human message bus |

Two things in CM already have the right shape and are the precedent:
continuous tasks (the `ct/<slug>` channel, `<slug>-orchestrator` name and
queue belong to the task; sessions come and go under it) and board holders
(keyed by participant, not by task).

## 3. Vocabulary

UI and docs use: **member**, **main session**, **assistant (session)**,
**workspace**, **team**, **manager**, **item**, **board**, **group**, **PoC**.
"Incarnation" is a schema term only.

| Term | Meaning |
|---|---|
| **Member** | a durable employee: handle, title, manager, primary team, profile. Owner is a member (`owner`) |
| **Main session** | the one session currently acting as the member (at most one) |
| **Assistant** | a session working for a member: reviewer, helper, bench runner, `detective-*`, workflow participant. Not a member; acts under its own name "for" the member |
| **Workspace** | a checkout on a host with its branch and local caches. `home` (the office), `stream` (one concurrent stream, durable) or `ephemeral` (an assistant's, closes with its item) |
| **Team** | lead + members + optional board, channel, charter. Kinds `division`, `team`, `initiative` |
| **Manager** | the member's one boss. Stored, cycle-checked |
| **Item** | a unit of work: title, description, status, holders with roles, group, team |
| **Backlog** | a board of unplaced items (kind `backlog`) |
| **Group** | a node in a board's item tree; groups nest |
| **PoC** | a group's lead; setting one makes the group a team |

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
  role). A same-uid model or effort change, revive or resume stays the same
  row with a change event. At most one open `main` row per member.
- Every write that records a pid today also records `member_id` and
  `acting_as = main | assistant` (§6.4), stamped by the daemon. Readers never
  join through occupancy history to find who did something.

### 4.2 Which sessions are members

Members are created deliberately and rarely. Everything else is an assistant.

| How the session starts | Result |
|---|---|
| Owner A-n, or planning "place → hire new member" | new member; Owner picks manager and team (default: Owner, no team) |
| `start_session(hire={handle, title, team?})`, `fork_session(hire=…)` | new member; manager = the caller's member; primary team = the given team if the caller leads it, else none |
| Promote an assistant | the assistant session becomes the main session of a new member managed by the old member |
| `start_session(member="<handle>")` | fills a vacant member as its main session (a replacement); needs to be that member's manager chain or Owner |
| Continuous scheduler respawn | new main session of the continuous member; the previous main row closes (§9.1) |
| Revive, A-R, Alt+s Resume, daemon/brain restart | same uid, same occupancy row; nothing changes |
| Any other agent spawn: plain `start_session`, `isolated=true`, `create_subtask` + spawn, `fork_session` without `hire`, `item_resolve(launch)`, `start_session(task_id=…)` | assistant of the caller's member. `isolated=true` (or `create_subtask` branch mode) gives it an **ephemeral** workspace, as today |
| Owner A-s (add a session to a workspace) | assistant of the workspace owner's member; Owner can tick "new member" |
| Owner A-F fork | assistant of the source's member unless Owner ticks "new member" |
| Bash sessions, backtest watch views, cloud dispatch workers | not members; bash/watch views are assistants of the workspace's member, cloud workers stay as today (§9.4) |

An assistant that turns out to be long-lived is promoted. Today's long-lived
lanes (msgfix, org, items, …) are hired explicitly at backfill (§13 S4).

### 4.3 Lifecycle

| Event | Effect |
|---|---|
| **Hire** | create the member; the spawn becomes its main session |
| **Replace** | the main row closes with a reason (`killed`, `replaced`, `context_exhausted`, `host_move`, `engine_change`); a new main row opens with `predecessor`; the member goes through **handover** (§7.5) |
| **Fencing** | a session whose main row has closed can no longer act *as the member*: member-scoped writes (holds, approvals, board settings) from it are refused with `not_main_session`. A revived old main session comes back as an assistant, with a notice |
| **Vacate** | the main row closes with no replacement for 24 h: member `vacant`; its items flag `member_vacant` to its manager; routing goes to its deputy, else its manager |
| **Suspend** | parked on purpose, or vacant 14 days with no holds (automatic): no flags, hidden from `who` and the directory |
| **Move** | change primary team and/or manager; needs Owner or the current manager chain; holds stay |
| **Retire** | holds hand back to the manager; reports move to the manager; handle tombstoned; history permanent. Closing a member's home workspace asks "retire member X?" (default yes when it holds no open items) |

Working memory (transcript, compaction, `agent_memory`) belongs to the session
and is not copied. Continuity comes from the board, rulings, the profile, the
standing rules (§8.4) and a **handoff record**: the durable checklist of
§7.5, plus an optional note the outgoing main session writes on `report_done`
or before a planned replacement. The checklist is the baseline; the note is
an optimisation, because a killed session cannot write one.

### 4.4 Names and messaging (v1)

- `@ep` and `chat_send(dm="ep")` deliver to the member's current main session,
  creating or reusing an ordinary DM with that session's pid. No message
  history moves between sessions in v1; continuity is the handoff record.
- The handle → main-session mapping lives in the messaging layer, written
  only by the API and replicated to every daemon like names are today. When
  the API is unreachable, `@handle` resolves to the last known main session.
- Assistants show under their own codename with a suffix: `rev1 (for ep)`.
  `@ep` never resolves to an assistant.
- A continuous member's handle is a normalized form of its task id
  (`bug-triage`), with `bug-triage-orchestrator` kept as an alias forever.
- Channel memberships and follows belong to the member and are re-applied to
  each new main session.
- **Later (not v1):** member mail, a new conversation kind whose audience is a
  member rather than a pid, readable by later main sessions on demand (never
  auto-injected), primaries only, 30-day window. It needs a versioned
  audience type in the store and hub validator, replacement-safe cursors and
  watches, and readers deployed before writers. Legacy pid DMs stay private.

## 5. Workspaces

A workspace is infrastructure: a checkout on a host, its branch, its local
venv/target/caches. It is not an org unit, has no reporting line, and being in
one grants nothing (no deploy rights, no authority over other sessions there).

- **Kinds.** `home` (the member's office; a replacement respawns there),
  `stream` (a member's own parallel checkout for one work stream, durable
  until closed), `ephemeral` (an assistant's; closes after its item closes,
  the assistant ends, and the reaper's existing protections pass: no live
  process, not dirty, evidence released).
- Owned by a member (`office_of`), or by a team for shared integration
  checkouts.
- Assistants that only read, review or run commands may share the office;
  assistants that write code get an ephemeral workspace by default, so an
  office never implies a shared dirty checkout.
- **Record and branch.** Workspaces get a central record (planning DB) and a
  local durable record (daemon manifest), both storing id, branch, base SHA,
  host and path. Creation reserves the id under an idempotency key *before*
  any filesystem or spawn work, then moves `creating → ready | failed`, so a
  lost response or crash never mints a duplicate. New workspaces use
  `cm-ws/<handle>-<ws7>` branches; the stored branch always wins over
  inference, a handle edit never renames a branch, and the `cm-sub/` and
  `cm/` parsers stay forever. Recovery and rematerialization learn the new
  prefix and read the local record when the API is offline.
- Profile: an optional one-line `purpose` plus derived facts (repo, branch,
  host, path, sessions, dirty state). Profiles that matter for routing are on
  members and teams.

## 6. Teams, managers and authority

### 6.1 Teams

`teams` evolves today's `initiatives` table in place (same ids, slugs,
boards and channels).

- `kind`: `division` (permanent), `team` (standing), `initiative`
  (time-bounded). Existing rows keep their kind.
- `parent_team_id` (acyclic, checked on write), `lead_member_id` (nullable:
  a leadless team shows its parent's lead as stand-in), `charter`, optional
  `board`, `channel`, `docs_path`, `org_id`.
- `coordinator_task_id` becomes nullable in S2, before the first taskless team
  exists; old clients keep receiving the stored id where there is one. The
  API create/response models change in the same stage.
- Projects (repos) stay attributes of items and workspaces, not org units.

### 6.2 Managers and reporting lines

Each member except Owner has exactly one `manager_member_id`. The boss is the
manager, full stop. It is stored rather than derived from teams because
members lead several teams, PoCs are set on other teams' boards, and a
derived rule produces zero, two or cyclic bosses in those cases (review
round 1, both reviewers).

- **Invariants:** no cycles (checked in the same transaction as the write);
  Owner is the root; a manager must be a non-retired member; vacant managers
  have a stand-in up the chain.
- **Defaults:** a hire's manager is the hirer's member. Joining a team as a
  primary member sets manager = that team's lead unless told otherwise.
  Leading a team never changes your own manager.
- **Teams and managers agree by default, not by force.** The org chart groups
  by team and draws manager lines; a member whose manager is not its primary
  team's lead is drawn with a dotted marker, which is how matrix cases show.
- **Who may change it:** Owner; the member's current manager chain (moving a
  report to another of its own reports); a lead accepting a member offered
  by that member's manager chain. A move is an `org_event` with a reason.
- `parent_task_id` keeps meaning "this item breaks down that item" and nothing
  else. Re-parenting work can no longer change anyone's boss.

### 6.3 Management scope (who may control whose sessions)

- A member's main session may control its own assistants and the sessions of
  its reports (transitively down the manager chain).
- An assistant may control only the sessions it spawned itself.
- Team membership (primary, secondary, guest) never extends scope; only the
  manager chain does. This also answers the interviewees' "authority per item,
  not per role" concern: a non-lead's scope shrinks to its own assistants and
  stops accumulating with every item it touches.
- `global_perms` keeps short-circuiting, and never crosses an org boundary.
- Rollout is shadow-first (§13 S5): the old task-subtree rule stays
  authoritative and the new rule is computed and logged; the new rule is
  allowed alone only where the old one cannot evaluate (taskless callers or
  targets) and for a caller's own assistants.

### 6.4 Permission matrix

| Action | Main session | Assistant | Manager chain | Team lead | Guest member | Owner |
|---|---|---|---|---|---|---|
| control sessions | own assistants + reports' sessions | own spawns | yes (via main session) | only if also the manager | no | yes |
| hold / update an item as the member | yes | only items delegated to it (`via`) | — | — | items they hold | yes |
| edit board settings | if board orchestrator or steward | no | — | own team's board | no | yes |
| create, edit or place items on a board | board team members + holders (their items) | as its member, on delegated items | — | yes | their items | yes |
| set a group's PoC | — | no | — | own board | no | yes |
| hire | yes (manager = self) | no | — | into own team | no | yes |
| move / retire a member | — | no | yes | accept only | no | yes |
| approve / rule | only with an existing grant or Owner ruling | never as the member | — | — | — | yes |

Board edit rules replace today's "holding any item on a board grants
board-wide edit": in the shadow period both apply; at cutover holders keep
rights on their own items only.

**Attribution of assistants.** Every write carries `member_id`, `acting_as`
and the pid. A ruling, approval, gate receipt or approver-role transition
counts as the member's only when made by its main session, or by an assistant
the member delegated that item to (recorded as `via`). Otherwise it is
attributed to the assistant's pid and shown "on behalf of ep", and earns the
member no evidence.

### 6.5 Approval authority

Reporting is not approval authority (EP's rule, kept). v1 does not add a
grants registry. "May decide" in v1 is: the item-level `approver` role (an
*assignment to make a decision*, not a permission), existing content-bound
gate grants (`gate_db`) and Owner rulings, all shown with their source. A
grants table with delegation lineage is a follow-up with the approval queue,
seeded explicitly by Owner (e.g. EP = prod GO, SEJD = law review).

## 7. Items, backlogs and boards

### 7.1 Link-first in v1

Planning tasks and board items converge **logically** in v1 and stay two
tables underneath (decision D4):

- **Placing** a task on a board creates an item linked to it (`items.task_id`,
  unique) with the task's description, and from then on **the item is the
  single authority for status and holders**. The task row mirrors it
  (`backlog` while unplaced, `running` while placed and held, `done` /
  `blocked` mirrored) through one API transaction; direct status writes to a
  placed task are refused with `placed_on_board` (old clients get the
  error text, not silent divergence).
- The item's stable cross-board id (`work_id`, UUID) **is the task id** for
  placed tasks, and a fresh UUID for items born on a board. So every existing
  task-id reference (worktree branches, `metadata.filer`, artifacts, creator
  edges) keeps resolving.
- **Unplacing** moves the item to the team's backlog board (the backlog is a
  board), keeping its `work_id`.
- **Moving** between boards assigns a new `board#n` and records the old one in
  `item_aliases`, so `sejd#1248` keeps resolving. Same-board `blocked_by`
  edges that would cross boards turn into `links` with a warning.
- New work filed by agents (`propose_task`) lands as a `draft` item on the
  relevant backlog board (the caller's team's, else the project's). Owner
  accepts or places it.
- Full table convergence (one storage model, tasks reduced to execution
  records) is decided after link-first has been in use; its preconditions are
  listed in §16.

### 7.2 What an item carries

- `title`, `description` (markdown; the work contract: scope, acceptance
  criteria, constraints). Placing requires a non-empty description. Edits to a
  placed item's description are versioned; holders get a "description changed
  by X" notice instead of silently new instructions on the next delivery.
- `metadata` (typed JSON): stage, reviewed commit, next action, authorization,
  receipts. Continuous triage's richer state machine lives here; the board
  shows what it knows and the rest verbatim.
- `status`: today's board vocabulary plus `draft` and `backlog` for backlog
  boards.
- `holders` with a role: exactly one `owner` (does it; the coordination owner
  interviewees asked for), any number of `contributor`, `reviewer`,
  `approver`. Holding, reviewing or witnessing never changes anyone's manager.
- `group` (§7.3) and `team` (the owning team: defaults to the nearest staffed
  ancestor group, else the board's team; settable independently, as EP asked,
  so a topic group can mix teams).
- Backtests (`kind=backtest`) are not items and never go on boards; their
  status, done-PATCH VM teardown, preemption requeue and `archived` lifecycle
  are unchanged (§9.4).

### 7.3 Groups and points of contact

Board groups become first-class (`groups`: board, name, parent group,
order, description, optional team).

- A group **with a PoC** is backed by a team: setting a PoC creates (or links)
  a `kind='team'` team whose lead is the PoC and whose parent is the team of
  the nearest staffed ancestor group, else the board's team.
- A group **without** a PoC is a plain topic heading, as today.
- **Promotion is deliberate.** Owner or the board team's lead sets a PoC. CM
  then offers the group's current holders as members, **secondary by
  default**. Making someone a *primary* member (so that they report to the
  PoC) is a move (§6.2): Owner or that member's manager chain must confirm.
  CM never infers a manager from the group of someone's latest item. Setting
  a PoC who sits outside the board team's tree, or who would create a cycle,
  needs Owner or that PoC's manager chain.
- **Routing for an item:** its approver-role holder (for decisions) → its
  owning team's lead → the group's PoC → the board team's lead. Unassigned
  work and group-level flags go to the PoC first.
- A sub-team's items stay on the parent board as its group by default, so one
  board shows the whole plan; a team may get its own board later.
- Migration: `items.grp` free text becomes one group per distinct value per
  board, with no PoC.

> **Owner's direction vs the interview evidence.** Owner asked that a group
> with a PoC be a real team reporting to that agent; this design does that.
> The interviews pushed back: of eight respondents to the group question, all
> described today's groups as topic labels or filing headings, and several
> preferred a PoC that only routes questions, without a reporting line. Their
> cases: Latent (a natural team, led by Latent-Forecast-Lead, with external
> law reviewers), "Batching & capacity" (independent holders from several
> teams; no one reports to a single contact), and Batch-Loop, which answers to
> Backfill-Lead on #1751 and to EP on #1797. The design reconciles the two by
> making promotion deliberate, membership secondary by default, and the
> item's owning team independent of its group. **Owner decision O2** (§15)
> confirms this or switches PoCs to routing-only.

### 7.4 Assigning and delivering work

- Holders are members (from S6; pids before). Assigning pushes to the member's
  main session, which may delegate to an assistant (`via` the member). A
  replacement main session inherits holds; pending pushes retarget to it.
  `holder_gone` becomes `member_vacant`.
- The active-item cap of strict boards counts per acting session (main or
  assistant), not per member, so a lead with three working assistants does not
  trip `holder_overloaded`.
- Delivering an item to a session composes the brief from: the item's title,
  description, group, board and links; the team's charter link; and the
  member's and team's standing rules (§8.4). `start_session(item=…)` replaces
  `start_session(task_id=…)` prompt auto-delivery (`prompt_source="item"`).
  The spawn message (`start_session(prompt=…)`) stays: it is a message to a
  session, not a field of the work.
- Closing an item unassigns its holders. It never kills a main session or
  closes a home or stream workspace. Ephemeral workspaces close per §5.
- When an item is created or claimed, or a stream workspace opened, CM returns
  a `possible_duplicate` hint for open items with overlapping title tokens or
  paths (three agents building overlapping fixes for one root cause was an
  interview example).

### 7.5 Handover

Replacement and reassignment go through a durable, idempotent checklist:
`replacing → reconciling → ready`.

- The checklist lists what CM knows is attached: held items, pending pushes,
  open reviews, monitors, declared jobs (`items.job`), stream workspaces with
  their dirty state, and (later) leases. Ad-hoc locks and external resources
  CM cannot see (e.g. a bb-l4 `cell.lock`, a VM) must be declared on the item
  to be listed; unknown resources are shown as "not tracked".
- The new main session accepts or releases each entry. Until it does, those
  items show `handover` on the board and their routing falls back to the
  manager. Nothing transfers silently.

## 8. Profiles

### 8.1 Four facts kept apart

Interviewees kept separating these, and conflating them misroutes:

| Fact | Source | Example |
|---|---|---|
| **worked on** | evidence: items closed as `owner`, commits authored | "closed sejd#1248; 31 commits under `tui/src/app/orgchart.rs`" |
| **reviewed** | evidence: reviewer holds, receipts | "reviewed 12 law fixes" |
| **owns now** | live: items held as `owner`, teams led, declared jobs, OWNER.md files, VM labels | "PoC of Latent; holds #1751" |
| **may decide** | §6.5 sources, each with its source and supersession | "approver on #1797; gate grant for `source_change` on sejd" |

### 8.2 Fields

| Field | Written by | Limit |
|---|---|---|
| `title` | manager chain or Owner | ≤ 60 chars |
| `mandate` (what the member is for) | manager chain or Owner | ≤ 400 chars |
| `focus` | **derived**: current owner-role holds, recent `report_done` reasons, stream-workspace purposes | — |
| `focus_note` | self; expires after 7 days | ≤ 120 chars |
| `ask_me_about` | self; checked against evidence | ≤ 8 tags |
| `not_my_area` (route away: `deploys → swarm-coord`) | self or manager | ≤ 5 entries |
| `deputy` | manager | a member |
| `availability_note` | self; expires | ≤ 80 chars |
| everything else (manager, team, reports, assistants, workspaces, host and how to reach it, engine/model, past work, expertise, contacts) | derived | — |

Six of seven interview respondents wanted role and focus set by the lead, not
the agent; deriving `focus` from what the member actually holds settles that
and removes the field hardest to keep current. Authored values are stored as
`{value, set_by, via, set_at}` with history in `profile_events` and
compare-and-set writes.

Team profiles: `charter` (lead/Owner), PoC/lead, and derived members,
board groups, open items and evidence. Workspace profiles: `purpose` only.

### 8.3 Evidence and past work

Evidence rows are keyed by an immutable source key (item event id, commit
SHA + repo, receipt id, ruling id) plus the attribution role, so ingestion is
idempotent and replays never double-count.

| Producer | Status | Notes |
|---|---|---|
| item closed / handed off / reviewer and approver holds | exists (`item_events`, `closed_by`) | credit the `owner`-role holder, not the actor who clicked done |
| gate receipts | exists (`item_receipts`) | |
| `report_done` reasons | exists (tombstones) | daemon → API forwarder to build |
| commits authored | **to build** | a `prepare-commit-msg` hook (chained like CM's existing post-checkout hook) adds `CM-Member:` / `CM-Session:` trailers; one hub-side `git log` cursor per repo ingests them. Credits authors, not landers; cherry-picks keep trailers. Every agent commits as the same git user today, so branch-based attribution is not used |
| rulings answered / superseded | **external prerequisite** (doc/rulings.md has no code yet) | discovery runs without it until it ships |
| OWNER.md files, VM labels | **to build** (owns-now only) | periodic scan of `~/.local/share/*/OWNER.md` and the worker label set |
| mention edges | **to build** | routing evidence only (who gets asked), never proof of expertise |

**Past work** is the reverse-chronological list of closed items, authored
merged commits, receipts and (later) rulings, folded where they link.
Corrections (a revert, a reopened item, a superseded ruling) are recorded
against the original evidence and shown, not deleted.

**Expertise score** (provisional weights, tuned against the §10 metrics):
ruling answered 5, item closed as owner 3, authored commit on a matching path
2 (capped per branch), review 2, routed question answered 2. Self tags are a
separate lower tier that can only break ties among members with no evidence.
Anti-gaming: dropped items count 0; items closed by the same holder within 2
minutes of creation count half; a routed question counts only once answered,
and no member gets more than half of its team's routed questions.

No language model on the read path. v1 matching is token overlap using the
messaging whole-word index.

### 8.4 Standing rules and their delivery

Standing rules ("no pytest on cm", "kill by PID", "logs outside the
worktree") are the thing interviewees said is most at risk without the
prompt field.

- Rules live on the member and on the team (≤ 10 one-liners each, or a
  runbook path), each with its source: Owner rule, ruling id, or lead default.
- They are delivered in every item brief (§7.4), and the first `ping` after a
  spawn, resume or respawn returns a short digest (a hash plus the list).
- A change pushes a one-line notice to the member's main session.

### 8.5 Keeping it current

1. **Derive first.** Focus, current work, past work, expertise, availability
   and org position are computed.
2. **Nudge on drift, not on a schedule.** `report_done`, closing an item, a
   resume or respawn, and the first `report_done` after a long gap return a
   one-line `profile_hint` only when computed drift crosses a threshold (e.g.
   `ask_me_about` tags with no support in the last 10 closed items). This does
   not depend on observing compaction, which Codex does not expose.
3. **Show staleness.** `ask_me_about` tags without evidence show "unverified";
   a `mandate` older than 60 days whose work has shifted raises
   `profile_drift` to the manager at most once per two weeks, on the board
   flag path.
4. **Owner overrides** from the org chart and A-e.
5. **Suggestions, never edits, from a cheap model** (later): pending diffs
   that expire unless accepted.

### 8.6 Safety

Profiles, item titles, `report_done` reasons, ruling text and handoff notes
are written by agents and read by other agents. Every agent-written string in
tool output is wrapped as `{"untrusted_text": …}` with a fixed docstring
warning. Tags are limited to `[a-z0-9 ./_-]{1,40}`, fields are length-capped,
`why` refs are ids plus titles of at most 80 chars, and routing never depends
on authored text alone. There is no imperative-phrase blocklist (it rejects
legitimate text and misses paraphrases).

## 9. How existing things map

### 9.1 Continuous tasks

- A continuous task becomes a member with `kind='continuous'`. The mapping is
  `(daemon_id, continuous record id) → member_id`, with a separate nullable
  planning-task FK, because continuous ids are host-local strings that may
  have no planning row.
- Handles are normalized (lowercase, `[a-z0-9-]`, collision-suffixed); the
  original slug and `<slug>-orchestrator` stay as permanent aliases. State
  directories, queues, `ct/<slug>` channels and names are never renamed.
- The scheduler fills the member. In Persistent mode the same session stays
  main. In Fresh mode each fire opens a new main row and closes the previous
  one; the previous session, if still alive, is fenced (§4.3) and kept only
  for its own run's `report_done`. The scheduler's run-sequence and
  `report_done` ownership checks are unchanged.
- Its workers are assistants. Workers that hold items across runs can be
  hired into the continuous member's team.

### 9.2 Initiatives and divisions

They are teams (the same rows). `lead_member_id` is backfilled from today's
lead of the coordinator task. Boards anchored by `root_task_id` get a team
created for them (kind `team`, lead from the same rule). Initiative project
approval is unchanged.

### 9.3 Subtasks, forks, workflows

- `create_subtask` creates a child item (decomposition) and, as today, can
  mint an ephemeral workspace for an assistant to work it. The old one-call
  shape is kept as a compatibility wrapper.
- `fork_session` makes an assistant unless `hire=` is passed (§4.2).
- Workflow participants are assistants of the member whose workspace runs the
  workflow.

### 9.4 Backtests and cloud workers

Unchanged. New lifecycle behaviour applies only to adopted work (items with a
member holder, workspaces with a central record). The API identifies it by
`kind` and by whether the workspace record exists; `kind=backtest` keeps its
statuses (incl. `archived`), done-PATCH VM teardown, preemption requeue and
archive sweep. Cloud dispatch workers stay one-shot task runners.

### 9.5 The Owner

Owner is a member (`handle='owner'`, `kind='owner'`, no sessions), the root of
the manager tree and a valid PoC. Owner availability (F7) feeds derived
availability. `who` does not return Owner except for topics where an Owner
ruling or grant is the only authority; agents reach Owner through
`notify_user` or the Owner agenda.

## 10. Discovery tools (MCP)

```
org(view="me")                    # my member, manager chain, reports, assistants, team(s), standing rules
org(view="member", member=…)      # profile (four facts) + chain + reports + host/reachable_here + doing-now
org(view="team", team=…)          # lead, members, sub-teams, board, groups with PoCs, charter
org(view="directory", query=None, team=None, project=None)
who(topic, intent="explain"|"owner"|"approve", scope="team"|"org", available=False, limit=5)
    -> [{member, handle, title, team, score, why[≤3 checkable refs], availability, load,
         host, reachable_here}], teams[{team, poc, score}]
owner_of(path=None, component=None, resource=None, repo=None)   # authored commits, claims, OWNER.md, VM labels
who_decided(query=None, item=None, board=None)                  # once rulings exist
profile_set(member=None, …, expected_revision, request_id)      # write
ask(topic=None, to=None, text, item=None)                       # write, later: routes to a member or a team inbox
```

- Compatibility: today's `org(view="reports")` maps to `org(view="me").reports`,
  `view="division"` to `view="team"`, and `action="set_lead"` to a team-lead or
  manager edit. The old names stay as aliases (schema-compat rule).
- `chat_people` rows gain `member`, `title`, `team` and the doing-now line.
- **Intent filters first, then ranks.** `owner` considers only current
  owner-role holders, PoCs and leads; `approve` only §6.5 sources. An
  unavailable authority is shown with its deputy, never replaced by an expert
  without authority. `explain` ranks by expertise × availability (live idle
  1.0, working 0.8, needs_human/errored 0.3, vacant → deputy) × a load penalty.
- **Evidence.** Each row carries up to three refs an agent can check with
  existing tools (`sejd#1248 done 2d`, `main@c3d4e5f tui/src/app/orgchart.rs`,
  `receipt 812`). For code topics `owner_of` points at the author of the path,
  not whoever last talked about it. Discovery finds the person; agents still
  read the implementation before relying on a claim.
- **Scope and visibility.** CM has no project ACLs today; boards and channels
  are readable org-wide. So `who` uses only evidence the caller could already
  read: boards, public channels and channels it belongs to, never DMs.
  Evidence the caller cannot read is not counted at all (counting it would
  leak its existence). Owner sees everything.
- **Metrics.** v1: a `who` call followed within 30 minutes by a DM or mention
  to a returned member that gets a reply; `possible_duplicate` hints accepted;
  evidence coverage per team. Later, for `ask`: answered on first route,
  re-routes, and duplicate work started on the same item or path.
- **`ask` (later)** routes a question to the best eligible member, or to a team
  inbox whose PoC/lead triages it; it reuses rulings question records once
  they exist, merges duplicates, and caps open inbound questions per member
  (default 5). This is the direct fix for "EP as the human message bus".

## 11. Owner's view (TUI)

- **Org chart (F6)** is drawn from teams and managers: team boxes with the lead
  on top, members under it, manager lines between members, dotted markers for
  matrix cases, assistants folded under their member as a count with state
  glyphs (expandable; ended ephemeral ones hidden), vacant members drawn empty
  with their stand-in. Cards show handle and title; the info pane shows the
  four facts, mandate, focus, current items and inbound question count. `e`
  edits the profile; `/?topic` runs `who` and highlights matches.
- **Sessions sidebar** gains a Team sub-view (team → member → sessions,
  folding as today). The Task sub-view stays selectable until S8.
- **Planning view** shows backlog boards per team/project. `A-f` places an
  item on a board, with "assign to member" and "hire a new member for it".
- **Board overlay (Alt+B)** shows groups as a folding tree with a PoC chip on
  each staffed group; `g` on a group sets its PoC.
- **Roster screen** (S4): the proposed member backfill (member, handle, team,
  manager, kind) for Owner to approve or edit in one pass.
- **A-e** on a session edits its member's profile; on a team header, the
  team's charter and lead.

## 12. Persisted state

New fields are added without a manifest-schema bump (no `--hard` deploy) and
without changing the holder wire schema. Every place session state is
serialized or adopted must round-trip them:

| State | Where | New fields |
|---|---|---|
| durable manifest + tombstones | `daemon/src/manifest.rs`, `session.rs` | `member_id`, `occupancy_id`, `acting_as`, `workspace_record_id`, `binding_epoch` |
| reexec record (split and monolith) | `daemon/src/reexec.rs`, `holder-proto/src/reexec_manifest.rs` | carried in the opaque per-session blob |
| holder adoption and rollback | `daemon/src/holder_mode.rs`, `holder/src/holder.rs` (reparses the blob into its own `SessionRecord`) | must preserve unknown fields; fixture tests |
| resume identities | `daemon/src/resume_identity.rs` | `member_id` |
| TUI manifest and snapshot pushes | `tui-sessions.json`, `task.update_tree` | `member_id`; old TUI snapshots must not overwrite member/binding relations (`binding_epoch` guard) |
| continuous state | `~/.cm/continuous-tasks/<id>/` | unchanged; the member mapping lives centrally |

Rules:

- Each stage ships readers and round-trip support **one deploy before** any
  writer populates the fields (fixture tests: old file → new binary → old
  binary → new binary keeps every field).
- The central API is authoritative for member bindings; daemons cache them.
  After a brain restart or rollback the daemon rehydrates bindings from the
  cache and the API before admitting member-scoped control calls; until then
  those calls answer `org_unavailable` while ordinary session I/O continues.
- Both task→workspace reconstruction paths (`state.rs` rehydration and the
  `methods.rs` restore path) are gated on `binding_epoch`.
- If the API is unreachable at spawn, the session starts and binds to its
  member once the API answers; the one-main-session invariant is enforced at
  bind time, and a losing session becomes an assistant.

## 13. Migration

Each stage deploys alone, is reversible, and keeps old clients working.
Permission changes run in shadow before they enforce.

| Stage | What | Notes |
|---|---|---|
| **S0 guards** | NULL-safe `prompt` readers (`/workers`, CLI, `worker/startup.sh` falls back to description); `propose_task` files `draft` as documented (confirmed: both clients omit status and the API defaults to `backlog`; check the dispatcher predicate before shipping); TUI adoption keeps taskless sessions; persisted-state readers and fixtures for the §12 fields | no visible behaviour change except the draft fix |
| **S1 prompt → description** | inventory first (real instructions in prompts, empty descriptions, continuous worker contracts → continuous standing instructions); compatibility table below; one marker-gated backfill appends real prompts under `## Instructions`; description-only delivery turns on only after the backfill and every dispatch path read description | the `prompt` **column is never dropped** (`sql/003` alters it on every start); the MCP param stays as a deprecated alias |
| **S2 groups, PoC, placement** | `groups` table, nesting, PoC (stored as a pid until S4 converts it); `coordinator_task_id` nullable + API model changes; PoC teams; backlog boards; place/unplace/move with `work_id` and `item_aliases`; item roles and single owner; description versioning | Owner's asks 1 and 5; no identity changes needed |
| **S3 stop firing employees** | classify existing workspaces (agent-minted `cm-sub/*` subtask and isolated ones → ephemeral; Owner-created and continuous → home); terminal sweep, `mark_subtask_done` and the reaper keep reclaiming ephemeral workspaces exactly as today and stop killing sessions in home/stream ones. Task bindings, spawn promotion and scope are **not** touched | per-host flag; backtests unchanged |
| **S4 members, read-only** | `members`, `incarnations`, `team_memberships`, `workspaces` tables with change feeds; Owner-approved roster (continuous members, initiative/division leads, long-lived lanes; dedupe by pid, coordinator and continuous ownership; handle collision policy: suffix, never steal an alias); `manager_member_id` backfilled from today's lead rule; daemons stamp `member_id`/`acting_as`; `org`, `ping`, A-i, F6 show members | nothing enforces |
| **S5 shadow scope + profiles v1** | manager-chain scope computed and logged beside the old rule (old authoritative, §6.3); `profile_set`, `org(view=member/team/directory)`, `who` over items, receipts, `report_done` and tags; commit trailer hook; standing rules delivery | |
| **S6 member behaviour** | spawn defaults (§4.2) and `hire`; `@handle` → main session; holds by member with all consumers updated together (holder checks, flags, capacity, pushes retargeted on replacement, board orchestrator/steward as members); fencing; handover checklist; new workspaces with records and `cm-ws/` branches; models resolve member before task | consumers roll before inherited holds turn on |
| **S7 scope cutover** | per host, after a week of shadow logs shows only expected differences: manager-chain scope authoritative; board edit by team + own holds; `agent_task_edges` retired | rollback = flip back to old rule |
| **S8 cleanup** | `start_session(item=…)`, Task sub-view becomes a filter, old `org` view names kept as aliases; decide full table convergence (§16) | |

**S1 compatibility table** (create and PATCH):

| Client sends | Result |
|---|---|
| description only | description stored; `prompt` untouched |
| prompt only (old client) | folded into description under `## Instructions` if non-empty, ≠ name, ≠ description and not contained in it |
| both, prompt equal to or contained in description | prompt ignored |
| both, differing (old editor round trip) | description wins; prompt is kept only in a `legacy_prompt` metadata field for provenance, never re-folded |
| responses | `prompt: ""` (not an echo of the description, which old composers would deliver twice) |

Composers (daemon `compose_task_launch_prompt`, TUI `compose_launch_prompt`,
dispatch) skip a prompt that is empty, equal to or contained in the
description. Acceptance cases: old-editor round trip, an instruction deleted
from the description, a prompt-only PATCH, API/daemon version skew.

**Acceptance cases for the whole migration:** same-uid revive and model
change; Persistent and Fresh continuous fires; replacement while a push or a
gate grant is pending; a manager finishing its own item while its reports keep
working, then managing them after a brain restart; old TUI/editor and old
hub/daemon combinations; API outage during a brain restart and during a spawn;
API started twice on a migrated database; backtest success, preemption and
archive.

Other hazards (details in the impact map): `sql/005` re-creates the status
CHECK on every start, so new statuses need 005 edited plus a later "replace
only if missing" block; backfills run once behind `migrations_applied` as
single statements; the auto-dispatch predicate (`backlog ∧ is_cloud ∧ project
IS NULL`) must not start matching ordinary items.

## 14. Schema sketch (planning DB)

```sql
-- teams: evolve initiatives in place (a `teams` view aliases the table)
ALTER TABLE initiatives ADD COLUMN IF NOT EXISTS parent_team_id UUID REFERENCES initiatives(id);
ALTER TABLE initiatives ADD COLUMN IF NOT EXISTS lead_member_id TEXT;
ALTER TABLE initiatives ADD COLUMN IF NOT EXISTS charter TEXT NOT NULL DEFAULT '';
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
  role TEXT NOT NULL,                        -- main | assistant
  pid TEXT NOT NULL, daemon_id TEXT NOT NULL, session_uid TEXT NOT NULL,
  workspace_id TEXT, predecessor BIGINT REFERENCES incarnations(id),
  started_at TIMESTAMPTZ NOT NULL, ended_at TIMESTAMPTZ, end_reason TEXT
);
CREATE UNIQUE INDEX incarnations_one_main ON incarnations (member_id)
  WHERE role = 'main' AND ended_at IS NULL;
CREATE UNIQUE INDEX incarnations_open_pid ON incarnations (pid) WHERE ended_at IS NULL;
CREATE TABLE team_memberships (team_id UUID, member_id TEXT, role TEXT,   -- primary | secondary | guest | deputy
  since TIMESTAMPTZ, until TIMESTAMPTZ, PRIMARY KEY (team_id, member_id, role));
CREATE TABLE workspaces (id TEXT PRIMARY KEY, idem_key TEXT UNIQUE, daemon_id TEXT, path TEXT,
  repo TEXT, branch TEXT, base_sha TEXT, kind TEXT,  -- home | stream | ephemeral
  office_of TEXT REFERENCES members(id), team_id UUID, purpose TEXT,
  state TEXT,                                -- creating | ready | failed | closed
  created_at TIMESTAMPTZ, closed_at TIMESTAMPTZ);
CREATE TABLE groups (id UUID PRIMARY KEY, board_id UUID REFERENCES boards(id),
  parent_group_id UUID REFERENCES groups(id), name TEXT, description TEXT, ord INT,
  team_id UUID REFERENCES initiatives(id));
ALTER TABLE items ADD COLUMN IF NOT EXISTS work_id UUID;        -- = task id when placed from a task
ALTER TABLE items ADD COLUMN IF NOT EXISTS task_id UUID UNIQUE REFERENCES tasks(id);
ALTER TABLE items ADD COLUMN IF NOT EXISTS description TEXT;
ALTER TABLE items ADD COLUMN IF NOT EXISTS description_rev INT NOT NULL DEFAULT 0;
ALTER TABLE items ADD COLUMN IF NOT EXISTS metadata JSONB;
ALTER TABLE items ADD COLUMN IF NOT EXISTS group_id UUID REFERENCES groups(id);
ALTER TABLE items ADD COLUMN IF NOT EXISTS team_id UUID REFERENCES initiatives(id);
ALTER TABLE item_holders ADD COLUMN IF NOT EXISTS role TEXT NOT NULL DEFAULT 'owner';
ALTER TABLE item_holders ADD COLUMN IF NOT EXISTS member_id TEXT;   -- authority key from S6
ALTER TABLE item_holders ADD COLUMN IF NOT EXISTS via_member_id TEXT;
CREATE TABLE item_aliases (board_id UUID, number INT, work_id UUID, PRIMARY KEY (board_id, number));
-- member_id + acting_as beside every pid column: item_events, item_receipts, boards.orchestrator/steward, ...
CREATE TABLE profile_events (id BIGSERIAL, subject TEXT, field TEXT, prev JSONB, new JSONB,
  actor JSONB, via TEXT, at TIMESTAMPTZ);
CREATE TABLE org_events (id BIGSERIAL, subject_type TEXT, subject_id TEXT, type TEXT,
  actor JSONB, prev JSONB, new JSONB, reason TEXT, at TIMESTAMPTZ);   -- hire, replace, move, retire, poc_set
CREATE TABLE evidence (id BIGSERIAL, source_key TEXT, role TEXT, member_id TEXT, pid TEXT,
  kind TEXT, ref TEXT, project TEXT, visibility TEXT, topic_tokens TEXT[], paths TEXT[],
  weight REAL, corrected_by TEXT, at TIMESTAMPTZ, UNIQUE (source_key, role, member_id));
```

New tables get their own change feed (members, teams, workspaces, groups) so
viewers never poll.

## 15. Decisions

### 15.1 Owner decisions

These change what Owner gets, so they are Owner's call. Each has a
recommendation.

- **O1. The member is the durable employee** (not the workspace, not the
  session). Recommended, for the reasons in §1.1. Rejected alternatives: the
  workspace as employee (breaks on multiple checkouts, host moves and fresh
  continuous runs); the session as employee with successor links (keeps every
  current pain); a generic node/edge graph (moves the invariants into code).
- **O2. A group with a PoC is a team whose primary members report to the
  PoC**, with deliberate promotion and secondary membership by default
  (§7.3). Recommended as drafted. Alternative the interviewees preferred: a
  PoC that only routes questions and owns the group, with no reporting line.
- **O3. Link-first for tasks and items in v1** (§7.1): place a task onto a
  board as a linked item that becomes the status authority; decide full
  convergence later. Recommended. Alternative: merge the tables now (largest
  migration in the design; delays everything else).

### 15.2 Design decisions (override any)

- D1. Hiring is explicit; every other spawn is an assistant, with an
  ephemeral workspace when it writes code.
- D2. The manager is stored and cycle-checked; teams group, and agree with
  managers by default.
- D3. Management scope follows the manager chain only; team membership never
  extends it; rollout is shadow-first.
- D4. Assistants act under their own name "for" the member; only the main
  session or a delegated assistant acts as the member.
- D5. `focus` is derived, with an expiring self note; title and mandate are
  set by the manager chain.
- D6. No member mail in v1: `@handle` reaches the current main session, and
  continuity comes from the handoff record and standing rules.
- D7. The `prompt` column stays physically (nullable, unused); only the wire
  field is deprecated.
- D8. No grants registry in v1; "may decide" comes from item approver roles,
  existing gate grants and Owner rulings.
- D9. Discovery counts only evidence the caller could already read.
- D10. Retired handles are never reused; references are org-qualified.

## 16. Open questions

1. **Full convergence** (after O3 link-first is in use). Preconditions: one
   canonical id (`work_id`) everywhere, a single status authority already in
   place, dependency and receipt FKs remapped, an adapter for old clients
   that write tasks, a rollback path.
2. **Member mail** (after D6). Whether later main sessions need readable
   history in practice; if so, the contract in §4.4.
3. **Grants registry** (after D8). Who seeds it and when; it comes with the
   approval queue.
4. **Assistants that live long without being promoted.** Should CM suggest
   promotion (e.g. an assistant alive 3 days with owner-role holds)?

## 17. Non-goals

- No performance scoring or HR metaphors beyond hire/move/retire.
- No transfer of transcripts or private memory between sessions.
- No concurrent main sessions per member; one manager each (dotted lines are
  team memberships).
- Projects do not become org units.
- No new permission enforcement beyond management scope and board edit rules.
- Backtests and cloud dispatch workers are not members or items.
- No cross-org federation now; `org_id` and qualified references keep the
  door open (a guest from another org gets item roles and channel access,
  never management scope).

## 18. Inputs

- Brief: `~/.local/share/swarm-coord/org-model/brief.md`
- Brainstorms: `…/brainstorm/entity-model.md`, `profiles-discovery.md`,
  `impact-map.md` (file:line references for the change sites)
- Interviews (Swarm-Gardener): `…/interviews/summary.md`, verbatim replies in
  `interview-log.md`
- Reviews, round 1: `…/reviews/codex.md` (8 blockers), `…/reviews/claude.md`
  (4 blockers); all blockers are addressed in this revision
