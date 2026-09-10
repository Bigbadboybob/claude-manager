# Shared artifacts — Swarm Focused Design

Initiative: `d381971d-0668-4ac0-8483-bf4111f3ddc2`. This directory holds results
and documents consumed across the initiative. Initiative-Bridge maintains the
index; each artifact has an explicit owning coordinator or approved task.

Read the [charter](../CHARTER.md), [plan](../PLAN.md), [protocol](../PROTOCOL.md),
and [project map](../PROJECTS.md) for current goals, scope, and onboarding.

## Index

| Artifact | Owner | Source/version | Status and use |
|---|---|---|---|
| [Interactive work plan](../PLAN.md) | Initiative-Bridge with Owner and Swarm-Gardener | CM hub branch; see decision log | Initial exploration directions, candidate review, and open choices |
| [CM coordination feature ideas](cm-feature-ideas.md) | Initiative-Bridge; research subagent draft | Source inspection at CM `849058f`; draft read by Initiative-Bridge | Four discussion candidates; no implementation selected |
| [Backtest machine selection and provenance](backtest-machine-provenance.md) | Initiative-Bridge | CM source `ecd41d6`; no deployed-state or run inspection | P3/P4/P5 input: differing source defaults, explicit override propagation, existing task VM metadata, and per-attempt limits |
| [predictionTrading research plan](https://github.com/Bigbadboybob/predictionTrading/blob/af7e3f732/agent_docs/swarm-focused-design/RESEARCH-PLAN.md) and [Owner/AI idea register](https://github.com/Bigbadboybob/predictionTrading/blob/af7e3f732/agent_docs/swarm-focused-design/IDEAS.md) | Swarm-Gardener | predictionTrading `af7e3f732` | Authorized research sequence; [personal CM review](../reviews/2026-09-10-predictiontrading-research-review.md) |
| [SEJD coordinator feedback](https://github.com/Bigbadboybob/predictionTrading/blob/af7e3f732/agent_docs/swarm-focused-design/SEJD-FEEDBACK-2026-09-10.md) | Swarm-Gardener, attributing SEJD-Coordinator | predictionTrading `af7e3f732`; source message and working-copy limits recorded in note | Reviewed case-study input; no independent audit of live SEJD |
| [Research launch and RAG notes](https://github.com/Bigbadboybob/predictionTrading/blob/cd7559d36/agent_docs/swarm-focused-design/NOTES.md) | Swarm-Gardener | predictionTrading `cd7559d36` | Fable runtime confirmed by coordinator; RAG source findings distinguished from deployed behavior and measured coverage |
| [Independent Fable brainstorm](https://github.com/Bigbadboybob/predictionTrading/blob/e65612f74964d8bb3674653ebc4ee42621e78d6b/agent_docs/swarm-focused-design/BRAINSTORM-FABLE.md) | Fable-Scout; integrated by Swarm-Gardener | Raw author commit `e65612f74964d8bb3674653ebc4ee42621e78d6b` | Preserve raw provenance; use subsequent source corrections and synthesis before relying on its claims |
| [Refined Owner/AI ideas](https://github.com/Bigbadboybob/predictionTrading/blob/a83f8ddc3/agent_docs/swarm-focused-design/IDEAS.md), [two list reviews](https://github.com/Bigbadboybob/predictionTrading/blob/a83f8ddc3/agent_docs/swarm-focused-design/LIST-REVIEW.md), and [source corrections](https://github.com/Bigbadboybob/predictionTrading/blob/a83f8ddc3/agent_docs/swarm-focused-design/FINDINGS.md) | Swarm-Gardener and independent reviewers | predictionTrading `a83f8ddc3`; read by Initiative-Bridge | Eight proposal-document topics; Owner ideas preserved; source findings do not establish live impact |
| [Proposal task roster](https://github.com/Bigbadboybob/predictionTrading/blob/545c96a6f/agent_docs/swarm-focused-design/LANES.md) | Swarm-Gardener | predictionTrading `545c96a6f` snapshot, personally read; live roster on its hub branch | P1/P2/P6 revising; P3/P4/P5 drafting; exact tasks, sessions, owned paths, and launch groups; later personal reviews linked below |
| [P6 source delivery reliability proposal](https://github.com/Bigbadboybob/predictionTrading/blob/0cf7013da/prediction/ingest/docs/swarm-source-reliability/PROPOSAL.md) | P6-Source-Scout; Swarm-Gardener coordinates | predictionTrading `0cf7013da`; proposal and notes personally read | [CM review: revise before Owner selection](../reviews/2026-09-10-p6-source-reliability-review.md); useful recovery metric, release matching, authority, evidence, and shared boundaries need correction |
| [P1 momentum coverage proposal](https://github.com/Bigbadboybob/predictionTrading/blob/e426f6885/applications/scraperGeneration/docs/swarm-coverage/PROPOSAL.md) | Coverage-Scout; Swarm-Gardener coordinates | predictionTrading `e426f6885`; proposal and notes personally read | [CM review: revise before Owner selection](../reviews/2026-09-10-p1-momentum-coverage-review.md); bias claims, comparable evaluation, historical availability, authority, and shared boundaries need correction |
| [P2 article follow-through proposal](https://github.com/Bigbadboybob/predictionTrading/blob/e9764a371/prediction/application/alphamodels/docs/swarm-follow-through/PROPOSAL.md) | Followthrough-Scout; Swarm-Gardener coordinates | predictionTrading initial `f16a13d1d` and revision `e9764a371`; proposal and notes personally read | [CM review: further revision before Owner selection](../reviews/2026-09-10-p2-article-follow-through-review.md); scope and major evaluation claims corrected, with remaining economic, recovery, historical-evidence, and authority requests |
| [Charter review record](../reviews/2026-09-10-charter-review.md) | Initiative-Bridge | CM hub branch; bootstrap commit identified in the pinned kickoff | Owner decisions and review provenance |
| [predictionTrading onboarding](https://github.com/Bigbadboybob/predictionTrading/blob/ae3724a69/agent_docs/swarm-focused-design-onboarding.md) | Swarm-Gardener | predictionTrading, commit `ae3724a69` | Verified local entry point to the shared docs, project instructions, and channels |
| [predictionTrading contribution and candidate register](https://github.com/Bigbadboybob/predictionTrading/blob/acfd6f8ce/agent_docs/swarm-focused-design-charter-review-2026-09-10.md) | Swarm-Gardener | predictionTrading, commit `acfd6f8ce` | Reviewed input and later candidate ideas; canonical goal is the approved shared charter |
| [SEJD historical evidence and lessons](https://github.com/Bigbadboybob/predictionTrading/blob/acfd6f8ce/agent_docs/swarm-focused-design-charter-review-2026-09-10.md#sejd-evidence-and-lessons) | Swarm-Gardener | predictionTrading contribution at `acfd6f8ce`, with dated source records | Historical case-study starting points; not current defect findings |

predictionTrading candidate documents will be indexed here as they arrive.
Initiative-Bridge personally reviews each
candidate and links review comments alongside it.

No new production swarm results exist yet. Add artifact directories as approved work needs
them; put repository-local evidence with its code when appropriate and link a
committed reference here. Include owner, purpose, source revision, and status.
When an artifact is superseded or its evidence invalidated, record that change
and link its replacement so dependent work can reassess it.
