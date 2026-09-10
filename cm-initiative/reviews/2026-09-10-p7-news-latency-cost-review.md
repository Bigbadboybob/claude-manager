# P7 — Personal review of useful news-decision latency and cost

Reviewer: Initiative-Bridge, 2026-09-10. Read the full proposal and NOTES returned
by News-Perf and written by Swarm-Gardener under
`prediction/application/alphamodels/docs/swarm-latency-cost/` in its coordinator
checkout. These were an **uncommitted draft**, expressly shared for review;
no committed candidate is accepted by this review. Proposal SHA256 at read:
`c25be2b49bc1971ade27b3c1f026d0b40fe740efa207e118e93aedeb226ca2d4`;
NOTES: `1e0f7a9b34a6c92c51ff1e342f6353dc2536784a862d3987a220581ddb16510a`.

**Disposition: revise before Owner discussion.** Preserve resource cost and
latency as separate dimensions, the existing repair queue, conditional cost
attribution, and a bounded first investigation that may find no worthwhile
change. The taxonomy is useful if it classifies actual effects rather than
certifying a change from its name.

## Requested revisions

1. **A termination population is not an admission census.** Sections 3.3, 6.2,
   and W1 treat awaited drop inserts as a workload denominator containing every
   admitted unit. I checked `DropsClient` at `545c96a6f`: awaiting an insert and
   using `ON CONFLICT ... DO NOTHING` do not establish that every admission has
   exactly one terminal row. Preserve URL/version collisions, article versus
   pair grain, successful emission identity, units still in flight at the window
   edge, missing outcomes, and failed writes. State the observable population
   before dividing cost by it. WARN counts are observed loss evidence, not an
   exact known deficit or drop rate without the attempted-write denominator and
   logging coverage. No new ledger is required by this correction.
2. **A same-window comparison does not cancel confounding or shared-resource
   effects.** Sections 6.3/6.4 say shadowing measures the change's own effect and
   load confounding cancels; routing a deterministic fraction is also described
   as equivalent to the existing shadows. Separate paired shadow execution from
   routing disjoint production units. They have different selection, capacity,
   provider-cache, and serving effects. State the assignment unit and any shared
   gate/queue interference, input comparability, and candidate-dependent work.
   A matched temporal comparison is qualified evidence, not automatically valid
   from arrival rate and day type alone. Do not make sampled same-window A/B the
   only possible valid design or require every bounded first window to contain
   both weekday and weekend data. Choose the design for the claim and preserve
   its limits; no experiment is currently authorized.
3. **Bound equivalence to what was checked.** Sections 5/6.3/11 promote a code
   argument plus one before-fail/after-pass test to decision identity and immunity
   to Goodharting. A regression fixture verifies its mechanism; an equivalence
   check normally compares both versions and should cover declared inputs and
   effects. Timing changes to locks, permits, caching, cancellation, or deadlines
   can change which work runs, freshness, RNG/order, side effects, and overload
   outcomes. Keep the strongest feasible preservation evidence without requiring
   downstream P&L for a narrow structural optimization; state residual limits
   instead of claiming information cannot change or the metric cannot be gamed.
4. **Treat unused work and metric direction as hypotheses.** A speculative result
   later discarded can buy useful latency ex ante. A timeout can protect fresh
   work. Neither is automatically waste. The draft already knows this but its
   opening target and W4/round recommendation lapse into the stronger claim.
   Lower admission caps do not monotonically improve delivered p90, nor does
   narrowing a gate always remove the slow tail. Give the conditional Goodhart
   mechanism rather than an asserted direction. July's 0.9% example shows a
   historical population difference; generalizing a conditional metric is risky
   without needing that rate to remain small. July to September is two months,
   not fourteen months.
5. **Do not equate agreement or accounting with economic value.** Section 6.3's
   ending says V3 “would establish economic value,” despite correctly stating
   the estimators' limits earlier. These are model-conditional diagnostics and
   observed accounting outcomes; none alone attributes economic value to a
   latency change. A carried attribution key improves identity but does not
   make all costs unambiguous: calls can serve several decisions, retries and
   detached work need identity, writers can lose rows, and pricing/coverage can
   be incomplete. Keep the named decision an attribution change would enable.
6. **Scope ownership and source claims.** A disabled default is not proof that
   backtests never write phase timings; separate configurable behavior and the
   particular replay surface from a universal absence claim. The lack of a cost
   protocol in inspected paths is not proof no owner/loop exists. Proposal
   authorship does not grant P2 recovery-code ownership or P6 an obligation to
   run live clock measurements during this research phase. Reconcile proposed
   responsibilities with existing owners only if selected. Match section 11's
   blanket escalation of any removed unconsumed call to section 12's narrower
   proposal based on actual policy effects; a prior gate-changing verifier
   removal is not a universal Owner gate. Keep assurance changes explicitly
   reasoned and owned without claiming current shadows are the only evidence
   of model safety.

Swarm-Gardener reports the waterline and DATABASE prose repairs already committed
at `538404b5a`; label those items historical/resolved after checking that commit.
The absence of a retention policy in setup SQL remains a scoped question, not
proof of harmful live growth or permission to choose retention.

This review performed repository reads only, including the drop writer. It did
not execute queries, benchmarks, tests, code changes, or deployments. Please
incorporate both coordinators' comments into one committed draft for personal
rereview; the author remains read-only in the coordinator checkout.
