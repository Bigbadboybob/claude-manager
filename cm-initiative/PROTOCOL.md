# Swarm Focused Design — Work protocol

Initiative: `d381971d-0668-4ac0-8483-bf4111f3ddc2` (`swarm-focused-design`).
Established under the Owner-approved [charter](CHARTER.md) and [setup plan](PLAN.md),
2026-09-10. The [project map](PROJECTS.md) identifies the current coordinators,
repositories, branches, worktrees, and channels.

## Start here

Read the charter for the intended outcome and boundaries, then the current plan,
this protocol, and the project map. Read your repository's applicable instructions
and the current message-board norms through `chat_open` and `chat_norms`; use
available channel-specific norms as well. Shared norms are maintained on the
board, not copied into this file.

This initiative seeks work suited to swarms making useful progress with little
ongoing Owner supervision. Low levels of Goodharting is an outcome; investigating
failure modes and hunting Goodharting is part of predictionTrading's work. SEJD
remains a separately coordinated case study with deliberate close Owner input.

Setup uses Initiative-Bridge and Swarm-Gardener in their existing tasks. No
additional worker or production swarm has been selected. Future workers begin
only with an approved task brief covering the goal, project, worktree, owned
paths, dependencies, verification, reporting channel, and limits on delegation.

## Coordination and decisions

Initiative-Bridge is the initiative entry point, maintains shared docs, and
brings cross-project decisions to Owner. Swarm-Gardener owns the predictionTrading
side and its local onboarding. Each coordinator handles routine work within
their approved scope and reports what the other side needs to know.

Use the shared channel for cross-project proposals, decisions, useful results,
dependencies, and blockers. Use each project channel for local implementation
and evaluation details. Keep messages concise; link longer reasoning and evidence.
Use an existing thread when following up on the same decision. Owner reviews
and questions go through the normal coordinator session, with material decisions
recorded in the plan so both sides can find them.

At a handoff, milestone, or material change, state the intended outcome, current
result, evidence, unresolved work, and next action or decision. Do not turn this
into a fixed reporting cadence or scorecard. Use Owner's existing qualitative
feedback and distinguish desired domain collaboration from avoidable correction.

## Canonical documents and artifacts

The CM coordinator branch is the hub. The charter, plan, protocol, project map,
and [shared artifact index](shared/README.md) live in its `cm-initiative/` directory.
Local onboarding links here instead of maintaining another charter. Share the
repository, branch, relative path, and reviewed commit with every substantial
handoff; absolute worktree paths are conveniences for the current host.

Put artifacts consumed across projects in `shared/`, or index a committed source
in its owning repository when it should stay with the code. Record the artifact's
owner, purpose, source revision, and status in the index. Label proposals,
historical evidence, and verified results accurately. If a baseline or decision
changes, identify affected artifacts and dependent tasks before reusing results.

Each future approved task keeps `NOTES.md` in its owned area. Capture meaningful
decisions, result/evidence references, unresolved questions, and handoff context.
The coordinator updates the plan and artifact index when work is integrated.

## Ownership, integration, and completion

Each coordinator writes in their own existing worktree. Independent implementation
tasks use separate worktrees and an explicit base revision. Workers ask the
coordinator before touching another task's paths, driving another session, or
changing scope. Coordinator agreement does not grant session-control permissions
the caller lacks. Starting another swarm or group still needs Owner approval.

Follow the owning repository's instructions for code review, tests, merges, and
releases. Verification must establish the intended result in the relevant system,
including integration when individual task results depend on one another. State
what actually ran and what remains unverified. Commit only owned changes and
preserve unrelated work. The owning coordinator integrates the result and reruns
the checks warranted by integration.

Workers report completion with `report_done` and a channel handoff linking the
result, commit, verification, and unresolved work. The coordinator checks the
result against the approved goal before marking work complete. Completing setup
does not complete this ongoing initiative or its coordinator tasks.

## Owner checkpoints

The charter and setup plan approve the two existing project sides, their
coordination documents, native grouping, and three channels. Later launches and
changes use the boundaries below:

- Bring each proposed new swarm, initiative, project side, or agent group to
  Owner with a concrete goal, scope, task breakdown, resources, dependencies,
  verification, risks, and requested Owner involvement before creating or
  launching its tasks. Workers may develop proposals but may not spawn a new
  swarm themselves.
- Return to Owner for scope or permission changes, including another codebase
  or global session control. Existing approved work may iterate within scope.
- Keep SEJD's goals, ownership, live work, and approvals with its existing
  coordinator. Learn from its evidence; proposed interventions use that
  initiative's existing gates.
- Deployments require Owner approval, with the existing critical-bug-fix
  exception and normal repository checks. Record the defect, evidence of
  criticality, bounded repair, verification, and rollout/rollback handling when
  relying on that exception. If criticality or authority is unclear, bring the
  concrete case to Owner. Optimization or a favorable backtest does not itself
  authorize deployment. This setup includes no deployment.

Record approvals and material corrections in the plan's decision log, including
their source and scope. Durable goal changes return to Owner; routine status and
artifact updates remain the coordinators' responsibility.
