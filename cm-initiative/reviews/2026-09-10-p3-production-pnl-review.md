# P3 — Personal review of production-baseline P&L improvement

Reviewer: Initiative-Bridge, 2026-09-10. Personally read the full proposal and
notes at predictionTrading `e616b6dea`, authored by PnL-Scout, task
`a2c94716-03c5-4112-8140-d556f1d7ff6b`. Proposal:
[PROPOSAL.md](https://github.com/Bigbadboybob/predictionTrading/blob/e616b6dea/analysis/backtestIteration/docs/swarm-pnl/PROPOSAL.md).

**Disposition: revise before Owner discussion.** Preserve the broad Owner goal,
post-signal work as one first application, and bounded supplier to the existing
single orchestrator. The explicit unknown freeze/Q23 status, separation of
realized/MID/size-aware liquidation, optional instrumentation, independent
scrutiny, and a valid no-useful-change outcome are useful. No round is selected.

## Corrections requested

### 1. Retain the exact timing evidence and attempted populations

Sections 9.1 and 10 call L4's 64 minutes battery makespan. I read
`L4-ROUND1-2026-09-05.md` at the inspected `91365cc40`: it reports the last
artifact published 64 minutes after the first, not the duration from submission
or launch to all usable results. Queue, provisioning, download, publication,
and review/collection can lie outside individual replay wall. Keep battery
turnaround as the proposed outcome, but call this historical figure publication
span; it does not establish zero queue or full turnaround. The report's table
also has 3,860 s for trio-t2, so 3,811 s is not its largest listed cell wall.

Preserve the distinction between seven deployment-law cells actually run and
four obsolete parent configs that could not boot; they are different declared
populations. No denominator manipulation follows merely from replacing configs
with documented siblings. Swarm-Gardener already supplied the timing correction
in-channel; reconcile with revised P4/P5 rather than the initial summaries.

### 2. Narrow the reference-policy screen's causality claim

Section 3.7, W6, and NOTES call `signal_rule_replay` fully causal. I checked the
job at `91365cc40`: lines 85–90 use joined article confidence as a fallback and
`a0.added_at`. Restricting future book access does not constrain all inputs to
values available at the decision time. A generic 70/30 runner split also does
not establish held-out provenance for the persisted pinned-policy rows.
Adopt the input-provenance and mixed `no_trade` limits in P2 `a97faa1a6` without
promoting that policy to an ES evaluator. Keep optional hypothesis generation;
no execution or new screen is required for this revision.

### 3. Keep selection protection distinct from metric validity

The development/final split addresses adaptive selection. It does not fix
REALIZED's insensitivity to an unclosed loss: a strategy can exploit that
metric on fresh data too. Section 5.1's claim that the split is what limits
selection needs this qualification; retaining the bracket and scrutiny of
open exposure remains necessary on both evidence classes.

Section 7.2 also treats episodes materialized after a candidate is fixed as
unseen. Artifact creation time alone is insufficient if the underlying episode
or its outcomes informed development, selection, or evaluator tuning. Require
that evidence actually be unexposed to those decisions, with provenance; retain
unknown exposure where it cannot be established. Preregistering an old finding
later does not by itself supply independent confirmation.

### 4. Keep baseline and fidelity conclusions conditional

Rerunning a baseline does not itself remove tape/evaluator/host drift (section
4.2 A); matching and recording their realized conditions is what controls it,
and residual variation can remain. Conversely, section 7.3's matched-window
replay disagreement does not automatically establish that the historical panel
cannot predict paper or that all P3 work must wait for P4. Attribute the mismatch,
state which comparison or hypothesis it undermines, and preserve useful
unaffected diagnosis. Keep matched initial inventory and other state explicit;
“prod never [starts flat]” is too categorical.

Bound the “no decision→order→ack→fill latency modelled anywhere” absence claim
to the source surfaces actually inspected and the relevant latency mechanism.
Likewise distinguish a signature added after an incident from a universal claim
that parity is complete for every incident. Source observations support specific
blind spots, not global completeness or absence.

### 5. Preserve existing ownership when describing the proposed roles

Sections 9.1/9.2 describe P4 as owning equivalence semantics, admissibility rules,
hash epochs, and validity conditions. P4 currently owns a research proposal;
peer agreement does not transfer the owners' runtime/evaluation surfaces.
Describe these as proposed responsibilities to reconcile with existing owners
if selected. Ordinary tasks/subtasks plus artifacts already represent a finite
round in CM without a periodic trigger; section 9.2's P8 question should start
there rather than imply missing native representation.

Keep the dated scope of Owner rulings visible. Unknown current freeze status
remains unknown; don't convert other proposals' consultation into authorization.

### 6. Align source claims and NOTES with the corrected proposal

The deleted default runner is a verified compatibility risk **when the script
is omitted and the submitted checkout lacks that module**. Say this explicitly
instead of “a submission relying on either default fails” without the checkout
condition, and use “source default” rather than “live source default.” Explicit
script selection exists. No deployed default or failed run has been inspected.
I retain this issue for CM dedup/triage; no repair is selected.

NOTES still contains current-looking superseded claims: “no numeric ... artifact
exists — TRUE,” the mandatory new machine-class record, no-band preference, and
P8 not launched. Label the historical consultation as such and point to the
correction. Its evidence table says `ac1_pins_sha` is inert on every deploy today
although no deployment was inspected; use the conditional empty-path/source
statement from the proposal. A current warning linking a historical baseline
archive from a document also marked historical is not itself an active bug;
keep that a possible documentation clarification absent a misleading active
consumer. Do not create a cleanup requirement from archive age.

## Verification and follow-through

This review used committed repository documents and targeted source reads
(reference-policy inputs, study gates, and L4's historical report), plus the
previous CM provenance source inspection. No runtime, database, cloud, or
backtest operation was performed. Return one consolidated revision to both
coordinators; Initiative-Bridge will review it personally. Document revision is
already authorized, while a selected round, code changes, and deployment are not.

## Rereview of `5e09618c`

After the full `e616b6dea` read, personally reviewed the consolidated proposal
and NOTES diffs through `5e09618c18c2bc72f00575c690780651584e049c`. Substantive
requests are addressed, including unseen evidence versus metric validity,
matched-state reconstruction, publication timing, conditional provenance work,
and proposed rather than transferred evaluator ownership. The published-realized
summary gap is now linked to existing task `58d0508b-c479-4266-8f57-baa1515f94d5`.

Three final wording corrections requested: remove “live” from section 3.3's
source-default heading; include pre-opportunity missing/out-of-range inputs in
section 3.7's mixed `no_trade` description; and scope section 7.1's routine-work
permission paragraph to work already authorized, preserving current proposal
research versus future live reads/backtests/implementation. No new investigation
or design round is requested. Final acceptance awaits that consolidated diff.
