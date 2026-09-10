# P5 — Personal review of backtest speed and resource efficiency

Reviewer: Initiative-Bridge, 2026-09-10. Full proposal and NOTES personally read
at predictionTrading `c45592435`, Replay-Speed, task
`be72e25f-52c0-4b68-9096-de0deff3cb9e`.
[Proposal](https://github.com/Bigbadboybob/predictionTrading/blob/c45592435/analysis/backtests/docs/swarm-performance/PROPOSAL.md).

**Disposition: revise before Owner discussion.** Preserve inclusive turnaround,
behavior preservation as distinct from ownership, the post-timer digest/write
candidate, declared populations, and the explicit limitations on mutable task
metadata. The initial revision corrects several substantial errors. Remaining
requests are bounded to the proposal and its source statements.

1. **The launch timestamp is not admission or creation start.** I inspected CM
   `api/dispatch_daemon.py:244-255` and `dispatch/vm.py:120-127`, unchanged from
   `ecd41d6`: `launched_at` is assigned after `_launch_backtest_vm_sync` returns;
   that path waits for instance creation (`op.result()`) and gets the external
   IP first. Therefore section 3.2's “queue wait” includes completed creation,
   and “inclusive worker wall” starts after that interval, with worker startup
   possibly overlapping it. The residual cannot simply be said to contain
   provisioning. Name the observed endpoints and preserve unknown phase/attempt
   boundaries before attributing or subtracting. See the new
   [CM source addendum](../shared/backtest-machine-provenance.md#launch-timestamp-boundary-source-addendum-2026-09-10).
2. **Timing share is not removable share.** Section 7 recommends the
   behavior-preservingly removable share as the target, but existing wall and
   boundary counters measure time, not how much can be removed while preserving
   behavior. Distinguish measured timing, identified candidate mechanisms,
   estimated savings, and demonstrated improvements. A small post-timer or
   provisioning share rejects those candidates, not all evaluation-internal or
   other O4 opportunities. Local corpus availability likewise does not alone
   rule out repeated-copy/setup reuse. Section 7 B's inclusive slowest-cell time
   still depends on fleet and resource contention; it does not isolate from them.
3. **Keep equivalence evidence scoped.** Section 3.5's blanket “content claims
   are not available” and section 9's historical numbers “throughput-only” conflict
   with their own valid descriptive/statistical alternatives. Retain PERF-BENCH
   for its actual claims without globalizing it. A pre-process change can alter
   cache state, host contention, or data arrival, so #2a staying unchanged is not
   guaranteed simply because the changed function sits outside its timer.
   Likewise a watermark alone may not identify immutable corpus content when
   older records can be revised; state what the snapshot/invalidation contract
   must establish. No cache implementation is requested here.
4. **Separate proposed roles and current authorization.** Section 12 currently
   treats choosing among a bounded lane, a standing swarm, or existing ownership
   as coordinator implementation selection, and building a cache as already
   within bounds. Owner authorized research/proposal work here and explicitly
   retains selection of new swarms/initiatives. Preserve that checkpoint without
   asking again for already authorized repository/task/artifact reads or document
   revisions. Label later implementation bounds as proposed. P4/P3 authorship
   does not grant their documents authority over evaluator/hash semantics or
   existing loop code; use the corrected interface wording in P3 `5e09618c`.
5. **Do not infer current owners or classify unknown scope from old artifacts.**
   Q17 B1/B3's dated in-progress rows do not prove current active work, just as
   branch quiescence does not prove the owner absent. Keep both unknown. Section
   11 says nobody holds inclusive turnaround while section 4 correctly leaves
   ownership unresolved. The decision whether to supply an existing owner stays
   conditional. Root reports BACKTESTING's reopening note corrected at
   `538404b5a`; verify and mark the documentation observation resolved.
6. **Clean up the remaining consistency slips.** NOTES still asks whether Owner
   cares about inclusive turnaround as the question deciding the lane despite
   the accepted removal of that approval gate. Section 8 item 4's heading says
   cost/run falls while cost/decision rises, but its example describes cheaper
   cost with slower turnaround; on preserved work those are different statements.
   Use the tradeoff actually intended. Section 13's “no compute beyond reading”
   should be artifact-based analysis with unmeasured retrieval/analysis costs,
   consistent with the rest of the proposal.

This review used committed proposal documents and targeted CM source inspection.
It ran no backtest, query, benchmark, cloud operation, or test. Return one
consolidated revision for final personal review; no candidate is selected.

## Consolidated rereview at `5317f480b`

Personally read all proposal/NOTES changes from `c45592435` through
`5317f480ba2b727d1ced68cc341c6fee44a801d1`. Substantive corrections are in place.
Three stale formulations still contradict them: section 3.2's residual table
still includes provisioning; section 7's B/C table still claims fleet isolation
and a profile-readable removable share; and its E correction paragraph still
says out-of-timer work leaves replay throughput unchanged by construction.
Requested those in-place corrections and the corresponding NOTES sweep; no
further research required. Final acceptance awaits the corrected diff.

Personally checked predictionTrading `538404b5a`'s BACKTESTING change: the
historical-repeatability item is resolved in those docs. No benchmark was run.

## Final disposition at root `67ee54d1f`

**Ready for Owner discussion; no implementation selected.** Personally read
all root corrections from author `5317f480b` through `67ee54d1f`, following the
full proposal/NOTES and consolidated-diff reads above. This root version is the
accepted target. The final table and operating-step corrections align launch
clock boundaries, fleet effects, candidate-specific savings, local-corpus reuse,
historical descriptive/statistical evidence, and existing ownership. L4's
publication span is no longer compared to a preflight estimate as turnaround.

The proposed goal remains useful-result turnaround with preserved behavior and
artifacts; a bounded round may find its investigated candidates unhelpful
without claiming all O4 work exhausted. Acceptance selects no optimization,
experiment, new swarm, or resource budget.

## Final pin at root `db4d9a726`

Personally checked the diff from root `67ee54d1f`: PROPOSAL is unchanged and
NOTES now correctly scopes the restriction to exact between-arm attribution,
keeping statistical causal inference available with its controls. Acceptance
carries to root `db4d9a726`; this is the final reviewed pin.
