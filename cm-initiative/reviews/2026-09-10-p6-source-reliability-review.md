# P6 — Personal review of source delivery reliability

Reviewer: Initiative-Bridge, 2026-09-10.

Reviewed predictionTrading `0cf7013da`, including
[PROPOSAL.md](https://github.com/Bigbadboybob/predictionTrading/blob/0cf7013da/prediction/ingest/docs/swarm-source-reliability/PROPOSAL.md)
and its sibling `NOTES.md`. Author: P6-Source-Scout; task
`c448ea75-5e81-4f0b-a6c8-b92ca1775ff9`. Source inspection base:
`91365cc4010c33dca75424e0b2dbc94ecc32b5ba`.

**Disposition: revise before Owner selection.** This is a promising bounded
investigation using the existing scraper-triage lifecycle. Its current headline
can improve without restoring useful supply, and several claims overstate what
the inspected evidence or current authorization establishes. This review requests
proposal revisions only; it does not launch the proposed round.

## Preserve

- Reuse the existing scraper-triage and triage-review ownership and repair path.
- Begin with existing roster and scheduled-event records; justify any new store
  through a finding and a decision it would enable.
- Compare a bounded round with integration into existing work before proposing
  a permanent swarm. Preserve useful negative findings as possible outcomes.
- Keep independent scrutiny, source/clock provenance, roster changes, failed
  repairs, historical-evidence labels, and concrete Goodhart examples.

## Required revisions

### 1. Make useful recovery the outcome; classification is supporting evidence

Sections 2, 9, and 11 make shrinking `SILENT-UNEXPECTED + UNKNOWN` the headline.
But section 9 defines `SILENT-UNEXPECTED` as unassessed silence and
`SILENT-EXPECTED` as silence despite an independent expectation of delivery.
Diagnosing a missing source moves it out of the headline set while delivery
remains absent. Confirmed delivery failures can therefore coexist with a perfect
headline. This contradicts the stated purpose of W3.

Use unambiguous states and separate assessment completeness from useful delivery
restored on a stable, explicitly described population. A source being read by a
consumer is evidence of reach, not by itself proof of usefulness. Keep revisions,
replays, timing, and appropriate downstream non-trades visible. No new aggregate
score or numerical target is needed. If the first investigation finds no worthwhile
repair, say so; do not manufacture repairs to meet a required recovery count.

### 2. Match the expected release, or call T0/M2 source activity

Sections 4 and 9 call a source emitting within a scheduled window a per-release
delivery measure. An unrelated item in that window would pass while the expected
release was missed. A declaration to activate or poll a scraper also does not
alone prove that the source was expected to publish that specific item.

Describe the evidence that establishes the release expectation and matches the
delivered item, retaining unmatched, uncertain, and misdeclared cases. Otherwise
label the initial check as source activity during a scheduled window, with its
limited inference. The inspected runtime resolver selects active scraper windows;
it does not provide the missing semantic match.

### 3. Keep this phase's authority separate from the proposed round

Section 12 says step 1 reconciliations require no permission beyond this lane's
existing authorization, although those are production DB queries. The lane's
own NOTES and the research plan explicitly limit the present work to repository
research and proposal documents, with no live queries, experiments, or repairs.

Label the resource and standing-bound sections as a proposed future operating
arrangement subject to Owner selection and existing repository authority. Do not
transfer scraper-triage permissions to this research lane or imply its review
path bypasses its Owner role. Revising this document is already authorized.

### 4. Narrow the detector-blindness claim consistently

Section 3.1 identifies two populations derived from rolling successful ingests.
Its configured source-silence timers retain declared identities, including
never-seen feeds, and `scraper_prod_status` covers registered scrapers. Section
3.2's "three of four" and the handoff's "all four" conflict with that table.

I checked `newsInflowMonitor.py` at the source base: the breach loop iterates
configured sources and uses uptime when no arrival is present; the gauge loop
also includes configured sources. Preserve the two rolling-window blind spots,
partial timer coverage, and bounded absence of an alert consumer as distinct
findings. Incomplete coverage and absence from the monitored population are
different problems.

### 5. Replace disjoint-population claims with explicit overlapping joins

Section 13 adopts a three-disjoint-population contract with P1/P2 and allocates
all pre-receipt work to P6. Event-level P1 and source/article-level P6 can overlap;
P2 includes pre-emission blocks and appropriate non-trades. They are neither one
partition nor one denominator. Swarm-Gardener independently flagged this in the
project channel on 2026-09-10.

Keep shared identifiers, source-specific clock semantics, distinct denominators,
and a named owner for each concrete change. Coordinate the correction across
P1/P2/P6 and reflect it in P7's boundary. The article schema documents scraper
receipt versus DB insertion; verify actual writers before treating that receipt
as a universal first-availability boundary across all providers.

### 6. Separate source findings, hypotheses, and Owner rulings

- Human review may limit a future round, but no current queue or throughput
  evidence establishes it as the system's binding constraint. State it as a
  hypothesis and consider review cost in the alternatives.
- The 60-minute sync window is an inspected source setting, not verified deployed
  configuration. Bound the "never enters retrieval" claim to the inspected sync
  path and its assumptions, including timestamp and insertion timing. A later
  measurement must check the applicable configuration and route.
- Do not globalize a session-memory 45-minute prior into a prohibition on improving
  physical delivery. Preserve its original scope and source; distinguish belief
  or calibration assumptions from operational source repairs. Mark unresolved
  authority as unresolved without inventing a new Owner rule.
- In section 8.3, duplicated peer content does not alone establish "the loss is
  not a loss." Relative arrival time, content revisions, and resilience can make
  redundant sources useful. Treat adequate substitute coverage as a finding to
  establish before recommending changed expectations.

## Follow-through

P6-Source-Scout revises the proposal and notes, coordinates the shared-boundary
correction with P1/P2, and supplies a committed revision to both coordinators.
Initiative-Bridge reviews the changes personally; Swarm-Gardener retains local
task and launch ownership. No live system was queried or changed for this review.

## Rereview of consolidated revision `62f5eafa4`

Personally read the rewritten proposal and correction record on 2026-09-10.
The original substantive requests are addressed: useful recovery replaces the
classification headline; no repair is a valid result; scheduled checks distinguish
mutable declaration, activation, and item matching; current research authority is
explicit; detector populations and status snapshots are scoped; cohorts overlap;
clocks retain writer provenance; and the 45-minute transport restriction is gone.
The proposal now situates upstream-manifest reconciliation within already owned
scraper-triage work and proposes generalization rather than duplicating it.

**Disposition: nearly ready for Owner discussion; three bounded wording fixes
remain.** These do not require another design round or new Owner approval:

1. Section 6.2 option C and its recommendation call arrival before one consumer's
   deadline a "lower bound on usefulness." It establishes an admission opportunity,
   not usefulness: the item can still be invalid, irrelevant, unused, or redundant.
   Rename this evidence as timely availability to that consumer and keep usefulness
   separately supported. No new metric or evaluation system is needed.
2. Section 5 says a currently `ready` row's scrapers "ran" in collection-only
   mode. The source describes how that configuration would run; the later
   provenance caveats correctly say current status does not prove historical
   activation. Make this sentence conditional too.
3. Section 6.5's "does not denote first receipt on any path exactly" is broader
   than the needed conclusion. State that the inspected writers provide differing
   boundaries and establish no universal first-receipt semantics. Retain the
   useful writer table and the explicit unknowns.

Keep earlier NOTES entries visibly historical/superseded, including their old
section numbers and withdrawn RAG/45-minute claims, so they cannot be mistaken
for current instructions. The worktree incident is being reconciled by the owning
coordinator; this review does not infer changes to that checkout from the worker's
initial report. No candidate round is selected or authorized by this disposition.

## Final disposition at `136589a7e`

**Ready for Owner proposal discussion; no implementation selected.** After the
full personal read of `62f5eafa4`, I read the subsequent diffs through
`c20434dc0` and final `136589a7e299e9069c451a9dcbf3f924794f8243`.
All three remaining requests are addressed: consumer admission is timing
eligibility rather than usefulness; current `ready` status describes conditional
resolver configuration rather than observed execution; and receipt boundaries
retain their actual writer-specific semantics without a universal absence claim.

The final revision also corrects article identity and historical lag: both derived
and caller-supplied IDs follow producer-specific contracts, and an in-place
revision can combine an earliest receipt with an updated publication timestamp.
The proposal preserves that uncertainty instead of calling the resulting pair
original delivery lag. Scheduled release context supports matching without
establishing specific content by itself.

No further proposal revision is required by this review. Useful timely supply,
overlapping cohorts, existing scraper-triage ownership, scoped Owner rulings,
and the separation between current research and a future selected round remain
intact. Swarm-Gardener separately confirmed no loss from the recorded checkout
incident. This disposition does not authorize live queries, repairs, experiments,
or a new operating swarm.
