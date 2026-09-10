# P2 — Personal review of article follow-through

Reviewer: Initiative-Bridge, 2026-09-10.

Personally reviewed the proposal and notes at predictionTrading `f16a13d1d`,
then revision 2 at `e9764a371` as it arrived during review:
[PROPOSAL.md](https://github.com/Bigbadboybob/predictionTrading/blob/e9764a371/prediction/application/alphamodels/docs/swarm-follow-through/PROPOSAL.md).
Author: Followthrough-Scout; task `90e23c72-6c32-4cc1-bf54-17f57c68be66`.
Source base: `91365cc40`.

**Disposition: further revision before Owner selection.** Revision 2 fixes
substantial problems found independently by both coordinators: emitted-only
scope, disjoint populations, definitive alpha labels, missing evaluation states,
unmeasurable reconstruction validation, trace-cohort claims, and historical rows
that a future repair cannot restore. Preserve those corrections and appropriate
non-trades, participation versus contribution, and existing SEJD/ES ownership.

The requests below apply to `e9764a371`; already corrected points need not be
redone. This review authorizes document revisions only, not the proposed round.

## Remaining revisions

### 1. A negative signed alpha can coexist with same-direction modeled profit

Section 4.3 now correctly rejects a definitive label and a rigorous upper bound.
Its explanation still emphasizes a wrong-direction call with money on the other
side, and concludes that the estimate splits cases into "nothing obvious was
there" and "something was there." That split is not supported by the headline.

The implementation uses `sign(pDelta) * (long_alpha - short_alpha)`. A positive
signal with long profit 10 and short profit 20 gives -10 despite a profitable
modeled same-direction leg. Use both legs, their strategy assumptions, and
missing/failed estimates explicitly; do not use the sign to designate no modeled
opportunity. Nor does hindsight profit alone establish whether the original
decision was reasonable. Keep policy consistency, estimated opportunity, causal
decision quality, and realized outcomes distinct in the round's verification.

Compare the existing `signal_rule_replay.py` / job where causal strategy evidence
would help. Its `TradeRule` and causal history view are useful existing options,
with execution limitations; it is not a replacement for SEJD/ES evaluation.
Inspect suitability only. No run or new evaluator is required by this review.

### 2. Do not infer that every infrastructure drop is an unintended loss

Sections 2 and 13 treat infrastructure stage names as a built-in definition of
"unintended." Timeouts, queue shedding, warmup admission, and lock budgets can
be deliberate safeguards. Being distinct from a relevance judgment makes a case
worth investigating; it does not establish a defect or authorize bypassing the
existing resource policy. Authentication failure is also not generally fixed by
retrying it.

For a candidate recovery, examine the existing policy and owner, the actionable
failure, and whether useful information can still arrive within its relevant
time. Verification should include fresh-work latency, resource pressure, repeated
side effects, and stale/repeated content as applicable. A transient-failure fixture
that later succeeds proves a mechanism, not useful recovery. Preserve the option
of no repair when the safeguard is doing its job. Evidence-only D1/D2 fixes
should be prerequisites only where a particular finding depends on them.

### 3. Preserve historical identity and updates at the right granularity

Section 3 still says every revision has a new UID and new clocks. Source shows
`Article.ensure_uid` preserves supplied semantic IDs; `ArticleDB` can upsert new
content and publication time under the same UID while retaining the initial
reception stamp. Current rows therefore do not establish every historical version.

Likewise, mandatory latest-per-signal projection is suitable for some terminal
summaries, not every follow-through study. It can erase a flash/refined change
needed to assess the information available to an earlier decision. Specify
terminal versus as-of/update-preserving views for the question being asked.

Some early drops happen before a market pair exists. Keep article-level evidence
where appropriate rather than inventing a market-pair denominator for those
cases. Conceptual inclusion and imperfect observable joins are separate matters:
URL-versus-UID coarseness makes inclusion harder to measure, not logically false.

### 4. Keep mint eligibility, consumption, and decision influence separate

Section 9 now recognizes that reconstruction predicts mint outcomes rather than
measuring disagreement. Extend that correction through the actual consumption
path: SEJD `build_claim_schedule` filters post-grid arrivals, deduplicates marks
with the same primary article, and applies minimum magnitude before consumption.
A record that a mark was minted does not prove those later steps or its causal
influence on a decision. Tie any proposed interface to the exact ambiguity it
would resolve, retaining SEJD's coordinator as owner.

Section 7.3 still says a large S3 establishes an upstream center of gravity,
although section 6 correctly describes multiple possible causes. Change the
recommendation to investigate those alternatives; S3 alone cannot make a new
receipt the dominant blocker or justify pausing all other useful work.

### 5. Correct remaining clock, era, and authority wording consistently

- The intent-tap source says real parent provenance arrived on 2026-08-20 after
  initially being hardcoded empty. The 2026-08-19 config flip does not establish
  usable signal linkage. Separate channel existence, field availability, and
  observed coverage in sections 3 and 12.
- Remove the remaining "standing prior on delivery" restriction in section 5
  and NOTES. Swarm-Gardener traced the 45-minute ruling to market absorption,
  not publication-to-ingest transport. Treat that as the scoped ruling, with
  its original source; no fresh Owner ruling is needed to correct this wording.
- Section 9 says P2 can run SQL reconstruction "now"; section 11 lists reads
  and D2 repair as independently authorized. The present phase authorizes
  repository research and documents only. Label these as future proposed
  operating bounds. Proposal ownership is not ownership of all pipeline code.
- Rewrite or explicitly supersede stale NOTES claims about universal new UIDs,
  mint failure in every artifact, the emitted-only boundary, and the delivery
  prior. Readers should not have to infer which contradictory statement wins.

## Evidence and follow-through

I read both proposal revisions and their notes and checked the alpha formula and
finite grid, evaluation schema, article UID/upsert semantics, intent-tap provenance
history, SEJD claim-schedule documentation, and the existing causal replay surface
at `91365cc40`. Additional writer and era details came from Swarm-Gardener's
independent source review. No live query, model job, test suite, implementation,
or deployment ran for this documentation review.

Return a committed revision to both coordinators and reconcile the population
and clock contract with P1/P6. The candidate remains unselected for implementation.

## Personal rereview of `a97faa1a6`

Personally read the full revised proposal at
`a97faa1a6c409da06a61897f819cba5acc7819c3` and its response mappings and notes.
The original substantive requests are addressed: observed stage outcomes are
primary, both economic estimators have explicit limits, `no_trade` can reflect
missing inputs, pre-mapping losses retain article grain, S3 remains causally
ambiguous, and recovery must consider deadlines, load displacement, fresh work,
and side effects. The proposal does not grant current execution authority.

**Disposition: bounded consistency corrections before Owner discussion.** No
new investigation or redesign is requested:

1. Section 4.3's bold bullet still says “`≤ 0` designates no modelled opportunity.”
   It needs “does not designate”; its own worked example and NOTES correctly say
   the opposite of the current opening sentence.
2. Section 3's table still labels policy replay “The no-hindsight artifact.”
   NOTES, “Findings from source,” item 3 also claims “no hindsight by construction”
   and that status distinguishes policy declines. Replace both current summaries
   with section 4.3's restricted future-book access, unconstrained input
   provenance, campaign selection, and mixed `no_trade` bucket. Historical
   response entries may remain historical, explicitly superseded where needed.
3. Section 5 defines Pop-I as everything entering the pipeline **and lost before
   emission**, then says it contains emitted Pop-M. Those cannot both hold.
   Define Pop-I as all entrants through the relevant stages, with pre-emission
   losses a subset, preserving article grain before mapping and pair grain
   afterward. This matches the intended nesting already explained in section 3.3.
4. Keep the proposed round consistent with the accepted conditional dependencies.
   Section 4.3 makes policy/alpha optional, and section 13 makes D1/D2 prerequisites
   only where needed. Yet section 13 Phase 0 lists all estimator coverage and all
   mint/ES coverage work as prerequisites, and section 8 says W3 rules settle
   first for W1 generally. Scope each prerequisite to the finding it supports;
   a useful pre-emission investigation need not wait for unrelated policy replay,
   mint reconstruction, ES provenance, or evidence repairs. Keep their optional
   diagnostic routes available without requiring them to produce a result.

The revised article identity and receipt-clock caveats are useful. The earlier
anchor now correctly shifts the forward window; the fixed duration is capped
per signal, so the parenthetical “except where the cap binds” should not imply
that moving only the anchor changes that duration. This is a wording clarification,
not a request to change estimator semantics.

After those changes, return one consolidated commit for final diff review. No
live queries, recovery experiment, code change, or new swarm is selected here.

## Final disposition at `3eea07af0`

**Ready for Owner proposal discussion; no implementation selected.** Following
my full personal read of `a97faa1a6`, I read the final diff through
`3eea07af0a107803263bcb3965e0196fe34165f4` and the updated current NOTES finding.
All five consistency corrections are applied: the negation is restored, policy
replay summaries carry their actual limits, Pop-I includes all entrants with
losses as a subset, prerequisites are scoped per finding, and moving the alpha
anchor does not change its per-signal capped duration. No further document
revision is required by this review. Live evidence remains unmeasured, useful
recovery remains a hypothesis, and no proposed round or code repair is authorized
by this disposition.
