# Swarm Focused Design — Work plan

Status: **Initial exploration authorized; planning remains interactive**,
2026-09-10. Owner supplied the initial directions below and then briefed
Swarm-Gardener directly with the research sequence recorded here. The
[charter](CHARTER.md) remains the agreed goal. Infrastructure history is in
[SETUP.md](SETUP.md); it is not the substantive work plan.

## How we plan together

Owner text/voice-dumps ideas, both coordinators explore and discuss them, and we
refine candidate plans together. Earlier dumps are useful starting material.
We keep open choices visible and update this plan as Owner steers the work;
drafting a document does not settle those choices.

The current authorization is to get ideas flowing through bounded research and
brainstorming within this existing initiative. Research tasks and the expressly
requested research subagents may support that exploration. Candidate documents
are proposals: no new initiative or production swarm is being created or
launched, and no candidate implementation or deployment is selected.

## Initial work

| Effort | Responsible side | Work and output | Review / dependencies | Current status |
|---|---|---|---|---|
| Find work suited to swarms | predictionTrading; Swarm-Gardener coordinates the Fable brainstorm, reviews, and collaborative proposal tasks | Preserve Owner ideas separately; investigate, synthesize AI ideas, review/refine, then write one document per refined idea | Direct Owner brief received; sequence below pre-authorized for research/proposals; Initiative-Bridge personally reviews candidate documents | Fable brainstorm and two independent list reviews complete; eight proposal-document tasks created, first group running |
| Brainstorm useful CM swarm features | Initiative-Bridge, assisted by a bounded research subagent | Inspect existing capabilities and identify useful coordination improvements, with evidence of the gap and concrete discussion options | Incorporate candidate plans and SEJD feedback as they arrive; Owner selects later feature implementation | [First discussion draft](shared/cm-feature-ideas.md) read and reviewed by Initiative-Bridge; no implementation selected |
| Learn from SEJD's coordinator | predictionTrading; Swarm-Gardener contacts SEJD-Coordinator | Ask what would help communication and swarming, capture concrete friction and useful feature/practice ideas in a feedback note | Preserve SEJD's current work and deliberate close Owner collaboration; bring transferable lessons to both sides | Feedback committed at predictionTrading `af7e3f732` and personally reviewed by Initiative-Bridge |

The predictionTrading side owns its research task, local briefs, codebase
investigation, candidate documents, and SEJD contact. Initiative-Bridge owns CM
feature exploration, cross-project review, and the shared plan/index. Existing
repository and session permissions still apply. Each research task gets bounded
owned paths and records its task ID, base revision, session configuration, and
result references when launched.

## Proposal documents in progress

The AI synthesis and two independent reviews retained the following eight
document topics. This is the authorized research division; it does not select
eight permanent swarms. Owner's O1–O7 list remains separate and preserved.
Initiative-Bridge read the synthesis, reviews, findings, and
[task roster](https://github.com/Bigbadboybob/predictionTrading/blob/a83f8ddc3/agent_docs/swarm-focused-design/LANES.md)
at predictionTrading `a83f8ddc3`. Status below is that committed snapshot,
reported at `2026-09-10T04:47:04Z`; the owning coordinator keeps the live roster.

| Topic | Intended proposal | Snapshot status |
|---|---|---|
| P1 | Momentum detection through explanatory ingest and useful source coverage | Draft `e426f6885` personally reviewed; [revisions requested](reviews/2026-09-10-p1-momentum-coverage-review.md) before Owner selection |
| P2 | Useful article follow-through, blocks, and appropriate non-trades | Running |
| P3 | Production-baseline P&L improvement with bounded rounds | Created; follows first group |
| P4 | Backtest fidelity, reliability, and useful completed comparisons | Created; follows first group |
| P5 | Backtest speed and resource efficiency preserving workload and outcomes | Created; follows first group |
| P6 | Useful timely delivery from existing sources | Draft `0cf7013da` personally reviewed; [revisions requested](reviews/2026-09-10-p6-source-reliability-review.md) before Owner selection |
| P7 | News-decision latency and cost while preserving useful outcomes | Created; uses earlier drafts |
| P8 | Recurring initiatives and continuous-task organization | Created; uses earlier drafts and CM input |

Each document must describe a plausible first improvement round and how it
would help Owner's goal, not stop at evidence storage or instrumentation.
Evaluation questions should improve decisions rather than make a new telemetry
system an automatic prerequisite. Authors compare overlapping scopes and share
findings and drafts: P1/P2, P3/P4/P5, P6 with P1, P7 with P2/P6, and P8 with P3
and Initiative-Bridge. Read the
[adopted list review](https://github.com/Bigbadboybob/predictionTrading/blob/a83f8ddc3/agent_docs/swarm-focused-design/LIST-REVIEW.md)
for the specific evaluation and collaboration refinements.

The configured worktree limit was reached after six isolated proposal trees.
Swarm-Gardener keeps P7/P8 authors read-only in its checkout and writes their
returned drafts itself; there are no shared worker writes, limit overrides, or
unrelated worktree cleanup. Launch groups limit simultaneous document workers.
This accommodation does not change the separate-worktree rule for independent
implementation. Each committed candidate handoff still receives Initiative-Bridge's
personal review before later Owner selection.

## predictionTrading candidate development

Keep a separate list of **all Owner ideas**, including earlier ideas. Synthesize
only the AI-generated ideas; do not merge away or silently replace Owner's list.
Owner's latest direct brainstorm on predictionTrading, relayed by Swarm-Gardener,
adds these directions:

- Momentum-event detection and coverage of explanations from existing articles;
  then explanations discovered on the web and routed toward scraper creation.
  Use the **existing RAG momentum pipeline**; do not have an agent manually
  search all recent articles.
- Follow articles through to trades and diagnose why each stage blocks progress;
  compare whether one swarm or separate swarms would fit this work.
- Improve P&L after production deployments using the **existing auto-backtest
  system** as the starting infrastructure.

The
[existing candidate register](https://github.com/Bigbadboybob/predictionTrading/blob/acfd6f8ce/agent_docs/swarm-focused-design-charter-review-2026-09-10.md#candidate-register-for-later-discussion)
also records production-baseline P&L improvement, backtest infrastructure, and
organization of continuous tasks. These are seeds for investigation, not a
selected implementation backlog. New platform expansion remains on hold.

The P&L proposal must also account for the existing
[execution improvement loop protocol](https://github.com/Bigbadboybob/predictionTrading/blob/cd7559d36/analysis/backtestIteration/PROTOCOL.md),
adopted with Owner on 2026-08-24. That record deliberately chose a
single-orchestrator loop rather than a continuous CM task and includes parity
and realized-primary evaluation rules. Check its current status, ownership,
and subsequent rulings before proposing a new arrangement. Compare compatible
evolution with any proposed revision; this brainstorm does not migrate that
loop, supersede its authority, or transfer SEJD/ES ownership.

Swarm-Gardener reports that Owner pre-authorized this sequence after the direct
brief is incorporated: preserve Owner's ideas; add the coordinator's ideas and
an independent Fable brainstorm; synthesize the AI ideas; obtain agent reviews
and refine that list; then launch one collaborative proposal-document effort
per refined idea. Agents should share findings, questions, and drafts with each
other. This authorization covers research and proposal development, including
those bounded proposal tasks, not the candidate implementations or production
swarms they describe.

Explore why the work is favorable to agents operating with little supervision:
what can proceed independently, how results fit together, what evidence can
establish useful progress, and which domain decisions still need Owner. Look
for clear metrics that track the desired result, with a credible baseline and
ways to check that apparent improvement is real. A metric to maximize is a
candidate evaluation tool; it does not override intended goals or justify
optimizing an unreliable proxy.

Write **one document per candidate plan**, in the predictionTrading task's
appropriate repository-local documentation area. Swarm-Gardener chooses the
exact directory under that repository's instructions and posts committed links
to the shared channel and artifact index. Each document should make the idea
reviewable without prematurely fixing implementation details:

- Intended outcome, why Owner might want it, and supporting codebase evidence.
- Why it suits swarming with little supervision; likely independent tasks and
  integration needs.
- Candidate metric/evaluation, baseline, uncertainty, and potential Goodharting.
- Proposed boundaries, dependencies, resources, risks, and Owner involvement.
- Alternatives and unresolved choices for the next interactive discussion.

Investigation may read code and existing evidence and produce documents. Any
proposed costly experiment, implementation, production change, or wider scope
must be brought back with its concrete requirements before proceeding.

## CM feature exploration and SEJD feedback

CM ideas should start from the existing product and an actual coordination need.
Distinguish capabilities already present, shortcomings in how agents use them,
and features that need building. For each promising idea, describe the user/agent
workflow, the observed or hypothesized failure it addresses, a bounded possible
change, and how we could tell whether it helped.

Swarm-Gardener should ask SEJD-Coordinator about communication friction, lost
context or decisions, handoffs, and other coordination improvements it would
find useful. Capture its examples and uncertainty in its own words; do not
interrupt or redirect SEJD work. SEJD is deliberately hands-on, so assess which
lessons transfer to the little-supervision work this initiative seeks.

Both sides may develop further ideas as evidence arrives. Owner chooses which
ones become implementation tasks or new swarms after discussion.

The initial CM discussion draft covers easier access to current initiative
context, outcome/evidence handoffs, finding results affected by changed inputs,
and linking concrete Owner decisions to plans. It separates current capabilities
from possible additions and leaves ranking open for the brainstorm.

## Review and next checkpoints

The [first coordinator review](reviews/2026-09-10-predictiontrading-research-review.md)
of predictionTrading's research plan, idea register, and SEJD feedback found no
blocker to this sequence. Evaluation comments are recorded for the later
candidate authors; the [shared index](shared/README.md) links the reviewed sources.

1. Swarm-Gardener incorporates the direct Owner brief, preserves the Owner idea
   list, and launches the independent Fable brainstorm using research subagents
   as authorized. Record actual task/session identities and source revisions.
2. Synthesize the AI ideas, obtain agent reviews, and refine them before creating
   the pre-authorized collaborative proposal-document tasks. Share findings and
   drafts across those tasks. Post candidate documents, CM ideas, and SEJD
   feedback as they become reviewable. There is no fixed candidate count, score,
   or deadline.
3. **Initiative-Bridge personally reviews each predictionTrading candidate
   document** for fit with Owner's intention, swarm suitability, evaluation and
   Goodharting risks, integration, and useful CM support. Return comments and
   questions to Swarm-Gardener, with review references in the shared index.
4. Discuss the options with Owner, revise the documents, and record the work
   Owner actually selects. Any new initiative, swarm, or implementation scope
   follows the charter's approval boundaries.

## Open choices for the brainstorm

- Owner's further corrections and preferred emphasis as ideas are reviewed.
- Which candidates have trustworthy evaluation and sufficiently independent
  work to merit a full swarm proposal.
- Which CM improvements would help those candidates or existing coordination
  enough to build first.

These are discussion topics, not a prerequisite questionnaire for Owner.

## Decision log

- **2026-09-10 — Owner:** clarified that planning should be an interactive
  text-dump and brainstorm with both sides; setup infrastructure alone is not
  the work plan. Infrastructure history moved to `SETUP.md`.
- **2026-09-10 — Owner:** requested predictionTrading codebase research and
  brainstorming through a Fable task/session using subagents, with one document
  per candidate plan. Owner will provide more ideas directly to Swarm-Gardener.
- **2026-09-10 — Owner:** requested CM feature brainstorming, allowed a research
  subagent, and asked the predictionTrading side to contact SEJD-Coordinator for
  communication and swarm-coordination ideas.
- **2026-09-10 — Owner:** asked Initiative-Bridge to review the predictionTrading
  documents personally, record these directions here, and mention that
  coordinator in the shared channel. New initiatives and production swarms
  remain unselected.
- **2026-09-10 — Coordinator:** read the initial CM feature draft and kept its
  four candidates as discussion options. Shared-channel message
  `37db72a8-da6c-444c-b0d0-64daa6dc6fb3:3d3e6d09-cd95-488d-b3d5-5110cc5287a7`
  mentions Swarm-Gardener with Owner's exploration directions, the direct
  brainstorm handoff, per-candidate documents, and the SEJD contact request.
- **2026-09-10 — Owner direction relayed by Swarm-Gardener:** message
  `37db72a8-da6c-444c-b0d0-64daa6dc6fb3:2b1cd16d-094a-4c5d-b06f-8db759d75588`
  records the further direct brainstorm and pre-authorized research/proposal
  sequence above. Existing RAG momentum and auto-backtest infrastructure are
  required starting points; Owner ideas remain separate from AI synthesis.
- **2026-09-10 — Launch configuration reported by Swarm-Gardener:** the local
  Claude settings select `claude-fable-5-1[1m]`, and its inspection found that
  `claude-code` launches have no daemon model override. That coordinator owns
  verifying the actual launched session; launch completion is not yet reported.
- **2026-09-10 — Research progress:** Swarm-Gardener supplied committed plan,
  idea register, SEJD feedback, and launch notes at `af7e3f732`. Fable research
  task `59a67007-7a71-412a-87d4-bf535df42893` is running in its isolated worktree,
  base `47076cd8e`, reported session `ts-18d3da59b187e3f9-1`. Initiative-Bridge
  personally reviewed those documents and the worker brief and checked the
  narrow RAG source claims. Runtime model confirmation is still pending; no
  candidate implementation or production swarm is selected.
- **2026-09-10 — Follow-up:** Swarm-Gardener confirmed Fable 5.1 in the child's
  transcript and channel report, recorded in predictionTrading `cd7559d36`.
  Initiative-Bridge read those notes and the existing execution-loop protocol;
  the P&L proposal must consider that precedent and its current authority.
- **2026-09-10 — Proposal preparation:** Swarm-Gardener integrated the Fable
  brainstorm, corrected unsupported source claims, synthesized AI ideas, and
  obtained two independent list reviews. P1–P8 are retained as document topics
  at `a83f8ddc3`; P1/P2/P6 are running. Initiative-Bridge read those records and
  indexed them; individual proposal reviews await committed handoffs.
- **2026-09-10 — P6 personal review:** Initiative-Bridge read the source-delivery
  proposal and notes at `0cf7013da`. Requested corrections to the recovery metric,
  scheduled-release matching, current authority, detector claims, and overlapping
  P1/P2/P6 populations. Classification alone must not count as restored useful
  supply. The candidate remains a proposal; no implementation round is selected.
