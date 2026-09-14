# Continuous orchestrator instructions: review and consolidation plan

Review date 2026-09-14, over the eleven live task definitions on cm-manager
(the three paused Codex-migration canaries are fixtures and were skipped).
Inputs: the standing texts captured by `scripts/migrate_standing_instructions.py`
(audit `~/.cm/audits/standing-instructions-20260914-171809`) and the deployed
policy `doc/continuous-review-routing.md`. Three independent reviewers covered
the scraper builders (scraper-creation, structured-scraper-creation,
scraper-opt), the triage lanes (scraper-triage, bug-triage, perf-triage,
health-alert-triage) and the rest (api-update, behavior-triage, code-bughunt,
momentum-detective) against one rubric: contradictions with the 2026-09-14
channel contract, redundancy, fit for file-delivered standing instructions,
missing pieces, structure, and outright hazards.

Headline: the lane-specific content is sound and is only about a third of each
file. The rest is a shared skeleton that was copied eleven times and has
drifted; the contradictions with the new contract live in that skeleton, so the
fix is structural, not eleven edits.

## 1. Systemic findings (every task)

1. **No wake-type dispatch.** Every body opens with "Run ONE scan/batch/review
   cycle now:" and ends with a mandatory `report_done`. A channel mention is not
   a fire, but nothing after the header says so. Read literally, a worker
   handoff re-syncs main, re-scans (consumer lanes re-read a consumed queue
   batch) and calls `report_done` on a run that is not open.
2. **The ready-for-approval bar exists only in the header.** Every lane's
   terminal step is still "LEAVE it blocked; the USER merges via
   /triage-review". No rebase at promotion, no gate receipts posted in the
   channel, no in-lane review verdict, no cross-lane collision check, no
   `owner_review` stage, no approval packet. The stale-base check states that a
   branch merely behind main "rebases cleanly at merge" and proceeds.
3. **Worker briefs predate the channel.** All spawn templates end in "paste
   the output into your final message" and "set status blocked when
   committed". None names the channel, the mention target, a codename,
   per-step posting or exit criteria, contradicting the header's own
   requirement to include the worker contract in every brief.
4. **Two stage vocabularies, never mapped.** Lanes write
   `detected/root_caused/proposed/fix_ready/in_review` (or `SC-`/`SP-`/`OPT-`
   ledger stages) into their private index and never write
   `metadata.continuous_stage`, `stage_updated_at` or `next_action`; the TUI
   therefore cannot color these lanes. Three of four triage lanes set planning
   `blocked` for "committed fix awaiting review", which the contract keeps
   `running`.
5. **`send_input` is still the default feedback verb** (up to seven sites per
   triage lane) and worker identity comes from `list_sessions`, not
   `chat_open().continuous` or `chat_people`.
6. **Contradicting fleet budgets.** perf-triage and health-alert-triage forbid
   directory sweeps; scraper-triage, bug-triage, api-update and behavior-triage
   mandate `uv run pytest test/ -m 'not slow'` plus repo-wide mypy in every
   worker brief on the same 16 GB host. Two builder lanes each claim "3 live
   workers fleet-wide" while counting different label prefixes; the triage
   lanes cap spawns per cycle but never concurrent workers (only health-alert
   does).
7. **Cross-lane handoffs go nowhere.** "Note it for bug-triage" is written to a
   gitignored local cycle log the other lane never reads. No lane posts into
   another lane's channel.
8. **One-time text frozen as permanent instruction.** The "Codex runtime and
   migration handover" block (byte-identical in ten files), "Read
   HANDOVER_CODEX.md before every scan", "On your very first cycle" (with a
   non-idempotent `echo >>`), paste/apply-convention preambles and dated
   changelogs in the builder lanes, incident forensics (the Aug 6/12 outage
   story appears three times), and hard-coded snapshots ("adopt
   PERF-066/072/083 first", "retain 914e0fc2 … BUG-079 belongs elsewhere").
9. **Smaller but real.** `mcp_start_session` in eight files that also say
   "use advertised tool names"; `report_done` demanded five to nine times per
   file; session hygiene stated three different ways per file; `--ff-only`
   asserted as "always a clean fast-forward" against the worktree policy;
   `notify_until="final"` (the parameter is `until`); mixed actors ("the
   operator", "the USER", "Owner", "the reviewer").

## 2. Lane-specific hazards (fix regardless of the consolidation)

| Lane | Finding | Fix |
|---|---|---|
| code-bughunt | LOW-risk auto-fix pushes to `origin/main` with no review verdict or collision check; its gate runs in the wrong directory (`$SW` inside single quotes never expands); unbounded push retry; scratch worktrees under `/tmp` never pruned; Claude-only `/bug-hunt` + Agent-tool fan-out mandated for a Codex runtime | Route LOW fixes through the same promotion packet or have Owner re-authorize the auto-merge lane explicitly; quote the variable; cap retries at 2; prune at Step 0; state the Codex equivalent |
| bug-triage | Orchestrator told to send Telegram alerts (`cm-notify`) for blockers, justified by a July incident the channel now covers; stranded WIP punted to Owner to commit | Post the blocker in `ct/bug-triage` at `owner_review`/`waiting`; re-spawn into the same worktree to finish WIP |
| perf-triage | Introduces a third Owner-attention mechanism (`metadata.operator_question`) and references `cm-notify`, which only bug-triage defines; `source_down` copied from the scraper lane; hard-coded PERF-083 example | Fold into `owner_review` + channel post, or document it as a display mirror; delete the copies |
| scraper-opt | On conflict "resolve in favor of origin/main" silently drops adopted edits; ledger lives inside the worktree that was reaped once already; `ILIKE '%source%'` scans the sibling lanes forbid; adoption merges live into a hot-reloaded checkout with no rebase, gates or rollback line; drifted duplicate never-remove lists | Backup branch + channel post before either side; `~/.cm/continuous-tasks/<task>/memory` symlink; indexed queries; the readiness bar or an explicit waiver list; single source for the rules |
| scraper-creation / structured | Both suppress `notify_user` for hard calls entirely (structured), so a genuine Owner decision has no path; "paste-ready default_prompt / --apply is required" preambles now false; `source .env` for read-only queries; audit rows keyed on a session UID | Hard calls → `owner_review` + channel packet; delete preambles; scoped DSN; key on task id |
| behavior-triage (paused) | Port 5433 called both "read-only" and "the write DSN"; workers given three contradictory `blocked` rules; five-level-escaped `ssh … psql` command in a prompt; no paused-state behavior | Name both DSNs once; workers never set `blocked`; wrap the query in a script; "while paused, answer mentions and reconcile only" |
| api-update | Full-suite pytest in every brief; every artifact routed to repo-root `NOTES.md` | Targeted tests; `agent_docs/APIU-NNN.md` |
| momentum-detective | Helper sessions get no channel contract; "kill ALWAYS" contradicts settle-first; sequential 25-minute items against a one-hour window; `blocked_checkout` where the policy says recover; the batch-seq "named in the delivered prompt" is gone | Append the worker line to the rendered brief; verify → final turn → kill; per-cycle cap; ff-first then integrate; read seq from `get_continuous_context` |
| health-alert-triage | No drain/run-identity handling at all; snapshot of task ids frozen as instruction; deploy runbook inline; test budget stated four times | Add the shared run-identity paragraph; move the exceptions to the watchlist; cite `PORTAL-DEPLOY.md`; state once. Its sync paragraph is the only policy-compliant one and should become the template's |

## 3. Shared-skeleton measurement

Exactly identical lines across scraper/bug/perf-triage: about 7.5 KB per file
(header, Codex block, stage table, Step 0, Step 2 preambles, the kill_session
paragraph, CLOSE THE RUN, Summary). Near-identical after substituting the lane
token: 12 to 13 KB per file. That is 48 percent of scraper-triage, 62 percent
of bug-triage and 44 percent of perf-triage; normalized similarity
scraper↔perf 59 percent, scraper↔bug 52 percent, bug↔perf 50 percent.
health-alert-triage is the outlier at 17 to 20 percent (prose, and missing the
drain block). Genuinely per-lane content is 8 to 11 KB per file: lane boundary
and GATE, signal gather, schema fields, monitoring rule, priority rule.

## 4. Plan

1. **Shared orchestrator contract, rendered by CM.** Extend
   `continuous::instructions::render` to emit a shared section (sourced from
   the deployed policy plus a short contract block) before the lane text:
   wake types (fire / mention / operator disposition and what each may do),
   the stage mapping table to `metadata.continuous_stage` and planning status,
   the promotion procedure with the seven readiness items and the one approval
   packet, the worker-brief block (channel, codename, mention target, per-step
   posting, exit criteria, `report_done`), the fleet test budget, the sync
   rule (fast-forward first, then preserve and integrate), concurrent-worker
   cap, cross-lane routing ("post in `ct/<other>` mentioning that
   orchestrator"), and close-the-run in one sentence. Written once, deployed
   with the policy, never copied.
2. **Cut each lane file to its per-lane content** (8 to 11 KB): lane and GATE,
   signal gather as one invocation line, lifecycle mapping, monitoring rule,
   priority rule, lane-specific hazards. Section order: Lane and GATE →
   Signals → Lifecycle and stage mapping → Promotion → Lane monitoring →
   Summary; everything else inherited from the shared section.
3. **Move stable reference material to repo docs and skills** the lanes
   already cite: YAML index schemas, gather heredocs and sampling recipes,
   deploy runbooks, prompt-rendering mechanics, `operator_question`
   mechanics, incident history.
4. **Fix the lane hazards in section 2 in the same pass.**
5. **Rollout**: shared contract first, then scraper-triage, bug-triage and
   perf-triage (highest traffic) so real cycles validate the result before the
   builder lanes, api-update, code-bughunt, momentum-detective and the paused
   tasks follow. Each step goes through `continuous.update` with the audit
   backup the migration script already produces, so any lane can be reverted
   independently.

Estimated effort: about one day across CM (renderer and policy) and the
predictionTrading prompt files.
