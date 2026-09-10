# CM coordination feature ideas

Status: discussion draft, 2026-09-10. Owner authorized initial CM research and
brainstorming; these candidates are not an approved implementation backlog.
Prepared for Initiative-Bridge to discuss with Owner and Swarm-Gardener.

The [charter](../CHARTER.md) seeks useful progress on swarm-favorable work with
little supervision. SEJD provides lessons while retaining its existing
coordinator and the close Owner involvement Owner wants. The ideas below target
avoidable coordination work; they do not treat desired domain collaboration as
a failure.

Repository evidence was inspected at CM commit
`849058f466e13ed5a83ad6d818ad056aa03b0fdb`. This is an initial source review, not a
live usability study or a claim that the listed gaps caused current failures.
The predictionTrading investigation and SEJD coordinator's input should refine
or replace these ideas. The order is for discussion, not priority.

## 1. Open an initiative's current working context from its task

**Problem and value:** a new or returning agent, or Owner, should be able to
reach the current goal, plan, decisions, project contacts, and relevant artifacts
without reconstructing the entry point from several chats and worktrees.

**Already available:** initiative records include the coordinator, `docs_path`,
shared channel, and project memberships; `get_initiative` exposes them. Channels
support pins. Task detail currently renders the initiative name and status.
See [initiative models](../../api/models.py), [`get_initiative` and
`chat_pins`](../../mcp_server/server.py), and the initiative detail line in
[planning rendering](../../tui/src/planning.rs).

**Candidate addition:** a task action and corresponding agent read that resolve
the initiative's canonical documents, project-side contact/channel, and artifact
index. Show the reviewed document revision and distinguish it from newer draft
edits. Reuse git-backed documents and the existing initiative record. An
unavailable worktree or reference should produce an explicit missing-source
state; cached text should retain its source revision.

**First validation:** have a participant unfamiliar with one existing task find
its agreed goal, current plan, and the correct project coordinator using this
entry point. Try an outdated checkout and an unavailable worktree as well.
Assess whether it removes repeated explanation, using Owner's feedback.

**Open choices:** which documents belong in the first view; whether this begins
as a better skill-generated index or a TUI/MCP feature; how much document text
to show versus links. A pinned index may already solve much of the problem.

## 2. Review a task's result against its intended outcome

**Problem and value:** a worker can finish its assigned activity while its
result still needs integration, stronger evidence, or correction to meet the
intended goal. A coordinator needs a compact, inspectable handoff.

**Already available:** `report_done` distinguishes a worker's finished report
from an ordinary pause; task completion and worktree cleanup are separate
operations. The API stores structured artifacts with a configurable `kind`,
summary, and partial flag, although the current MCP result reader serves
backtests. See [`report_done`, `mark_subtask_done`, and
`get_backtest_result`](../../mcp_server/server.py),
[artifact models](../../api/models.py), and
[artifact storage](../../sql/013_task_artifacts.sql).

**Candidate addition:** a general handoff attached to an existing task, linking
the intended outcome, result commit, verification/evaluation actually run,
remaining limitations, and integration status. A reviewer records their
assessment alongside the worker's claim. Reuse existing artifact storage where
appropriate; keep large evidence in its repository or artifact store. A
favorable metric alone must not stand in for meeting the intended outcome.

**First validation:** review a completed historical example where isolated
results did not establish the required integrated behavior. Check whether the
handoff makes that limitation apparent and permits a reviewer to trace what
actually ran. Try the format in a document before building UI.

**Open choices:** the smallest useful handoff format; when the coordinator can
accept a result within existing scope and when Owner judgment is needed; whether
an artifact panel is enough without adding task statuses. Domain evaluation
rules remain with predictionTrading.

## 3. Show which results need review when an input changes

**Problem and value:** an improved baseline, changed specification, or corrected
preprocessing step can leave dependent results apparently complete but no longer
applicable. The charter's historical SEJD evidence makes this a concrete question
to investigate.

**Already available:** tasks have `depends` references, including project-qualified
references in planning. The TUI displays dependencies and checks ordering
conflicts. Artifact rows store summaries and partial results. The current
[protocol](../PROTOCOL.md) requires coordinators to identify affected artifacts
when their inputs change. See `TaskCreate.depends` in
[models](../../api/models.py), `recompute_conflicts` in
[planning](../../tui/src/planning.rs), and
[artifact storage](../../sql/013_task_artifacts.sql).

**Candidate addition:** allow a result to declare the specific specification,
baseline, dataset/configuration, or upstream artifact revision it consumed.
When a declared input is superseded, expose the directly affected results and
tasks as needing review, with the old and new references. The coordinator
decides whether the change invalidates evidence and what needs rerunning.

**First validation:** reconstruct one documented baseline change, record its
actual consumers, and check that the affected results are discoverable without
flagging unrelated work. Test revision replacement separately from a harmless
document edit. Begin with a small explicit dependency index.

**Open choices:** which input types cause enough real trouble to support first;
how to record stable identities across codebases; who declares a replacement;
how much manual upkeep this introduces. Task ordering dependencies alone do not
describe the validity of evidence, but a full automatic dependency graph may be
unnecessary.

## 4. Keep concrete Owner decisions beside the plan

**Problem and value:** Owner should be able to see what decision is being asked
for, the proposal it applies to, and what changed since their last review. This
also helps agents preserve the difference between exploratory ideas and approved
work as brainstorming evolves.

**Already available:** initiatives have lifecycle/membership approval records
and events. Plans and chat carry richer decisions; task metadata can hold links.
See [initiative schema](../../sql/015_initiatives.sql),
[`update_initiative`](../../dispatch/db.py), and the
[protocol's Owner checkpoints](../PROTOCOL.md).

**Candidate addition:** expose linked decision items in initiative/task detail:
the concrete request, options or tradeoffs, relevant proposal revision, and the
recorded Owner response with its scope. Start with entries in the existing plan;
consider a compact view only if locating these decisions remains difficult.
This supports interactive planning and the existing launch/scope checkpoints.

**First validation:** replay a planning exchange containing an inline correction
and a later approval. Verify that a reader can identify the exact agreed scope
and distinguish it from the revised or still-unselected ideas. Ask Owner whether
the view helps their normal planning conversation.

**Open choices:** which decisions deserve explicit entries; how to link inline
comments and conversational approval without requiring duplicate Owner input;
whether the existing plan is sufficient. No new review cadence, scoring system,
or blanket requirement for extra approvals is proposed.

## Next discussion

Use predictionTrading's candidate documents and SEJD coordinator's observations
to identify a concrete example for any promising idea. Initiative-Bridge can
review those documents for coordination needs, evaluation clarity, and where a
practice change may suffice. Owner can add directions, reject ideas, or choose
one for deeper design; none of these candidates requires launching a new swarm
to begin that conversation.
