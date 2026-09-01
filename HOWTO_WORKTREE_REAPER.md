# Worktree reaper

`scripts/worktree_reaper.py` removes old claude-manager checkout directories while preserving Git branches. It is dry-run by default. The installed daily service uses a seven-day retention window and an apply-time mass-event cap of 25 removals.

## Eligibility

A checkout is eligible only when all of the following are true:

- every task associated through a workspace binding or the task's `wip_branch` is `done` or `archived`;
- at least seven days have elapsed since the newest task update, session/manifest event, Claude transcript, checkout/reflog/HEAD activity, or tracked/untracked changed-path mtime;
- no live daemon session or local process references the checkout;
- the workspace is neither pinned nor continuous;
- the planning API and daemon session inventory are both available;
- Git reports no merge conflicts, secret-like or oversized untracked files, or meaningful ignored artifacts.

Exact workspace/task bindings are authoritative. The fallback `wip_branch` association is accepted only when both the branch name and canonical repository URL match; branch names alone are not globally unique across projects.

Ordinary tracked and untracked changes are staged and committed on the existing branch immediately before removal. A detached checkout first receives a `cm-reaper/rescue-...` branch. Branch refs are never deleted. Canonical provisioning symlinks such as a worktree `.env` pointing back to the primary checkout are safe because removal deletes only the symlink; a real ignored dataset, log tree, or credential file blocks removal. Every candidate is fully re-evaluated after acquiring the host lock and again after any preservation commit.

The script computes whether the checkout's commits are already represented in `origin/main` for audit information, but that is not an eligibility requirement. Task terminality is the owner's completion signal, and the preserved branch is the recovery path for unmerged work.

Worktrees with no task association fail closed as `unowned_worktree`. This deliberately leaves true orphans for an explicit later policy decision instead of guessing that an absent binding means finished work.

## Commands and recovery

```bash
# Full dry run
python3 scripts/worktree_reaper.py

# Compact dry run
python3 scripts/worktree_reaper.py --summary-only

# Apply at most the default 25 removals
python3 scripts/worktree_reaper.py --apply --summary-only
```

Apply records are appended to `~/.cm/worktree-reaper.jsonl`. To resume work after a checkout is removed, start the task again through claude-manager; the existing worktree self-heal reattaches the preserved task branch. Manual recovery is also ordinary Git: `git worktree add <path> <preserved-branch>`.

The deployed `cm-worktree-reaper.timer` runs daily between 12:55 and 13:05 UTC, ahead of the 13:15 UTC disk alert. The service uses `--no-fetch`; this only makes the audit-only landed proof rely on the locally cached `origin/main` ref and does not weaken an eligibility gate.

## Installation

Install the script and system units on each host, then enable the timer:

```bash
sudo install -D -m 0755 scripts/worktree_reaper.py /home/lucas/.local/libexec/cm-worktree-reaper && sudo install -m 0644 deploy/cm-worktree-reaper.service deploy/cm-worktree-reaper.timer /etc/systemd/system/ && sudo systemctl daemon-reload && sudo systemctl enable --now cm-worktree-reaper.timer
```
