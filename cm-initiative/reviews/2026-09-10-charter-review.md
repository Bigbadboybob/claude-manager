# Charter review — 2026-09-10

Owner left three inline comments in `CHARTER.md`. The comments are preserved
below; their substance is incorporated into the revised charter. Owner approved
the combined charter on 2026-09-10 after the further reviews recorded below.
Earlier references to pending approval describe the state at that review stage.
The initial setup plan was subsequently approved; its decision log records setup.

## Owner comments and changes

> Not just ideas but yes onboarding/launching swarms and working with swarms to
> facilitate and improve capacity to reach user intended goals.

The outcome and predictionTrading role now include launching and onboarding
approved swarms, working with them during execution, and helping them reach the
user's intended goals. Proposed examples include objective refinement, task
breakdown, blocker resolution, and result integration. Approval before new
swarm launches remains in place.

> Let's add some concrete ones on how effective we are at reaching objectives.
> I guess this would mostly be based on my subjective feedback. Not sure how to
> operationalize. Maybe we don't have to. This is also why I think prediction
> trading side should add to the charter. Feel like this charter is a bit focused
> on the cm side.

Success criteria now lead with reaching intended goals, useful results, active
help during execution, and improvement informed by Owner's qualitative feedback.
Optional milestone discussion prompts make that feedback concrete without
requiring a score, cadence, or dashboard. Setup infrastructure follows those
outcome criteria. These formulations remain proposals for Owner review.

The existing predictionTrading `swarm-focused-design` agent was asked by DM to
contribute outcome, role, effectiveness, and SEJD case-study wording. Its task is
`f279464b-59da-4ba2-bce6-7bdb0a450847`. The request was confirmed in shared message
history (request ID `sfd-charter-pt-review-20260910-r1`). Swarm-Gardener supplied
the contribution in commits `e5918f340` and `3dcf661ce` on
`cm/swarm-focused-design`. It replied rather than editing the shared charter
concurrently. This is a review within the existing tasks, not a new worker/group
launch.

> Wait to be clear, SEJD is now an intiative. You already migrated it. It's a
> good case study.

The charter now explicitly records SEJD as an already active first-class
initiative, with its completed migration, retained coordinator, and `#sejd`
channel. A dedicated case-study section makes learning from SEJD part of the
scope while preserving its ownership and approval boundaries. Questions about
its effectiveness are identified as questions, not findings.

## predictionTrading contribution integrated

Source: [committed contribution](https://github.com/Bigbadboybob/predictionTrading/blob/b0c2038e7/agent_docs/swarm-focused-design-charter-review-2026-09-10.md).
Local source: `/home/lucas/.cm/worktrees/predictionTrading-swarm-focused-design/agent_docs/swarm-focused-design-charter-review-2026-09-10.md`.
Final contribution confirmed by Swarm-Gardener in message
`37db72a8-da6c-444c-b0d0-64daa6dc6fb3:e1f4b0fa-38cb-4fc1-9c16-13ea06f25bd8`.

### Additional Owner context relayed by that side

The following was reported as Owner's direct instructions on the
predictionTrading task. It is incorporated with that provenance for the combined
Owner review; it is not represented as approval of this charter or a launch plan.

- Approved swarms should make sustained useful progress with very little
  ongoing Owner supervision, protecting attention for SEJD, the execution
  pipeline, and structured-latent-space design decisions.
- Set up this coordination initiative before choosing the later ingest/P&L
  swarms. The initial setup plan can use the two existing coordinators and cover
  documents, channels, native memberships, and review of future proposals.
- Deployments require Owner approval except critical bug fixes; ordinary
  engineering checks still apply. The critical-fix exception needs concrete
  handling in the protocol without adding a wider exception.
- Future P&L optimization requires trustworthy evaluation, bounded modification
  permissions, and alignment with Owner's longer-term agenda.
- New platform expansion is on hold. Candidate swarm descriptions are for later
  discussion, not selected launches.

### Contributions and proposals from the reviewer

- Active predictionTrading support spans goal and responsibility clarification,
  onboarding, current context, blocker/goal-drift handling, integration,
  evaluation, and Owner feedback.
- Success should show useful, verified outcomes and shared understanding that
  survives changes in decisions or evidence. Qualitative feedback should use
  what Owner already provides and should itself require little attention.
- An enduring improvement objective can use recurring triggers and bounded
  rounds. Scheduler details and any continuous-task migration are later design
  choices, not commitments made by this charter.
- The candidate register keeps P&L improvement, ingest/momentum, backtest
  infrastructure, and continuous-task organization available for later
  discussion with their unresolved evaluation and permission choices.

### Evidence reviewed

The CM coordinator also inspected the cited 2026-09-01 R-band plan and
2026-08-25 SEJD config-parity audit. They support historical examples of stale
shared context, changes that invalidate dependent evidence, and results that
do not verify their intended integrated behavior. They do not establish those
issues as current defects, measure Owner's satisfaction, or demonstrate a causal
benefit from CM. The charter preserves those limits.

Both coordinator sides reported that no combined charter or initial-plan
approval had been received and that no native initiative, memberships, channels,
or workers had been provisioned for this setup.

## Further Owner review: active Goodharting monitoring

Swarm-Gardener relayed Owner's additional request to monitor swarms for
Goodharting and explicitly hunt for it, including where goals are qualitative.
The contribution is committed at `cd52d0a6e` on `cm/swarm-focused-design`;
source: [Goodharting addition](https://github.com/Bigbadboybob/predictionTrading/blob/cd52d0a6e/agent_docs/swarm-focused-design-charter-review-2026-09-10.md#proposed-success-criteria).

Owner subsequently clarified directly in the CM task that the success criterion
is **low levels of Goodharting**, while hunting for it is an activity. The charter
now has a short outcome criterion and puts active investigation in the
predictionTrading activity description. Both project roles have broader activity
descriptions under Participating projects; the charter does not prescribe a
detailed investigation checklist.

## Further Owner review: supervision modes and SEJD

Owner clarified that SEJD is currently their main focus and that deep involvement
there is intentional. Typically one or two initiatives receive most of Owner's
attention while others operate with little supervision. In main-line work, Owner
has less choice of task and uses swarming where it helps; this initiative focuses
on finding swarm-favorable tasks suited to little supervision.

The outcome, predictionTrading activities, and SEJD case-study section now reflect
that distinction. SEJD remains useful for investigating failure modes and hunting
Goodharting, with lessons considered for transfer to other supervision modes.
Owner's desired close involvement is not a failure against the low-supervision
objective. SEJD retains its existing coordination and approval boundaries. This
clarification does not constitute final charter or launch-plan approval.

## Charter approval

Swarm-Gardener confirmed that the predictionTrading contribution was aligned
with the supervision-mode clarification at commit `acfd6f8ce`, in message
`37db72a8-da6c-444c-b0d0-64daa6dc6fb3:014431c6-a0d4-4c63-a94f-6842ad00489d`.
That contribution was read before recording approval.

Owner subsequently said in this CM task:

> Okay is the charter complete. This looks solid to me

Recorded as approval of the combined charter on 2026-09-10. The next step is
review of `../PLAN.md` for setup using the two existing coordinator tasks.
Native provisioning and new swarm launches have not been approved by this
charter acceptance alone.
