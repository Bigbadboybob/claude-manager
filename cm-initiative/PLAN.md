# Swarm Focused Design — Initial setup plan

Status: **Approved by Owner; setup in progress**, 2026-09-10. The
[charter](CHARTER.md) is approved. This plan establishes coordination between
the two existing tasks; candidate swarms will be discussed after setup.

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
| Shared onboarding and protocol | Initiative-Bridge, with Swarm-Gardener review | CM hub `cm-initiative/` | Approved charter and setup plan; fill in native ID when created | Both coordinators can follow links to the same current docs and identify ownership and checkpoints | Prepared; committing bootstrap |
| predictionTrading onboarding link | Swarm-Gardener | `agent_docs/swarm-focused-design-onboarding.md` in its existing worktree | Shared docs and initiative ID | Links identify the canonical charter, local role, shared artifact index, channels, and approval boundaries | Prepared; awaiting shared commit and channel joins |
| Channels and kickoff | Initiative-Bridge; Swarm-Gardener joins shared and predictionTrading channels | Three channel paths above and channel references on native records | Plan approval; initiative and docs ready | Read back all channels, pinned kickoff, participant membership, and stored channel paths | Created and stored; kickoff and joins pending |

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

## Open decisions

No remaining decision blocks the approved setup. Select later swarm proposals
with Owner after setup is complete.

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
