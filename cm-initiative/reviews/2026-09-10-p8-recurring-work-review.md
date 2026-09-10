# P8 — Personal review of recurring-work organization

Reviewer: Initiative-Bridge, 2026-09-10. Personally read the complete proposal
and NOTES at predictionTrading `1bf555817`, authored by Recurrence-Scout and
written by Swarm-Gardener in its owned checkout. Proposal:
[PROPOSAL.md](https://github.com/Bigbadboybob/predictionTrading/blob/1bf555817/agent_docs/swarm-focused-design/recurring-work/PROPOSAL.md).

**Disposition: consolidate corrections before Owner discussion.** A bounded
reconciliation of one existing effort is a useful option. Preserve ordinary
tasks/subtasks and existing documents as sufficient building blocks, the
planning-versus-daemon distinction, unknown outcomes, and a valid no-change
result. These are corrections to this proposal, not a request for more research,
a new fleet census, or any change to existing orchestrators.

## 1. Align supervision and decisions with this initiative's charter

Section 1's “anything here that reduces Owner contact is a bug” contradicts the
approved little-supervision focus. SEJD deliberately retains deep Owner input;
other suitable work should reduce avoidable correction and supervision while
preserving the checkpoints Owner wants. Do not apply SEJD's particular rulings
to every effort. Likewise, an effort can need no Owner decision, or more than
one; “the one decision it needs” is not a universal lifecycle property.

The claimed Owner-decision throughput bottleneck in sections 2/11 is unmeasured.
P3's final proposal withdrew that historical timing claim. Keep it a hypothesis
to assess where relevant. M0 qualitative feedback is useful input, not a new
“gates everything” requirement, a compulsory questionnaire, or a reason all
authorized investigation must stop. Preserve existing work authorization and
Owner's new-swarm checkpoint even if the actual active fleet is smaller.

Notification delivery remains outside this initiative. Decision-record
organization may be discussed, but sections 10/13 and NOTES must not expand the
proposal into reconciling notification/push policies.

## 2. Separate finite outcomes from scheduling choices

Section 7.2's first-fit rule is incorrect: a finite outcome and a scheduled or
event trigger are independent dimensions. A finite effort can use a Consumer
or periodic task, and repeated checks need not require a standing swarm.
Check existing ownership and the intended outcome first, then arrival/trigger
needs and lifecycle. `window_secs` is a batching trigger, not a maximum item
wait under inflight work, pauses, failures, or capacity limits.

Section 5.4's document does not establish no CM task/session/schedule anywhere,
idempotent ticks, or the superiority of manual execution. Its use of
“continuous” need not contradict the unrelated backtest loop's deliberate
single-orchestrator choice. Keep the documented workflow and unknown current
operation; do not import that other loop's ruling as a universal rule.

## 3. Preserve the meanings of pause, completion, and successful no-change work

Sections 5.2/7.4/8 propose `blocked` for every deliberate pause. Under the cited
convention it means operator action is needed. A deliberate pause with no
pending Owner action need not be blocked; describe it through an appropriate
existing note/state surface and leave daemon scheduling with its owner.

Sections 3.1/3.2/15 overstate “recurrence solved,” run mechanisms as solid, and
run completion as merely “the agent stopped.” The documented mechanisms exist;
`report_done` is an explicit completion declaration, and historical closure
failures show that neither declaration nor process state independently proves
the intended outcome. No current reliability measurement was made.

G6 must not require a changed disposition to count as useful reconciliation.
Confirming an intended healthy or paused state, or ruling out an unnecessary
intervention with evidence, can be valuable. Otherwise this proposal rewards
manufacturing changes. Saturation trending toward zero likewise calls for
checking search validity and intended progress, not automatic retirement of a
consumer or record convention; use P3's final qualification.

## 4. Bound the sampled evidence and historical claims

No `task_artifacts` rows in the newest 60 is not no outcome records. Empty
metadata is not proof that Behavior Triage, Code Bug-Hunt, or scraper-opt lack
in-repository results. Correct sections 3.6/7.1/15 and the NOTES findings table.
The L4 directory plus that unrelated sample does not establish the absence of
any L4 task/artifact, nor that every current mechanism existed at L4's date.
State the concrete representation available now, the documents actually found,
and which associations were not established. Do not label absence of a new
pointer convention an established adoption failure.

`git diff A...B` reports merge-base-to-B changes, not the current A-to-B tree
delta. The 550 count describes commit ancestry, not whether those changes
arrived by another path. A merge from main into a branch does not establish
current freshness or unlanded content. Preserve the exact commands and refs;
withdraw NOTES' “directionally right” endorsement of the unverified session
memory. No per-item landing claim is justified by those two counts alone.

Reconcile or remove the SEJD total: 232 + 19 + 3 + 3 = 257, not 255. The
per-orchestrator table sum also differs from the direct 692 count; NOTES calls
it a nested-continuous edge case without establishing that explanation. Label
the discrepancy unresolved using the existing snapshot. Queue pending age
does not establish that no other item was claimed since that date. This host
is a cloud worktree; do not call it Owner's laptop.

## 5. Distinguish missing planning columns from all CM visibility

Null project/initiative fields on those fourteen planning rows describe that
snapshot. They do not prove that no project/initiative view can reach any
recurring work, particularly its children, nor that such associations cannot
be represented. Current `api/main.py` creation and update validation accepts
project/initiative associations without a blanket continuous-kind exclusion;
exact lifecycle and existing ownership still matter before any reassignment.

Similarly, an absent `metadata.continuous` mirror does not prove that the whole
TUI or daemon surface cannot show schedule/run/pause state. Distinguish the
queried planning API from existing `continuous.list`, broadcasts, and TUI
badges. Momentum Detective has a named planning row, so section 5.5's “no board
presence at all” contradicts its evidence; no descendants in a non-archived
snapshot is not no work. Historical runbook fleet counts also need not be
claims about today's fleet; retain only demonstrated current contradictions.

## 6. Keep record and evaluation choices proportionate

REQUIRED gates abandoning staged output are not automatically consistent with
“gates are reads.” Use P3's distinction: choose the gate policy for its claim,
retain failed evidence and every attempt, and keep interpretation separate from
publication. A preregistration or `kind:round` row is a proposed convention,
not a necessary condition for every valid finite task. Record shape alone does
not prevent Goodharting or establish independent evidence.

For section 10's CM questions: ordinary tasks plus documents already represent
a finite round without a trigger or new type. A general artifact reader may
improve discoverability if the existing authorized GET route is insufficient
for an actual workflow; that is a candidate usability improvement, not an
asserted missing store. Project/initiative associations are already schema/API
concepts; first identify the precise integration limitation, without changing
other efforts. These answers require no migration or implementation now.

Section 13's “no compute, no LLM spend” overstates the cost of agent reading and
analysis; describe it as bounded research effort with costs unmeasured. Keep
section 8's future label/artifact writes and remote reconciliation proposed;
the current author remains read-only and no sibling operation is selected.

## Follow-through

Fold this review together with Swarm-Gardener's existing review into one
revision, updating repeated claims and NOTES. Preserve useful options and
uncertainty without adding new required studies or expanding the current scope.
Initiative-Bridge will personally read the consolidated changes.

## Full consolidated personal read at `380cf96f6`

Personally read the complete revised PROPOSAL and NOTES. The substantive six
groups are addressed. The draft now proposes one bounded reconciliation with
optional pointers, treats finite work and triggers independently, distinguishes
planning rows from daemon/UI state, and preserves successful no-change work.
Historical absence and landing claims are appropriately narrowed.

Requested four final root consistency edits: P3/P4 are proposal contributors,
not owners of evaluation/reuse policy; section 5.2's prohibition on `blocked`
applies when no operator action is pending, matching section 8; notification
delivery is outside this initiative while decision-record organization remains
legitimate scope, with no convergence selected; and ordinary-task representation
was already confirmed by this review, not a pending gate. No new research or
author cycle is requested. Final acceptance awaits the root correction diff.

Read-only CM follow-through confirms HOWTO_CONTINUOUS_TASKS.md line 67 still
says the API does not persist kind, while current api/main.py passes kind to
db.add_task. This is a source/runbook inconsistency; no deployed API was queried
and no documentation repair or task mutation was performed. The proposed
metadata concurrency concern remains unverified and is not a runtime finding.

## Final disposition at root `080256a08`

**Ready for Owner discussion; no implementation selected.** Personally read all
proposal/NOTES changes from the full consolidated `380cf96f6` through final
root `080256a08`. P3/P4 contributions remain subject to existing owners;
`blocked` requires actual operator action; decision-record organization remains
in scope while notification delivery is excluded; and ordinary-task
representation is settled. The final correction also distinguishes completion
declarations from evidence, limits association claims to inspected rows, and
keeps saturation diagnostic. Existing authorized reads acquire no invented
approval gate; actual access limitations still apply.

The candidate is one bounded reconciliation of an existing effort with optional
pointers and a valid no-change outcome. It authorizes no migration, schedule
change, new swarm, or action on a sibling session. The perf-loop documentation
item is resolved in the same root revision; CM's kind-persistence runbook
observation remains a source/documentation finding, not a live defect verdict.
