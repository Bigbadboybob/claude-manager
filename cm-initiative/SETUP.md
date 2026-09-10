# Swarm Focused Design — Setup record

Status: **Setup complete; both coordinators and all nine research scouts grouped**,
2026-09-10. The [charter](CHARTER.md) is approved. This document records
infrastructure setup. The substantive, interactive work plan is [PLAN.md](PLAN.md).

Owner's further review identified two omitted setup checks: Owner must belong
to every chat and the existing tasks need a Work / Task sidebar subsection.
Owner membership is verified in all three channels. Owner created the laptop
subsection; both coordinator workspaces, all eight proposal scouts, and the
separate Fable-Scout research workspace are now verified grouped, with evidence
below. Earlier completion statements in the decision log record the narrower
checks done at their time.

## Sidebar grouping verification

The laptop TUI's **Work / Task sidebar** subsection is **Swarm-Focused-Design**,
ID `sec-18d3da776be0a234`, blue. All requested grouping is complete.

- **CM coordinator — directly verified by Initiative-Bridge:** `sidebar.list`
  reports `viewer_published=true`; workspace `ws-18d3bae538b649c9` has
  `observed.choice` and `observed.effective_section_id` equal to the subsection
  ID, with `pending=null`. Its receipt is null because this is an existing
  assignment, not a newly queued change. No assignment mutation was needed.
- **predictionTrading coordinator and scouts — verified by Swarm-Gardener:**
  all six standalone P1–P6 scout workspaces have applied receipts, matching
  effective section IDs, and no pending request. P7/P8 share the coordinator
  workspace, also observed in this subsection. Completion report:
  `37db72a8-da6c-444c-b0d0-64daa6dc6fb3:c9950088-dec7-41e2-8d19-bf577e19ab76`.
- **Fable-Scout — verified by Fable, relayed by Swarm-Gardener:** the earlier
  research task `59a67007-7a71-412a-87d4-bf535df42893` is separate from P1–P8.
  Fable assigned workspace `73253516d3a241619a5f497a658a1977` using its own
  session `ts-18d3da59b187e3f9-1`; assignment
  `7b03487e-9647-47b1-b736-41c9163f4da3` has receipt status `applied`, matching
  observed choice and effective section ID, and `pending=null`. This resolves
  the earlier coordinator-scope limitation without changing permissions.
  Completion report:
  `37db72a8-da6c-444c-b0d0-64daa6dc6fb3:eab81204-b9bf-4a8b-9438-6f976f8b0f24`.

The deployed sidebar tools now support scoped remote assignment to existing
sections. Both coordinators used the documented shell fallback with their own
CM session identities because their MCP tool lists were cached. Future tasks
should use `list_sidebar_sections` and `set_session_section`, or that fallback,
and verify observed membership after any queued assignment. Reference:
[sidebar sections at `d0dfb5e`](https://github.com/Bigbadboybob/claude-manager/blob/d0dfb5e7aaae48cc2aa43cd300fb708cb82d3b28/doc/sidebar-sections.md).
The older sidebar document on this coordinator branch predates those tools.
Section definitions remain viewer-owned; native initiative membership and the
coordinator glyph are separate from this display grouping.

## Approved setup

Create one initiative named **Swarm Focused Design**, slug
`swarm-focused-design`, with these two project memberships and existing tasks:

| Project | Membership role | Existing task to link | Coordinator |
|---|---|---|---|
| `claude-manager` | Initiative coordination and CM capabilities | `Swarm focused design CM` — `3b58ab69-3c89-4dee-bed4-c714858b0656` | Initiative-Bridge; initiative entry point and native coordinator |
| `predictionTrading` | Find suitable work, support approved swarms, and evaluate coordination practices | `Swarm focused design` — `f279464b-59da-4ba2-bce6-7bdb0a450847` | Swarm-Gardener; predictionTrading entry point |

The CM hub is `/home/lucas/.cm/worktrees/claude-manager-swarm-focused-design-cm`
on `cm/swarm-focused-design-cm`. The predictionTrading side remains in
`/home/lucas/.cm/worktrees/predictionTrading-swarm-focused-design` on
`cm/swarm-focused-design`. Preserve both tasks' identities, history, sessions,
and worktrees. Linking them requires no new worker sessions or subtasks.

Create and record these channels:

| Channel | Purpose |
|---|---|
| `initiative/swarm-focused-design` | Shared goals, decisions, proposals, results, and cross-project blockers |
| `initiative/swarm-focused-design/claude-manager` | CM coordination features and local implementation discussion |
| `initiative/swarm-focused-design/prediction-trading` | Suitable task ideas, approved swarm support, evaluation, and local discussion |

## Setup work

These are responsibilities within the existing coordinator tasks.

| Work | Responsible coordinator | Owned paths or records | Dependencies | Verification | Status |
|---|---|---|---|---|---|
| Native initiative and task links | Initiative-Bridge | New initiative and its two memberships; initiative association on the two named tasks | Owner approval of this plan, memberships, and activation through the supported Owner flow | Read back the initiative, coordinator, memberships, task links, and approval fields | Complete |
| Shared onboarding and protocol | Initiative-Bridge, with Swarm-Gardener review | CM hub `cm-initiative/` | Approved charter and setup plan; fill in native ID when created | Both coordinators can follow links to the same current docs and identify ownership and checkpoints | Complete; bootstrap `867d8ee` pushed |
| predictionTrading onboarding link | Swarm-Gardener | `agent_docs/swarm-focused-design-onboarding.md` in its existing worktree | Shared docs and initiative ID | Links identify the canonical charter, local role, shared artifact index, channels, and approval boundaries | Complete; `ae3724a69` pushed and reviewed |
| Channels and kickoff | Initiative-Bridge; Swarm-Gardener joins shared and predictionTrading channels | Three channel paths above and channel references on native records | Plan approval; initiative and docs ready | Read back all channels, pinned kickoff, participant membership, and stored channel paths | Complete; pins and memberships verified |
| Owner chat membership | Initiative-Bridge | Membership in all three channels | Channels created; Owner identity resolved | Owner appears in all three rosters | Complete; already joined by Owner, verified on review |
| Work / Task sidebar subsection | Owner creates; coordinators and scouts assign within their session scope | Swarm-Focused-Design (`sec-18d3da776be0a234`), containing participating workspaces | Viewer catalogue published; supported sidebar tools or documented fallback | Observed effective section matches; no pending assignment; applied receipts for new changes | Complete; both coordinators, P1–P8, and Fable-Scout verified |

## Documents and shared artifacts

The CM coordinator branch is the canonical hub. Prepare and commit the following
initiative bootstrap files there, including this plan and the review record:

- `CHARTER.md`: approved goal and boundaries.
- `PLAN.md`: setup status, later approved work, and decision log.
- `PROTOCOL.md`: onboarding order, communication, file ownership, verification,
  integration, and Owner checkpoints.
- `PROJECTS.md`: native initiative ID and project/task/worktree/channel mapping.
- `shared/README.md`: index linking shared results and documents to their owners
  and source versions. Add artifact directories as work needs them.

The protocol will direct participants to read the charter, current plan, project
mapping, and current message-board norms before working. Each future approved
task keeps `NOTES.md` in its owned area and reports completion with `report_done`,
including the outcome, evidence, and unresolved work. Coordinators keep status
and material decisions current at handoffs and reviews.

Onboarding and kickoff references identify the canonical repository, branch,
path, and reviewed commit as well as convenient absolute worktree paths.
Shared artifacts live in the hub when consumed across projects. Repository-local
evidence may stay with its code and be indexed by committed reference. The
predictionTrading onboarding doc links to the canonical charter rather than
maintaining a second copy. Each coordinator edits their own repository; proposed
hub changes come through the CM coordinator.

Independent implementation tasks use separate worktrees. Workers ask the
coordinator before touching another task's paths or driving another session;
coordination does not grant permissions the session lacks. Normal repository
review, merge, and verification rules apply. Commit only setup files, preserving
unrelated working-tree changes.

## Owner checkpoints and future proposals

Approval of this plan covers setting up the named initiative, its two project
memberships, linking the existing tasks, the onboarding documents, and the three
channels. Apply the native membership and activation approvals through the
supported Owner flow and record the resulting state.

New swarms, initiatives, project sides, groups, or expanded scope still require
Owner approval. Future proposals will state the intended goal, why the task is
suited to swarms, project and owned paths, task breakdown and dependencies,
verification, resources and risks, reporting, and requested Owner involvement.
Before any approved launch, the brief identifies the actual task, worktree,
current document versions, channel, and limits on further delegation.

Keep the charter's deployment rule in the protocol: Owner approval is required,
with the existing critical-bug-fix exception and normal repository checks. Where
the exception's applicability or authority is unclear, bring the concrete case
to Owner; do not infer a wider exception. This setup requires no deployment.

After setup, discuss the candidate directions already recorded in the charter
and predictionTrading contribution. SEJD remains an independently coordinated,
deliberately hands-on case study. Later proposals should distinguish lessons
that transfer to little-supervision work from those requiring close Owner input.

## Milestones and completion checks

1. **Setup approved:** Owner approves this plan and the named memberships and
   activation; record the approval before provisioning.
2. **Native grouping established:** the initiative is active, both memberships
   are approved, and both existing tasks point to it with the CM task as coordinator.
3. **Onboarding usable:** commit and cross-check the shared documents and local
   onboarding link; both coordinators know their responsibilities and checkpoints.
4. **Shared conversations ready:** create channels, post and pin a kickoff with
   doc links and approved setup scope, and verify shared access and native links.
5. **Setup complete:** report the initiative ID, channels, document references,
   and any remaining issue. Move to discussion of later work with Owner.

Before provisioning, check for existing records or channels with the same slug
to avoid duplicates. If a task already belongs to another initiative, resolve
the conflict with Owner before changing that association. If setup partially
succeeds, record completed IDs and resume from them. Check that no unrelated
tasks or SEJD records changed.

## Remaining setup work

None. Owner membership and all requested sidebar grouping are verified.
Selecting candidate implementations or new swarms remains an interactive Owner
decision in [PLAN.md](PLAN.md).

## Initial completion record

- Initiative `d381971d-0668-4ac0-8483-bf4111f3ddc2` is active with both approved
  memberships and exactly the two existing coordinator tasks linked. Task
  identity, status, worktree branches, and SEJD ownership remain intact.
- Shared docs were reviewed and pushed at CM bootstrap commit
  `867d8ee5ae3413c654e0c06483e376763f3c908e`. Follow the hub branch for this
  completion record and later decisions.
- predictionTrading onboarding was reviewed and pushed at
  [`ae3724a69`](https://github.com/Bigbadboybob/predictionTrading/blob/ae3724a69/agent_docs/swarm-focused-design-onboarding.md).
- All three channels are created, linked from native records, and have pinned
  kickoffs. Both coordinators' required memberships were read back; shared
  messages and pins were confirmed synchronized with the message-board hub.
- Local document links and whitespace checks passed. Swarm-Gardener independently
  verified the six shared documents, published bootstrap reference, local
  instruction links, and shared/predictionTrading channel access and pins.

The initiative and its coordinator tasks remain ongoing. Completing this setup
does not launch later candidate swarms or authorize deployments.

## Decision log

- **2026-09-10 — Owner:** approved the combined charter: "Okay is the charter
  complete. This looks solid to me".
- **2026-09-10 — Owner, earlier direction:** establish this coordination
  initiative using the two existing tasks before choosing later swarms.
- **2026-09-10 — Owner:** focus this initiative on swarm-favorable work with
  little supervision; preserve desired deep participation in SEJD and learn
  from its failure modes and Goodharting.
- **2026-09-10 — Coordinator proposal:** the setup responsibilities, document
  layout, and provisioning sequence above are ready for review. No new worker
  launch is part of this setup plan.
- **2026-09-10 — predictionTrading review:** Swarm-Gardener found no blockers
  with the proposed responsibilities; added canonical repository/branch/path
  and reviewed commit to onboarding references.
- **2026-09-10 — Owner:** responded to the setup-plan checkpoint, "Oh I see,
  protocol and projects. Sure sure, makes sense". Recorded as approval to set up
  the named initiative, both memberships, existing task links, documents, and
  channels. This authorizes the setup above; later swarm launches remain gated.
- **2026-09-10 — Provisioning:** created initiative
  `d381971d-0668-4ac0-8483-bf4111f3ddc2`, approved both memberships through the
  supported API, activated it at `2026-09-10T03:32:34.797365Z`, and linked the
  two existing tasks. All three channel paths are now stored on native records.
- **2026-09-10 — Setup complete:** both coordinators verified the shared docs and
  conversations; predictionTrading onboarding is committed at `ae3724a69`.
  The initiative is ready for discussion of later work with Owner.
- **2026-09-10 — Owner correction:** setup omitted Owner membership checks and
  the Work / Task sidebar subsection, and this setup record was not the work
  plan. Owner is verified in all three chats; subsection creation remains a
  laptop UI action. The interactive work plan now lives in `PLAN.md`. Full
  setup completion remains pending until the subsection is verified.
- **2026-09-10 — Sidebar grouping verified:** after deployment of the remote
  assignment tools, Swarm-Gardener verified its coordinator and all eight
  proposal scouts in `sec-18d3da776be0a234`. Initiative-Bridge directly verified
  its CM coordinator workspace in the same subsection. The observed assignments
  above supersede the earlier manual-only status. Swarm-Gardener then reported
  the separate Fable-Scout workspace still needs assignment and requested its
  self-assignment; full setup completion awaits that remaining verification.
- **2026-09-10 — Final sidebar verification:** Swarm-Gardener relayed Fable's
  successful self-assignment and observed applied receipt, resolving the last
  pending workspace. Both coordinators and all nine research scouts are
  grouped; setup is complete. This does not select any implementation or swarm.
