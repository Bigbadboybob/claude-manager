# Closing tasks and reclaiming worktrees

In the work view, **Alt+d** opens the completion dialog for the selected task.
**Keep worktrees** is the default. Use Tab or the arrow keys to select
**Reap this task + descendants**, inspect the paths and retention reasons, and
press Enter. Use j/k to scroll the preview or result. Escape cancels before
submission, or closes the report afterward.

**Alt+Shift+w** opens the same choice for closing a workspace. Closing a
workspace does not mark its associated planning tasks done: any nonterminal
task still protects its checkout. For task completion and cleanup, use Alt+d
on that task's header. Plain Alt+w retains its existing close-session behavior.
A workspace containing several tasks requires selecting a specific task before
Alt+d; cleanup never guesses which one to complete.

Task completion previews the owning host and other configured remote hosts,
so CM children created on a different worker host can be included. A remote
task does not include the laptop's retained migration copies. A workspace-only
close stays on its owning host. Unavailable hosts are reported and must return
a preview before recursive cleanup can be selected; Keep remains available.

## What cleanup includes

The preview fixes the set of checkout identities approved for this operation:

- The selected task's checkout and checkouts of its CM descendant tasks.
- Raw Git worktrees with recorded creation links, including nested descendants.
- Claude-native worktrees with exact parent metadata.

New checkouts created after the preview are not silently added. Each checkout
has a UUID stored in its Git administration directory, so deleting a checkout
and reusing its pathname does not transfer an old cleanup approval.

This choice skips the usual seven-day retention delay for these checkouts.
It preserves the existing reaper's other protections: active sessions and
processes, pinned and continuous work, nonterminal tasks, shared ownership,
unsafe files, conflicts, and uncertain Git state. A retained nested checkout
also protects the enclosing checkout. Children inherit task ownership and
pin/continuous protection through their creation ancestry. Cleanup rechecks
ownership, task status and activity before and after preservation and at the
final removal boundary. It does not terminate descendant agents to free space.

Branches are retained. Ordinary uncommitted files are saved in a WIP commit;
meaningful ignored outputs are archived under `~/.cm/worktree-artifacts/`.
If preservation fails, the checkout remains. Generated caches may be discarded
according to the existing reaper policy. A task can finish with some checkouts
retained; the report explains each outcome. This command does not delete
branches or trigger artifact-vault garbage collection.

## Raw Git tracking and limits

The daemon installs a chained `post-checkout` hook for known repositories and
before creating worktrees in new repositories. The wrapper records ordinary
`git worktree add` creation, then runs the previous hook with its original
arguments and exit status. Existing hooks, including predictionTrading's
bootstrap, are preserved as `post-checkout.before-cm-lineage`.

Creation evidence includes the creating Git process's checkout and the inherited
CM session/task identity when available. Branch switches do not reparent a
checkout. Records live under `~/.cm/worktree-lineage/`; ownership evidence
survives removal of an ancestor. The host inventories exact CM bindings,
branch-and-repository task bindings, and Claude metadata to reconcile existing
checkouts. Branch ancestry or a similar name is never enough to assign a raw
checkout to a task.

Git can bypass post-checkout with `--no-checkout`, disabled/replaced hooks, or
creation on an unconfigured host. Unattributed historical worktrees are left
out of recursive cleanup. Hook conflicts are reported; CM does not overwrite
an externally replaced or symlinked hook. After inspecting a missed checkout,
an operator/agent can explicitly register its creation parent on that host:

```bash
python3 ~/.cm/worktree-tools/worktree_lineage.py register /absolute/child /absolute/parent
```

Register only a parent supported by creation evidence. To install the chained
hook in another repository:

```bash
python3 ~/.cm/worktree-tools/worktree_lineage.py install /absolute/repository
```

## Durable jobs and recovery

The operator-only daemon RPC is `worktree.cleanup` with actions `preview`,
`apply`, and `status`. Preview uses a caller-generated UUID `id`, optional
`task_id`, and optional absolute host-local `worktree_path`. Apply and status
use that same `id`; retries are idempotent. Agent session callers cannot invoke
this destructive RPC through MCP. Normal task/session permissions are unchanged.

The TUI waits for every host to acknowledge its persisted cleanup request
before completing the task. If the viewer exits before acknowledgment, it
leaves the task open. Once queued, a detached host worker continues without the
viewer and waits briefly for closure; a failed status update retains a
nonterminal task's checkout. Requests and per-path results are saved after every
step under `~/.cm/worktree-cleanup/<id>.json`, with a sibling `.log`. The daemon
resumes unfinished jobs at startup; a worker lock prevents duplicate execution.
Cleanup shares the scheduled reaper's host lock.

To inspect an existing job from the CM checkout:

```bash
scripts/cm-op --ssh cm-sessions worktree.cleanup '{"action":"status","id":"<job-uuid>"}'
```

The JSON lists removed and retained paths and any artifact recovery location.
The shared removal ledger is `~/.cm/worktree-reaper.jsonl`. A retained checkout
can be reconsidered with a fresh preview after its blocking condition is
resolved. An error does not justify bypassing preservation or deleting an
unknown checkout manually.
