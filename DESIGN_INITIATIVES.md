# Design: First-class initiatives

**Status:** Approved design; v1 implementation is in progress.

## 1. Goal

Make an **initiative** a first-class Claude Manager planning object. An initiative groups related tasks and subtasks, records the Owner-approved scope, connects the participating codebases, and provides one durable coordination identity for the message board and shared project documents.

The vocabulary is deliberately small:

```
Project (a codebase)
└── Initiative
    └── Task
        └── Subtask
```

Tasks and subtasks may also exist without an initiative. A cross-project initiative is an initiative whose membership includes more than one project. A single-project initiative has one project membership. We do not add official “workstream” or “swarm” objects.

## 2. Motivation

Today `tasks.project` is a free-form codebase name and `parent_task_id` supplies task nesting. There is no durable object for the work that spans several tasks, several repositories, or several CM sessions. Agents therefore encode initiative identity in prompts, local documents, or channel names, which makes discovery, filtering, Owner review, and migration fragile.

The first target use case is the Swarm Design effort connecting the Claude Manager and Prediction Trading codebases. The Claude Manager side will develop coordination features; the Prediction Trading side will request, test, and onboard those features. Both sides need a shared conversation as well as project-specific conversations, while the Owner retains approval over starting a new initiative and expanding its project membership.

## 3. Locked decisions

1. **Project means codebase.** Keep the existing `project` field as the codebase key used by planning, repository lookup, and task filtering.
2. **Initiative is the grouping object.** Do not overload `project` with initiative names or use task names as a substitute.
3. **Tasks remain the execution unit.** An initiative owns or links tasks; it does not replace task lifecycle, worktrees, sessions, subtasks, or task permissions.
4. **Initiative membership is explicit.** A task has at most one initiative. An initiative may contain tasks in one or many projects.
5. **Standalone work remains valid.** A null `initiative_id` means the task is not part of an initiative. Existing tasks remain standalone after migration.
6. **Cross-project is a property, not a separate object.** The UI and API derive it from the number of approved project memberships.
7. **Owner approval is a lifecycle transition.** Agents may propose an initiative or a new project membership, but only Owner-authorized planning operations may activate an initiative or approve a membership.
8. **No official swarm/workstream model.** Agent groupings remain tasks, subtasks, sessions, and message-board channels.
9. **The message board remains the communication system of record.** Initiative records store stable channel addresses and provisioning state; they do not duplicate messages.
10. **Documents remain in git.** The initiative record points to its coordinator task/worktree and `cm-initiative/` docs; it does not store the charter or plan as database blobs.

## 4. Core object

An initiative has a stable UUID and a human-readable slug. Its lifecycle is:

```
draft → active → paused → active
                  └────→ completed → archived
draft ────────────────→ cancelled
```

Only Owner-authorized callers may move an initiative into `active`, add or approve a project membership, complete it, archive it, or cancel it. A coordinator may edit its plan documents and update task relationships within approved scope. A draft is visible but must not be treated as permission to launch work. Every initiative has a coordinator task; creation must supply one or create it atomically as part of setup.

Minimum fields:

| Field | Meaning |
|---|---|
| `id` | UUID, immutable identity |
| `slug` | Stable URL/channel-safe name, unique among initiatives |
| `name` | Display name |
| `description` | Short purpose statement |
| `status` | `draft`, `active`, `paused`, `completed`, `archived`, or `cancelled` |
| `color` | Optional named accent used for planning subsection tinting |
| `coordinator_task_id` | Required task that hosts the coordinator session and hub worktree |
| `coordinator_project` | Codebase containing the coordinator task/worktree |
| `docs_path` | Relative path, normally `cm-initiative/` |
| `shared_channel` | Stable message-board channel path, normally `initiative/<slug>` |
| `created_at`, `updated_at` | Server timestamps |
| `approved_at`, `approved_by` | Approval audit for activation; the actor is taken from authenticated CM context |
| `metadata` | Versioned extension bag for non-core details |

The coordinator task is a normal task. It may belong to the initiative, but the initiative record must not depend on a live session: an initiative remains inspectable when its coordinator is stopped or its worktree is temporarily unavailable.

## 5. Project membership

Projects are currently represented by the existing project name string and repository mapping. Membership is a separate relation so a cross-project initiative can link `claude-manager` and `predictionTrading` without changing the meaning of either project.

```sql
CREATE TABLE initiatives (
  id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  slug TEXT NOT NULL UNIQUE,
  name TEXT NOT NULL,
  description TEXT NOT NULL DEFAULT '',
  status TEXT NOT NULL DEFAULT 'draft',
  color TEXT,
  coordinator_task_id UUID NOT NULL REFERENCES tasks(id) ON DELETE RESTRICT,
  coordinator_project TEXT,
  docs_path TEXT NOT NULL DEFAULT 'cm-initiative',
  shared_channel TEXT,
  approved_at TIMESTAMPTZ,
  approved_by TEXT,
  metadata JSONB,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE initiative_projects (
  initiative_id UUID NOT NULL REFERENCES initiatives(id) ON DELETE CASCADE,
  project TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'proposed',
  project_channel TEXT,
  role TEXT NOT NULL DEFAULT '',
  proposed_by TEXT,
  approved_at TIMESTAMPTZ,
  approved_by TEXT,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (initiative_id, project)
);

CREATE TABLE initiative_events (
  id BIGSERIAL PRIMARY KEY,
  initiative_id UUID NOT NULL REFERENCES initiatives(id) ON DELETE CASCADE,
  actor TEXT NOT NULL,
  event_type TEXT NOT NULL,
  project TEXT,
  previous_value JSONB,
  new_value JSONB,
  reason TEXT,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

`initiative_projects.status` is `proposed`, `approved`, or `removed`. Removing a membership does not delete its tasks or messages; it prevents new tasks from being assigned to that project under the initiative unless the Owner approves it again.

The first version does not add an `owner_id` column. CM currently authenticates an Owner through the operator/API authorization layer, but it has no durable multi-user account table or account UUID contract. Approval and lifecycle audit rows record the authenticated actor string and token/principal provenance. If CM later gains accounts and delegation, `owner_id` can become a foreign key in a follow-up migration without changing initiative identity.

The first migration should keep `project` as text. A normalized `projects` table can follow later if repository identity, aliases, and ownership need stronger guarantees; that is not required to make initiatives first-class.

## 6. Task relationship

Add a nullable `initiative_id` foreign key to `tasks`:

```sql
ALTER TABLE tasks ADD COLUMN initiative_id UUID
  REFERENCES initiatives(id) ON DELETE SET NULL;
CREATE INDEX idx_tasks_initiative_id ON tasks (initiative_id)
  WHERE initiative_id IS NOT NULL;
```

Creation rules:

- A task with no `initiative_id` remains standalone.
- A task assigned to an initiative must name a project that has an approved membership, except for the required coordinator task created as part of initiative setup, which may use the coordinator project while membership is being approved in the same transaction.
- A new subtask inherits its parent's `initiative_id` and `project` by default.
- Callers may explicitly assign a different approved project in the same initiative, which is how a cross-project initiative gets tasks on both sides.
- A caller may explicitly create a standalone top-level task with `initiative_id = null`.
- A subtask whose parent is standalone cannot be assigned to an initiative. The API rejects that combination; first make the parent part of the initiative or create the task at the initiative's top level. A child cannot name a different initiative from its parent either.
- Existing `parent_task_id`, worktree, branch, and session authorization rules remain unchanged.

The API validates the task/project/initiative relation. A database composite foreign key is not required in v1 because the current project registry is a free-form string and task creation already performs application-level project validation.

## 7. Owner checkpoints and authorization

The object makes the important checkpoint machine-readable without turning ordinary task execution into a new approval system.

### Initiative creation

`POST /initiatives` and `propose_initiative` create a `draft` and require `coordinator_task_id` (or an atomic coordinator-task creation request). Draft creation is safe for an agent because it does not activate work, grant access, or create sessions. The Owner approves activation through the planning UI or an Owner-authorized API call.

### Project membership

Adding a project creates a `proposed` membership. Activation of a cross-project initiative requires every participating project membership to be `approved`. An agent cannot silently add a second codebase to an active initiative.

### Task and subtask launch

Task launch continues to use the existing task/session authorization model. Initiative membership supplies context and filtering; it does not grant global session permissions or authorize an agent to spawn arbitrary tasks. The `setup-project` skill remains responsible for the conversational Owner gate before creating and launching initial subtasks.

### Audit

Every lifecycle and membership transition records actor, previous value, new value, timestamp, and reason in an append-only audit table or equivalent planning event log. The audit is needed to answer “who approved this initiative/project side?” without relying on chat history. The actor is an authenticated CM operator/API principal, not an agent-supplied owner field.

## 8. API surface

The cloud planning API owns durable initiative state. Names below are illustrative and should follow existing endpoint conventions.

### Initiative endpoints

```text
POST   /initiatives
GET    /initiatives?status=&project=&include_archived=
GET    /initiatives/{initiative_id}
PATCH  /initiatives/{initiative_id}
POST   /initiatives/{initiative_id}/approve
POST   /initiatives/{initiative_id}/pause
POST   /initiatives/{initiative_id}/complete
POST   /initiatives/{initiative_id}/projects
PATCH  /initiatives/{initiative_id}/projects/{project}
GET    /initiatives/{initiative_id}/tasks
```

`GET /initiatives/{id}` returns the initiative, approved/proposed memberships, channel paths, coordinator location, and task counts. It does not inline every task by default; the task endpoint supplies the filtered list.

### Task endpoints

Extend `TaskCreate`, `TaskUpdate`, and `TaskResponse` with nullable `initiative_id` and an optional compact `initiative` summary (`id`, `slug`, `name`, `status`). Extend task listing with `initiative_id` and `project` filters. Preserve backward compatibility for clients that ignore unknown response fields.

### MCP tools

Add read/propose tools first:

```text
propose_initiative(name, slug?, description, coordinator_project?, metadata?)
list_initiatives(status?, project?, include_archived?)
get_initiative(initiative_id | slug)
list_initiative_tasks(initiative_id, project?, status?)
```

Owner-authorized mutation tools may follow:

```text
approve_initiative(initiative_id)
propose_initiative_project(initiative_id, project, role?)
approve_initiative_project(initiative_id, project)
update_initiative(initiative_id, ...)
```

`create_subtask` gains an optional `initiative_id`; omitted means inherit from the parent task. Passing an initiative from another task tree remains subject to existing authorization and Owner checkpoint rules.

## 9. Message-board integration

Channel provisioning is an orchestration step, not a second persistence model. Once an initiative is approved, the coordinator creates or reconciles:

- shared channel: `initiative/<initiative-slug>`;
- one channel per approved project: `initiative/<initiative-slug>/<project-slug>`.

The shared channel path is stored on `initiatives.shared_channel`; each project channel is stored on `initiative_projects.project_channel`. Creation is idempotent and uses the channel API's stable paths/request IDs. The kickoff message links the initiative ID, charter, plan, protocol, and project list. The initiative API must not claim channels exist until provisioning succeeds; `metadata.channel_provisioning` may record pending/error state for retry.

Cross-machine visibility follows existing message-board enrollment and sync rules. An initiative record can be global in the planning API while a channel remains locally cached or offline; the UI must show sync state instead of hiding that distinction.

## 10. Documents and worktrees

The initiative object points to, but does not replace, git-backed coordination files:

```text
<coordinator worktree>/cm-initiative/
├── CHARTER.md
├── PLAN.md
├── PROTOCOL.md
├── PROJECTS.md
└── shared/
```

`CHARTER.md` is the Owner-approved purpose and boundaries. `PLAN.md` tracks tasks, milestones, decisions, and open questions. `PROTOCOL.md` defines merge, communication, verification, and checkpoint rules. `PROJECTS.md` maps project names to worktrees, coordinator tasks, and channels. `shared/` contains artifacts used by more than one project side.

The existing `project-setup` skill remains appropriate for a single codebase hub. The `setup-project` skill is the initiative entry point and calls the initiative API; `cm-initiative/` and channels are the git-backed and communication surfaces attached to the first-class record.

## 11. TUI planning experience

The planning view should expose the declared hierarchy without forcing standalone tasks into a fake initiative. Reuse the existing planning subsection/header mechanism (`GridItem::Header`, fold state, and layout persistence) as the visual primitive; this is a display grouping, not another planning object:

```text
Claude Manager
  ▾ Swarm Enablement Initiative  [cross-project] [active]
    Build initiative model       backlog
      Add API schema              backlog
  ▾ Standalone
    Unrelated task               in progress
Prediction Trading
  ▾ Swarm Enablement Initiative  [linked] [active]
    Onboard first swarm           planned
```

Required v1 behavior:

- project remains the top-level codebase filter;
- initiative-backed subsections group initiative tasks inside each project view;
- standalone tasks remain visible in a standalone subsection;
- the same cross-project initiative identity renders consistently in each project;
- task detail shows initiative name, status, project memberships, channels, and coordinator docs;
- initiative detail shows lifecycle, Owner approval, projects, coordinator task, and task counts;
- creating a task or subtask offers an initiative picker filtered to the selected project;
- archived/completed initiatives can be hidden without hiding their task history.

### Subsection boundaries and color

The current subsection rendering marks the top of a group but makes the bottom ambiguous. Initiative subsections should make the whole group legible without turning the board into a set of loud colored panels:

- Give each initiative subsection a stable accent derived from its stored color, with a muted background tint applied to the subsection header and its member rows.
- Use the existing terminal color palette and a deterministic fallback color; blend toward the normal background so text contrast and the selected-row style remain unchanged.
- Draw a one-row dim bottom rule after the last visible member of each subsection. The rule is derived from the visible projection, so it remains correct when tasks are folded, filtered, archived, or moved.
- Keep the top header, fold arrow, and existing header selection treatment. The bottom rule must not become an extra selectable task or alter task ordering.
- Apply the same treatment to manually authored subsections, with their color stored in the project layout sidecar. Old layout files parse with the default muted header style.
- Do not color the entire project column or the Status sub-view; the boundary belongs to the subsection only.

The exact RGB values and glyph choice can be tuned during the TUI slice, but the acceptance condition is visual: a user can identify both the beginning and end of a subsection at a glance on a narrow terminal.

No new “swarm” or “workstream” column is needed.

## 12. Migration and rollout

1. **Schema:** add `initiatives`, `initiative_projects`, audit storage, and nullable `tasks.initiative_id`; deploy idempotent migrations.
2. **API model:** add Pydantic models, CRUD, membership validation, lifecycle authorization, task filters, and response summaries.
3. **MCP:** add read/propose tools and initiative-aware task/subtask fields. Keep old task calls valid.
4. **TUI:** render initiative labels and hierarchy, then add create/edit/approve flows.
5. **Skill:** update `setup-project` to create a draft initiative, wait for charter and plan approval, approve/provision channels, and attach tasks by `initiative_id`.
6. **First migration:** create the Swarm Design initiative as a draft, attach Claude Manager and Prediction Trading as proposed memberships, review the charter, then activate it through the Owner flow.
7. **Backfill:** do not guess initiative membership for existing tasks. They remain standalone until explicitly attached.

Each slice must be deployable with old clients. Unknown initiative fields are ignored by older TUI binaries, and nullable task fields preserve existing rows and task creation.

## 13. Testing

- migration tests create a fresh database and upgrade an existing database with tasks, subtasks, continuous tasks, and backtests;
- API tests cover draft creation, Owner-only activation, membership approval, cross-project membership, invalid task/project assignment, and standalone tasks;
- task tests cover initiative inheritance for `create_subtask`, explicit standalone override, and cross-project assignment within one approved initiative;
- MCP tests cover list/get/propose behavior and backward-compatible task responses;
- message-board integration tests verify idempotent shared/project channel provisioning and failed provisioning retry state;
- TUI tests cover project → initiative → task rendering, linked cross-project identity, standalone visibility, filtering, and detail views;
- end-to-end smoke test creates a draft, approves two project sides, provisions channels, creates tasks on both sides, launches one approved subtask, and confirms the Owner audit trail.

## 14. Risks and open decisions

### Risks

- Free-form project names can drift (`Claude Manager` vs `claude-manager`); the first migration should define canonical names and aliases without forcing a projects-table rewrite.
- A task may outlive its initiative; `ON DELETE SET NULL` preserves the task as standalone if an initiative is removed, but deletion should normally be replaced by archival.
- TUI hierarchy changes can make large planning boards harder to scan; retain project filtering and a flat status view.
- Channel creation can succeed partially; provisioning state and stable request IDs are required for safe retries.

### Open decisions

1. **Resolved:** An initiative requires a coordinator task. Creation must receive one or create it atomically.
2. **Resolved:** Do not add `owner_id` in v1. CM has no durable account UUID model; use the authenticated operator/API actor in the audit trail. Add an account foreign key only when CM gains that identity system.
3. **Resolved:** Completed initiatives permit new task attachments. Attaching a task does not silently reactivate the initiative; the Owner may explicitly reopen it if active coordination resumes.
4. **Resolved:** Initiative grouping reuses the existing planning subsection/header system. Initiative subsections get a subtle background tint and a bottom boundary so the end of a group is visible, while standalone tasks and manually authored headers continue to work.
5. **Resolved:** A standalone parent cannot gain an initiative child, and a child cannot name a different initiative from its parent. The API rejects both combinations.

## 15. Non-goals

- replacing tasks, subtasks, worktrees, or session permissions;
- introducing official workstream, swarm, pipeline, or agent-team objects;
- normalizing every existing project name in the first migration;
- storing charter/plan contents in the database;
- automatically launching agents when an initiative becomes active;
- allowing agents to activate initiatives or add project sides without Owner authorization.
