# Swarm Focused Design — Charter

Status: **Approved by Owner**, 2026-09-10: "Okay is the charter complete. This
looks solid to me". Incorporates Owner's inline comments, subsequent
clarifications, and the predictionTrading contribution, aligned through commit
`acfd6f8ce`. See [review notes](reviews/2026-09-10-charter-review.md) for provenance.
The charter and initial infrastructure setup were approved. Native initiative
`d381971d-0668-4ac0-8483-bf4111f3ddc2` is active with both memberships approved.
The [project map](PROJECTS.md) records native IDs and channels. [PLAN.md](PLAN.md)
captures Owner's initial exploration directions and continuing interactive
brainstorm; [SETUP.md](SETUP.md) records infrastructure and the pending sidebar
subsection.

Initiative slug: `swarm-focused-design`.

## Outcome

Enable Owner-approved groups of agents to make sustained, demonstrable progress
toward the user's intended goals with little ongoing supervision. Choose work
that is favorable to swarms and that agents can carry forward independently,
launch and onboard approved swarms, and work with them through execution to
produce useful, verified results. Claude Manager and predictionTrading
contribute complementary capabilities to that outcome.

Owner typically concentrates most of their attention on one or two initiatives
at a time, while others operate with little supervision. SEJD is currently
Owner's main focus, and deep involvement there is intentional. In main-line
work, the task is less freely chosen; Owner uses swarming where it helps with
the work that needs doing. This initiative focuses on selecting work suited to
the little-supervision mode, protecting Owner's attention for those priorities.

The **claude-manager side** develops the coordination capabilities supporting
this work. The **predictionTrading side** launches, onboards, and works with
approved swarms to help them reach the user's intended goals. Both sides improve
how the work is carried out and check whether their changes help it succeed in
use.

The two sides share a feedback loop: intended goal → approved approach and
swarm → supported execution → useful results and Owner feedback → improvements
to how the swarm works and to CM where needed. Improvements can come from
better task framing, onboarding, coordination, or tooling. Owner remains
involved at the checkpoints below, especially before a new initiative or swarm
starts.

This is an ongoing initiative. Its initial setup establishes coordination
between the two existing tasks. Choosing later swarms is a separate discussion
after setup; this document does not select a new swarm to launch or authorize an
implementation backlog.

## Why

Owner wants coordinated agents to become more capable of accomplishing useful
objectives, with results that match what the user actually wanted. That requires
ongoing help with execution as well as clear goals, onboarding, shared context,
and communication. Real work in predictionTrading supplies the setting for
learning what helps; CM supplies coordination capabilities that make those
practices easier to carry out and oversee.

## Participating projects

Projects are codebases. Initiatives group tasks and subtasks within one or more
projects; tasks may also stand outside an initiative. "Swarm" and "stream" are
working descriptions, not additional CM object types.

| Project | Role in the initiative | Coordinator task/worktree | Project channel |
|---|---|---|---|
| `claude-manager` | Initiative coordination; coordination feature proposals, implementation, and verification | `Swarm focused design CM` (`3b58ab69-3c89-4dee-bed4-c714858b0656`); `/home/lucas/.cm/worktrees/claude-manager-swarm-focused-design-cm` | `initiative/swarm-focused-design/claude-manager` |
| `predictionTrading` | Launch and onboard approved swarms; work with them to reach user-intended goals; develop suitable ideas, improve execution practices, and request CM capabilities | `Swarm focused design` (`f279464b-59da-4ba2-bce6-7bdb0a450847`); `/home/lucas/.cm/worktrees/predictionTrading-swarm-focused-design` | `initiative/swarm-focused-design/prediction-trading` |

**Claude Manager activities:** develop and evaluate features for coordinating
agents, informed by both practical requests and proactive exploration. Improve
how goals, shared context, tasks, results, and Owner checkpoints are organized
and made accessible. Maintain the initiative's shared coordination documents and
work with predictionTrading to assess whether CM changes help real work.

**predictionTrading activities:** find promising tasks suited to swarms working
with little ongoing Owner supervision, help frame goals and approaches, and
launch and onboard approved swarms. Support them through execution, blockers,
integration, and evaluation; improve working practices and bring Owner the
decisions needing their judgment. Investigate failure modes and actively hunt
for Goodharting by checking whether claimed progress and ways of evaluating
work are pulling swarms away from intended outcomes. Learn from both these
swarms and closely supervised work such as SEJD, and feed those lessons and
concrete tooling needs back into CM development.

The existing CM task is the initiative's entry point and native
coordinator task. Its hub branch is `cm/swarm-focused-design-cm`. The existing
predictionTrading task remains the entry point for that side, on branch
`cm/swarm-focused-design`. Both are existing top-level tasks; setup should retain
their identities and history.

Shared channel: `initiative/swarm-focused-design`. It carries
cross-project requirements, decisions, blockers, and approved plans. The two
project channels carry their respective implementation and onboarding details.
All three channels are provisioned and recorded on the native initiative and
project memberships.

## Success criteria

- **Reaching the intended goal:** at a meaningful milestone or completion,
  the swarm presents its result against the goal agreed with Owner. Owner can
  judge whether it achieved the intended outcome and is useful; any remaining
  gap is explicit. A technically completed task can still need work if it missed
  that intended outcome. Results carry the verification appropriate to the
  work, and reports distinguish achieved outcomes, supporting evidence, and
  unresolved or blocked work.
- **Low levels of Goodharting:** swarms rarely improve metrics or appear to
  complete work at the expense of Owner's actual goals.
- **Helping swarms deliver:** predictionTrading's contribution includes active
  help with onboarding, execution, blockers, and integration. Concrete examples
  should show how that help moved work toward the intended result, and where it
  failed to help.
- **Improving effectiveness over time:** use Owner's qualitative feedback to
  identify whether work is getting closer to the intended outcome and requiring
  less avoidable correction, repeated explanation, or rescue. Record specific
  examples and adjustments worth carrying into later work. This is a direction
  for improvement, not a claim that every task requires less Owner involvement.
- **Protecting Owner's attention:** for work selected for low-supervision
  delegation, Owner finds that the swarm keeps making useful progress with
  little ongoing direction and leaves attention available for priority work.
  Required reviews and domain decisions remain part of the agreed boundaries.
- **Maintaining shared understanding:** approved swarms can begin and continue
  work with the current goal, decisions, responsibilities, dependencies, and
  evidence available. When those change, affected participants can find the
  change and understand its consequences for their work.
- **Useful learning from actual work:** use SEJD and other explicitly selected
  cases to identify what helps swarms reach their goals. Try approved changes to
  coordination practices or CM features in real work and record whether Owner
  found the resulting work more effective.
- Owner can inspect current work, results, blockers, and upcoming approval
  points without reconstructing them from individual agent transcripts.

Owner's subjective assessment is a primary source of evidence here. Use feedback
already offered and keep collecting it lightweight. At an existing result review
or natural point of friction, show the intended outcome, result, and any relevant
coordination difficulty. A brief discussion can cover: "Did this achieve what you wanted?",
"What was useful or missing?", and "Where did coordination need avoidable correction?" These are
optional prompts for useful feedback, not a required questionnaire. We do not
need a numeric score, fixed review cadence, or reporting dashboard to begin.
Record corrections in the existing decision record and apply them in the next
approved iteration. Quantitative measures can be proposed later when they help
judge a specific objective.

The setup also succeeds when the coordination infrastructure supports that work:

- The two existing tasks are linked to one first-class initiative with both
  project memberships approved and discoverable.
- Both sides can find the same charter, current plan, work protocol, shared
  artifact index, and shared conversation, plus their own project conversation.
- A proposed swarm can be presented to Owner with a concrete goal, bounded
  scope, initial task breakdown, onboarding, verification, and reporting plan
  before agents are launched.
- predictionTrading can turn an observed coordination problem into a CM feature
  request with enough context to assess it; CM can propose improvements without
  waiting for a request. Owner-selected work receives concrete acceptance checks.

## Existing case study: SEJD

SEJD is already an active first-class initiative in predictionTrading
(`22da0567-5f23-41a8-b529-2171f258a383`), with its existing coordinator and `#sejd`
channel. Its migration has already been completed.

SEJD involves swarming with Owner deeply involved, as Owner wants. Its usefulness
as a case study does not depend on becoming a low-supervision initiative, and
Owner's chosen close involvement is not a coordination failure or a shortfall
against this initiative's goal.

Use SEJD's experience to examine whether swarms reached the intended objectives,
how onboarding and ongoing coordination helped or hindered them, what failure
modes or Goodharting occurred, and how individual results became a useful whole.
Distinguish avoidable correction or coordination overhead from the domain work
and close collaboration Owner wants to do. Consider which lessons transfer to
work with little supervision and which depend on Owner's close involvement.
These are questions to investigate, not conclusions about SEJD's effectiveness.

Case-study learning belongs in this initiative. SEJD keeps its own goal,
coordinator, tasks, channels, and approval boundaries; any proposed change to its
live work goes through that existing coordination and the relevant Owner gate.

Historical evidence identified by the predictionTrading review provides concrete
starting points:

- The 2026-09-01 R-band build plan documents stale specification copies,
  reconciliation of canonical sources, explicit branch bases, and evaluations
  at stage boundaries. It also documents a preprocessing correction that
  invalidated earlier baselines. These support examining how swarms keep shared
  context current and propagate changes to evidence across dependent tasks.
- The 2026-08-25 config-parity audit records results that did not establish the
  deployed-model behavior their intended use required. It supports checking
  integrated behavior and recording what actually ran when judging completion.

The [predictionTrading contribution](https://github.com/Bigbadboybob/predictionTrading/blob/b0c2038e7/agent_docs/swarm-focused-design-charter-review-2026-09-10.md#sejd-evidence-and-lessons)
links those sources. They describe historical situations, not a current defect
list or proof of Owner satisfaction, comparative swarm effectiveness, or a
benefit caused by CM.

## Later directions and current priorities

Owner's additional context on the predictionTrading side names production-
baseline P&L improvement, ingest/momentum discovery, backtest infrastructure,
and the organization of existing continuous tasks as directions for later
discussion. Their details and unresolved choices are recorded in that side's
[candidate register](https://github.com/Bigbadboybob/predictionTrading/blob/b0c2038e7/agent_docs/swarm-focused-design-charter-review-2026-09-10.md#candidate-register-for-later-discussion).
They are not selected tasks or prerequisites for establishing this initiative.
New platform support is on hold.

A future P&L improvement swarm needs trustworthy evaluation, explicit limits on
what it may modify, and a shared direction consistent with durable code and
Owner's longer-term agenda. Its evaluation rules, writable scope, and promotion
criteria must be designed before launch. An enduring improvement objective can
have recurring triggers and bounded rounds that finish, are rejected, or yield
a deployment proposal. This is a proposed organizing model, not authorization
to configure a scheduler or migrate existing continuous tasks.

## Constraints and checkpoints

- **Charter:** the predictionTrading side contributes its perspective on
  outcomes, ongoing swarm support, and effectiveness. Owner reviews the combined
  outcome, participating projects, success criteria, constraints, and exclusions
  before the initial plan is finalized.
- **Initial setup plan:** after charter approval, agree the roles of the two
  existing coordinators, onboarding and shared documents, native initiative and
  memberships, channels, and the process for reviewing later work. Setup can
  complete before selecting a production-improvement swarm. Any new task/group
  launch requires review of its purpose, project, scope, verification,
  dependencies, and risks.
- **New swarms and initiatives:** agents may develop ideas and proposals, but
  starting a new swarm, initiative, project side, or agent group requires explicit
  Owner approval. Approval of this charter is not approval of unspecified future
  launches. Existing approved work may iterate within its approved scope.
- **Scope and permissions:** adding a codebase or changing the permission model
  returns to Owner. Initiative membership does not grant global session control
  or permission to drive another task's sessions.
- **Coordination:** the coordinator maintains the shared documents and decisions;
  each side owns its local work and reports results and blockers through the
  agreed channels. Independent tasks use separate worktrees unless Owner
  explicitly chooses shared ownership.
- **Native state:** activate the initiative only after the charter, initial plan,
  and both memberships are approved. Store channel paths after provisioning
  succeeds, and preserve the existing tasks' history when linking them.
- **Verification and rollout:** deployments require Owner approval, with the
  critical-bug-fix exception relayed from Owner by the predictionTrading side.
  Normal repository verification and rollout procedures still apply. The
  protocol must make the exception's handling concrete without broadening it;
  an optimization objective or favorable backtest is not deployment approval.
  This charter does not authorize a blanket rollout or automatic agent restarts.
- No deadline, spending limit, or target swarm count has been set. Concrete
  resource needs and any additional checkpoints should be stated when proposing
  the initial work.

## Out of scope

- Autonomous creation or launch of new swarms or initiatives.
- A separate first-class "workstream" or "swarm" object in CM.
- Taking over, reparenting, or repeating the completed migration of SEJD. Its
  use as a case study is in scope; changing its live work requires the existing
  approvals. Other initiatives retain their current ownership as well.
- Automatically enrolling unrelated tasks or adding projects beyond the two
  named above.
- Treating a coordination experiment as authorization for live trading changes,
  unrelated feature development, or expanded session permissions.
- New platform expansion, which Owner has put on hold.

## Review and next step

Owner approved the combined charter and initial setup on 2026-09-10. The
initiative is active, both coordinator tasks are linked, onboarding docs are
committed, and channels have pinned kickoffs and Owner membership. The laptop
sidebar subsection remains pending in [SETUP.md](SETUP.md). Owner has now
authorized the initial research and brainstorming in [PLAN.md](PLAN.md); develop
and review candidate plans interactively before selecting new swarms.
