# P1 — Personal review of momentum coverage

Reviewer: Initiative-Bridge, 2026-09-10.

Reviewed predictionTrading `e426f6885`, including
[PROPOSAL.md](https://github.com/Bigbadboybob/predictionTrading/blob/e426f6885/applications/scraperGeneration/docs/swarm-coverage/PROPOSAL.md)
and `NOTES.md`. Author: Coverage-Scout; task
`a9ce0270-2b3d-4e1e-b84b-e9cff06d9e02`. Inspected source base: `91365cc40`.

**Disposition: revise before Owner selection.** Preserve the complete
detection → existing explanations → source acquisition → later usefulness loop,
the existing RAG starting point, and the preference for existing audit records.
The assessment gap is well supported. Retrieval asymmetry is a credible research
question, but the document currently turns it into a proven directional bias and
uses stronger causal conclusions than its proposed evaluation could establish.

## Required revisions

### 1. Do not promote unequal retrieval settings into a proven coverage bias

Sections 3.3 and 9 claim any current existing-supply estimate is biased downward
by construction. The 30-minute RAG window, one-day web filter, and candidate
limits are different configured mechanisms; they do not establish 48 times the
effective event-relative reach or the direction and size of an estimate's error.
The web filter is request-relative, subject to indexing and source filtering;
RAG is event-anchored and subject to admission and retention. Accepted causal
assessments can also be false positives, as the document already recognizes.

Keep the source finding that the merger's RAG records are unscored and excluded
from the creation batch, alongside the separate scorer's limitations. State that
the current artifacts do not establish the requested coverage comparison. Compare
retrieval alternatives as hypotheses rather than making parity with the web
configuration the presumed correct destination. Low explanation yield after
widening would still allow retrieval misses, timestamp errors, weak evaluation,
or moves without an article cause; it would not prove missing source coverage.

### 2. Make the proposed comparison identifiable and honest about history

The current merger has no scored RAG baseline. Changing assessment and retrieval
together cannot by itself attribute a change in explained fraction to retrieval.
Describe comparable evaluations of current and candidate retrieval using the same
stated evaluator, with assessment failures kept separate. Compare reusing the
existing scorer with extending the merger before selecting a second assessment
path; their different purposes and costs matter.

For a historical comparison, distinguish records now available from the content
and versions actually available at the event. Pinning today's DB/configuration
does not recreate the old vector store or online index. State what existing
artifacts can reconstruct, what remains unknown, and whether a bounded prospective
comparison is a better alternative. This remains a proposal for later selection;
do not run either evaluation in this document phase.

### 3. An unexplained move is not automatically a missing-news failure

Section 10 calls an unexplained move a gap to close. The proposal itself lists
mechanical and informed-flow alternatives and acknowledges uncertain onset and
model labels. Preserve detection quality, causal explanation quality, historical
availability, and actionable source-acquisition opportunities as distinct claims.
Do not reward attaching an explanation where the right answer is uncertain or
there is no demonstrated public-news cause.

The independent census is useful as a candidate reference, not truth. Requiring
it to share no parameter with the detector is stronger than independence from the
detector's selected output and needs justification. State its intended population
and reference assumptions. The adversary's mandatory census comparison in section
7.2 also conflicts with section 9 allowing a first conditional round before D1;
make the requirements agree without forcing a new census as a prerequisite to
every useful investigation. Keep both marginal-pair scrutiny and checks for
missed valid explanations, without inventing review quotas.

### 4. Correct the shared populations and clock contract with P2/P6

Events, sources, articles, and signals overlap through explicit joins; they are
not three disjoint populations. P2 includes pre-emission filtering, downstream
failures, and appropriate non-trades. Organizing one combined effort does not
require one headline number. Compare combined versus coordinated ownership on
actual integration and review cost, with separate denominators in either shape.

Carry timestamp provenance and path-specific availability semantics. A single
article UID or current DB row is not automatically a complete arrival/version
history. Swarm-Gardener's additional source checks identify poll-start stamps,
wrapper-yield stamps, and mutable content under stable UIDs; reconcile those
findings before using the shared tuple as proof of historical availability.

### 5. Correct authority, configuration, and dependency claims

- The current authorization is repository research and documents. Sections 8–11
  describe future parameter changes, DB reads, replays, and LLM spending; absence
  of a historical prohibition does not authorize them in this phase. Label the
  proposed operating bounds as future choices and retain existing service owners.
- The aux service affects scraper proposals and shares infrastructure with other
  paths. It does not place orders directly, but "nothing blocking" and "no
  actuation" do not establish that all changes are isolated or ownerless.
- Reuse the document's own source-versus-deployment caveat consistently for
  thresholds, eviction, sync windows, and enabled legs. For example, inspected
  wiring constructs Perplexity only when its configuration enables it.
- The 45-minute ruling is not an operational delivery prohibition.
  Swarm-Gardener traced it to market absorption after onset, reporting sources
  `sejd/docs/rulings-2026-08-28.md:306–330` and the corresponding session memory.
  That file is absent at this proposal's source base, so this is attributed
  coordinator evidence, not a falsely pinned source read by this reviewer.
- Reconcile the proposed scored historical replay with section 13's claimed
  no-significant-LLM-backfill restriction. Trace any applicable ruling's scope
  and present feasible bounded alternatives; do not add a blanket new gate.

## Follow-through and evidence

I read the full proposal and notes and checked source wiring, RAG dedup/batch
exclusion, web parameters, and scorer persistence at `91365cc40`. The earlier
research review already checked the temporal scoring guard. No live query, model
job, or implementation ran. Coverage-Scout should revise its owned documents,
reconcile the shared contract with P2/P6, and return a committed revision for both
coordinators' review before Owner selection.

## Final personal read of `8bbfcd823`

**Ready for Owner proposal discussion.** I personally read the consolidated
proposal and its revision-response record on 2026-09-10. The substantive review
requests are addressed: the directional-bias claim is withdrawn; exploratory
descriptive reads remain usable with their limits; retrieval comparisons use a
common evaluator; existing scorer versus merger extension is an explicit choice;
historical reconstruction and prospective capture limits are stated; unexplained
events retain uncertainty; and the shared populations, clocks, and authority are
correctly scoped. The census is an optional reference rather than a prerequisite
for every conditional investigation.

The proposal now offers a reviewable bounded round using existing infrastructure,
with no new production swarm, implementation, measurement, or spend authorized.
No further document revision is required by this personal review. Owner still
chooses whether and how to pursue it, including evaluation definitions, resources,
and the preferred relationship to the other proposals. No live impact or baseline
has been established by these source reads.
