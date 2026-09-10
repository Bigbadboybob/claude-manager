# predictionTrading research plan and SEJD feedback — coordinator review

Reviewer: Initiative-Bridge, 2026-09-10. **No blocker to the authorized research
and proposal sequence.** This reviews the preparation, not a candidate swarm
launch or implementation. Candidate documents will each receive a later review.

## Material reviewed

Read the committed predictionTrading documents at `af7e3f732`:

- [RESEARCH-PLAN.md](https://github.com/Bigbadboybob/predictionTrading/blob/af7e3f732/agent_docs/swarm-focused-design/RESEARCH-PLAN.md).
- [IDEAS.md](https://github.com/Bigbadboybob/predictionTrading/blob/af7e3f732/agent_docs/swarm-focused-design/IDEAS.md).
- [SEJD-FEEDBACK-2026-09-10.md](https://github.com/Bigbadboybob/predictionTrading/blob/af7e3f732/agent_docs/swarm-focused-design/SEJD-FEEDBACK-2026-09-10.md).
- [NOTES.md](https://github.com/Bigbadboybob/predictionTrading/blob/af7e3f732/agent_docs/swarm-focused-design/NOTES.md).

Also read the actual brief for research task
`59a67007-7a71-412a-87d4-bf535df42893` through CM. It tells Fable to read Owner's
idea list before forming its own ideas, without anchoring on Swarm-Gardener's
AI list; preserves bounded research scope and explicit file ownership; and
requires confirmation of the requested model. The task is running. Effective
runtime model confirmation remains with its coordinator.

## Assessment and comments for candidate authors

The plan follows Owner's requested sequence and separates the Owner list from
AI synthesis. Existing Owner ideas remain visible, including deferred directions.
Independent brainstorming, agent review, collaborative proposal writing, and
personal CM review all have a place. Research completion does not select the
proposed production swarms. The combined-versus-separate ingest/follow-through
choice remains open for reasoned proposals.

Carry these evaluation distinctions into the candidate documents:

- **Detection, retrieval, explanation, and actionability are different claims.**
  Define the population and denominator appropriate to each. In particular,
  `unscored_rag` with `passed=False` represents retrieval without a causal
  assessment, not a validated explanation or a scored rejection. Missing or
  failed evaluation should not silently count as evidence of no explanation.
- **Event-time and decision-time availability matter.** The merger and separate
  scorer currently anchor retrieval differently. Proposals should account for
  processing lag and when the system actually had the article, as well as the
  article's reported publication time. A retrospectively explanatory article
  need not have enabled a useful trade at the time.
- **Appropriate non-trades remain valid outcomes.** Diagnose stages and reasons
  before proposing improvements; increasing passage through protective gates
  or increasing trade count does not by itself establish better outcomes.

These refine the plan's existing evaluation questions; they are not a new
scoring scheme or additional Owner gate. The P&L proposal should likewise keep
evaluation integrity and permitted optimizer changes explicit, using the
existing auto-backtest system as Owner requested.

## Source checks and SEJD transfer

Inspected the cited RAG source at predictionTrading commit `47076cd8e`:
`AttributionMergerService._rag_search` supplies `end=event.time`; its audit
records use `unscored_rag` with null scores and exclude RAG-only entries from the
creation batch. `MomentumAttributionScorer` queries without `end`, and
`SlidingWindowArticleDB.query` defaults that argument to current UTC time.
These support the documented evaluation questions. This was source inspection,
not an independent deployment check or measurement of current coverage.

The SEJD note preserves the coordinator's attribution and the limits of its
working-copy evidence. Its observation that a microscope HEAD does not pin
uncommitted result files is useful and should survive future handoffs. Obtain
immutable source references before treating those files as reproducible
evidence; no need to interrupt SEJD's work to continue this brainstorm.

The feedback sharpens the CM candidates: current state versus history supports
the context-entry idea; component checks versus promised behavior supports
outcome/evidence handoffs; source-content mismatches and changing baselines
support tracking affected results; repeated approval requests support retaining
the scope of existing Owner decisions. These are links between reported needs
and discussion ideas, not evidence that a particular feature will solve them.
SEJD's desired close Owner collaboration remains part of its intended operation.
