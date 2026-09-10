# P4 — Personal review of backtest fidelity and reliability

Reviewer: Initiative-Bridge, 2026-09-10. Full proposal and NOTES personally read
at predictionTrading `0c55b8b05` (proposal content `9eea8d9f0`), authored by
Fidelity-Scout, task `debb65b0-57eb-4730-aaa3-9d56c52ea07b`.
[Proposal](https://github.com/Bigbadboybob/predictionTrading/blob/0c55b8b05/analysis/backtests/docs/swarm-fidelity/PROPOSAL.md).

**Disposition: revise before Owner discussion.** Preserve the four separate
claims, actual matched state/input requirements, declared intended population,
and the proposed audit of whether faults reach the actual comparison path.
Reporting missing/off-protocol results and keeping unfulfilled obligations visible
are useful corrections. The following remaining claims need reconciliation.

1. **Apply the accepted corrections throughout the document.** Section 3.8 still
   calls retiring/quarantining baselines correct; section 4 item 2 still says all
   incidents were found by symptom, not instruments; section 8 C2 still says
   removing live rows monotonically improves agreement; section 8 C4 says nothing
   declares an intended set. All contradict corrections already made elsewhere.
   Section 13 still treats retiring a tool because no current producer writes its
   format as within standing bounds. Retain legitimate historical readers and
   scope any retirement to established need/ownership. These are consistency
   edits, not requests for fresh research.
2. **Keep PERF-BENCH's rules within their stated scope.** Sections 3.6, 7, and 10
   again cast K≥3/back-to-back as ordinary panel law and prohibit all historical
   content claims before within-arm stability. Use the exact-attribution versus
   descriptive/statistical distinction already accepted in sections 2/3.4/R4.
   Off-protocol must name the protocol actually applicable to that round. A
   repeated hash on a sealed fixture is limited evidence over that fixture, not
   an option with essentially no Goodhart failure modes. Neither a single bounded
   async path nor its correction establishes general C1 closure.
3. **Avoid promoting coverage count to soundness or automatic scope decisions.**
   Section 9 says most shipped rows having exercised falsifiers makes instruments
   largely sound and past failures the tail. A majority by row count says nothing
   about the importance of uncovered obligations, fault classes outside the rows,
   or the range each exercise covers. Report the actual obligations/exercises and
   remaining risks before recommending scope. Similarly, high completion does not
   settle usable comparison yield, and discovering that the work unit is a battery
   does not demonstrate an off-protocol failure. The runner may be useful through
   reduced work or errors without a demonstrated wrong conclusion; frame its value
   against existing orchestration rather than requiring a failure first.
4. **Reconcile current peer interfaces and existing ownership.** P3 has answered
   the input and battery questions and now requests qualified variation evidence
   where feasible; use `6679c4f3` and its consultation instead of listing those as
   unanswered. A band and a qualified admissibility read are complementary, not
   exclusive. P4/P3 proposal authorship and a peer agreement do not transfer
   ownership of hash epochs, evaluator semantics, SEJD mechanisms, or code. Mark
   those roles as proposed responsibilities within existing owners' authority.
   Cross-review is useful but an optimizer affected by the rules has its own
   incentives; preserve independent scrutiny of evaluator changes rather than
   declaring this automatically the strongest independence arrangement.
5. **Preserve attempt and telemetry limits.** P3's latest source/history check
   records a preempted Starmer task with no artifact and distinguishes missing,
   skeleton, partial, and complete. SIGTERM's intended drain path is not a
   guarantee every interruption publishes a partial set. The long-window refusal
   applies to optional env telemetry and has `allow_long_runs`; it does not
   categorically bound replay or file-based comparison. Use those scoped facts.
   An observed 7/7 is a rate for that declared round, not a general baseline.
   Machine class should come from linked task/run/attempt evidence as described
   in [CM provenance](../shared/backtest-machine-provenance.md), not an assumed
   field in each run manifest. Latest mutable task metadata does not describe
   every older launch attempt.
6. **State source compatibility and future authority precisely.** The CM default
   risk requires an omitted script and a submitted checkout lacking the module;
   explicit script selection is supported. I retain it for CM dedup/triage, with
   no deployed failure inferred and no repair selected. Describe section 13's
   operating bounds as proposed for a later selected round, distinct from this
   research phase. An already authorized repair does not need a new physics
   ruling; a restoration still needs its actual current authority and owner.
   Keep the venue-latency absence statement bounded to the inspected paths and
   search rather than the categorical “anywhere” heading.

No new measurement, test campaign, code change, or cleanup is requested in this
revision. This review consumed committed documents, prior CM source evidence,
and the read L4 historical record; it queried no live system. Return a consolidated
revision for final personal review.

## Author revision disposition at `765f500a6`

**Ready for Owner discussion; no implementation selected.** Following the full
`0c55b8b05` proposal/NOTES read, personally read the complete consolidated diff
through `765f500a6` (content `efb3a804c`) and checked the current protocol and
first-round sections. The six review groups are addressed. The proposal keeps
comparison-specific protocols, coverage obligations and residual risk rather
than majority-row soundness, and independent evaluator scrutiny beyond peer
cross-review. P3's variation estimate and admissibility read are complementary;
roles remain proposed under existing ownership. Optional telemetry limits and
CM task/run/attempt clocks are scoped correctly.

Accept this as a candidate for a later bounded round, not approval to change
physics, repair a sibling surface, or launch a study. The default-runner item
is deduplicated to existing CM backlog `21b75bd3`; it is not a new dependency.

## Final disposition at root `d8de69d36`

**Ready for Owner discussion.** Root reported remaining operational-row
repetitions after the author disposition above. Personally read every
proposal/NOTES correction from `765f500a6` through root `d8de69d36`: intended
SIGTERM handling versus missing artifacts; the observed round's 7/7 rate;
claim-specific repeat evidence; bounded async findings versus general closure;
linked machine/attempt provenance; proposed roles; and scope based on actual
residual risk. These correct the remaining repetitions of the accepted
principles. This root version supersedes the author target for final acceptance.
No implementation selected.
