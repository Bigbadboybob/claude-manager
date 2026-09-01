# Worktree reaper

`scripts/worktree_reaper.py` removes old claude-manager and Claude-native checkout directories while preserving Git branches and ignored outputs. It is dry-run by default. The daily policy is seven days for both task-owned and unowned checkouts, with an apply-time mass-event cap of 100 removals. Eligible candidates are measured and processed largest-first.

## Eligibility

A task-owned checkout is eligible only when all of the following are true:

- every task associated through a workspace binding or the task's `wip_branch` is `done` or `archived`;
- at least seven days have elapsed since the newest checkout-specific session input, Claude transcript, reflog event, tracked/untracked changed-path mtime, or meaningful ignored-output mtime;
- no live daemon session or local process references the checkout;
- the workspace is neither pinned nor continuous;
- the planning API and daemon session inventory are both available;
- Git reports no merge conflicts or unsafe oversized/secret-like untracked files.

Task `updated_at`, task closure/report timestamps, the checkout root mtime, the `.git` pointer mtime, and the inherited HEAD commit timestamp do not count as activity. Inventory jobs, bulk task updates, and a recent trunk commit therefore cannot reset hundreds of retention clocks.

An unowned checkout—one with neither an exact workspace binding nor a repository-scoped `wip_branch` match—uses the same seven-day policy and hard runtime gates. Its existing branch is retained, safe dirty changes receive a WIP commit, and detached HEADs receive a rescue branch. Whether its commits are represented in `origin/main` remains audit information, not an eligibility gate, because deleting a worktree never deletes its branch.

Claude-native children are discovered from `~/.claude/projects/*/*/subagents/*.meta.json`. `worktreePath` identifies the child and the containing encoded project directory identifies its manager parent. An exactly linked child inherits the parent's task ids, pin/continuous state, and terminal-task requirement. The child's own transcript and Git state drive its activity clock; a live process or daemon session protects only the checkout it actually references. Metadata-discovered native roots are scanned automatically, and `--native-root` adds explicit roots for custom/workflow worktrees.

Exact workspace/task bindings are authoritative. The fallback `wip_branch` association is accepted only when both the branch name and canonical repository URL match; branch names alone are not globally unique across projects.

Ordinary tracked and untracked changes are staged and committed on the existing branch immediately before removal. A detached checkout first receives a `cm-reaper/rescue-...` branch. Branch refs are never deleted. Empty ignored directories, external provisioning symlinks, caches, logs, scratch trees, and canonical secret copies are discarded with the checkout. Other ignored payloads—including divergent secret copies and analysis outputs—move atomically to `~/.cm/worktree-artifacts/` before removal; the mode-700 vault records task/branch/HEAD provenance and a per-file SHA-256 manifest. Unpinned archives expire after 30 days by default; place `.keep` inside an ignored-output archive (or `<bundle>.keep` beside a standalone bundle) to retain it. Every candidate is fully re-evaluated after acquiring the host lock, after any preservation commit, and after artifact archival.

A standalone Git clone accidentally created beneath the managed worktree root is eligible under the same policy. Before removal, all repository refs are captured in a verified Git bundle under the artifact vault. This is distinct from an ordinary registered worktree, whose refs already live in the primary repository.

## Commands and recovery

```bash
# Full dry run
python3 scripts/worktree_reaper.py

# Compact dry run
python3 scripts/worktree_reaper.py --summary-only

# Include an explicit native root in addition to metadata discovery
python3 scripts/worktree_reaper.py --native-root ~/code/projects/predictionTrading/.claude/worktrees --summary-only

# Apply at most the default 100 removals, largest first
python3 scripts/worktree_reaper.py --apply --summary-only
```

Apply records are appended to `~/.cm/worktree-reaper.jsonl`. To resume work after a checkout is removed, start the task again through claude-manager; the existing worktree self-heal reattaches the preserved task branch. Manual recovery is also ordinary Git: `git worktree add <path> <preserved-branch>`. Ignored outputs are recoverable from the ledger's `artifact_path`; standalone clones are recoverable from `standalone_bundle`.

The deployed `cm-worktree-reaper.timer` runs daily between 12:55 and 13:05 UTC, ahead of the 13:15 UTC disk alert. A host without passwordless system-unit installation can run the same command from the user's crontab at 12:55 UTC. Scheduled runs fetch each candidate repository's origin once so the audit ledger can record landed status; a failed fetch is reported but does not endanger branch preservation.

## Installation

Install the script and system units on each host, then enable the timer:

```bash
sudo install -D -m 0755 scripts/worktree_reaper.py /home/lucas/.local/libexec/cm-worktree-reaper && sudo install -m 0644 deploy/cm-worktree-reaper.service deploy/cm-worktree-reaper.timer /etc/systemd/system/ && sudo systemctl daemon-reload && sudo systemctl enable --now cm-worktree-reaper.timer
```

User-cron fallback (install the script without `sudo`, preserve the existing crontab, then add this line once):

```text
55 12 * * * /usr/bin/python3 /home/lucas/.local/libexec/cm-worktree-reaper --apply --summary-only 2>&1 | /usr/bin/logger -t cm-worktree-reaper
```
