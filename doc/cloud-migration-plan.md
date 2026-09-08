# Local-to-cloud migration plan — 2026-09-08

Status: the controlled cutover succeeded on September 8. All 26 original CM
sessions (9 Codex, 7 Claude, 10 Bash) now run on `cm-sessions`; their conversation
IDs, workspaces, labels and permissions were preserved. The laptop daemon owns
no sessions, and fresh work defaults to `sessions`. Software is merged, pushed
and installed. The cloud development database is restored and verified.

The migration controller subsequently completed its same-conversation handoff
from the standalone laptop terminal into cloud CM, bringing the verified total
to 27 sessions (10 Codex, 7 Claude, 10 Bash). The source writer was released;
the final history, workspace and YOLO mode were preserved. See
`~/.cm/migrations/cloud-20260908/controller-handoff/complete.json`. Do not repeat
the historical controller stop/resume instructions below.

The ten-minute stability observation passed. A post-cutover shell continued
through a deliberately disconnected TUI tunnel; reconnect and a full TUI
close/reopen accepted input with all 26 session processes and both editors
unchanged. All 134 archive/code copies are checksum-verified, including three
ordinary directories whose inherited Git context required separate verification.
All 28 pre-existing archive paths are reconciled without overwriting cloud
files, 3,845 non-omitted symlinks resolve, and seven additional native Python
environments match the source. The migration is complete. Live cloud work must
never be overwritten by the old source.

Owner amended the data policy during execution: keep large research data and
other bulky workspace files on the laptop. The exact omission manifest and
`cm-fetch-local` helper let agents fetch only what they need. No source data was
deleted. See [the execution host runbook](cloud-session-host.md).

The Codex pool is cloud-owned; its old local database must never be copied over
the live pool. Eight ongoing orchestrators on `cm-manager` now subscribe to
`#orchestrators`, with their schedules and sessions preserved. The remaining
sections record the approved plan and the historical pre-cutover inventory.

## Requested outcome

Reopening the TUI shows the same tasks, workspaces, session labels, conversation
histories and continuous-task layout. Existing local work runs on `cm-sessions`;
new work defaults there. Every applicable launch dialog exposes an explicit local
option. Bash and Neovim work against cloud files. Closing the client or losing
laptop connectivity does not stop the cloud processes. Session push/pull is
retired; future exceptional host migrations are operator/agent-managed work.

Keep the existing resource-management approach. No PRs: test on the working
branch, merge to main and push. No delegation/subagents are authorized for this
work. Owner requested the plan before execution starts.

## Historical starting point

- 27 local CM sessions: prior inventory classified 17 agents and 10 shells,
  across 14 active worktrees. Refresh engine/transcript/process inventory before
  cutover. The sole active agent is the migration coordinator.
- All 13 configured continuous tasks already reside on `cm-manager`; none are
  local. Preserve their schedules, paused flags, current sessions and column.
  The existing manager also coordinates the shared message board.
- Approximately 73.26 GiB in active worktrees; approximately 376 GiB across the
  previously selected full worktree/repository/agent-state source. Include other
  local workspaces, dirty and ignored files, archived histories and external
  symlink targets. These numbers are disk use, not measured transfer bytes.
- Cleanup inventory found 114 worktree directories: the 14 active ones account
  for 73.26 GiB, and the other 100 for 205.56 GiB. An initial Git/process-CWD
  audit identified 45 inactive checkouts totaling 65.85 GiB with clean tracked
  and untracked state, HEAD contained in `origin/main`, and no process CWD under
  the checkout. These are candidates, not an approved deletion list or guaranteed
  recoverable space: task state, pins, open-file/symlink references, and ignored
  data still need review. The largest candidate, `s1-single-market-jd-kf-causal-and-evals`,
  is 23.41 GiB, including 22.61 GiB under `analysis`. The 17.30-GiB `aa074c18-microscope`
  checkout has six tracked changes and 300 untracked entries; the 11.84-GiB
  `sejd-cohort415-overnight-20260907` has an unpublished commit. Preserve those
  changes and data. No cleanup was performed during planning.
- The local `predictiondb` is approximately 16 GB and is not a physical replica.
  It must be migrated consistently or replaced only by a verified equivalent.
  The production database reached by `postgres-remote` is approximately 1,735 GB;
  it remains in place. Read-replica access requires separate validation.
- New host: 16 vCPU / 64 GiB, 1 TiB data disk, on demand, tested CM runtime,
  agent authentication, shared messaging and planning API access. No CUD bought.
- Local UID is 1000; cloud `lucas` is UID 1001. Preserve ownership by destination
  account, not blindly by numeric UID, while retaining modes and symlinks.
- Kitty remote control works at `unix:/tmp/kitty-4422`; window 1 runs the TUI.
  This supports terminal-state capture and targeted input/relaunch without
  requiring desktop mouse automation. Capture the existing view before editing.
- Neovim 0.11.2 and its configuration/plugin directories are local. Clipboard
  configuration currently assumes a desktop clipboard (`unnamedplus`). Several
  Neovim processes exist; inspect their roles and unsaved buffers before retiring
  any shell/editor process.

## Execution sequence

1. **Move the migration controller outside CM.** After the planning turn ends,
   Owner stops this specific CM session from a separate local Kitty terminal,
   then resumes its saved conversation there. Closing the TUI is insufficient:
   the CM-owned Codex backend keeps the conversation's active-writer lock even
   while idle. The attempted resume-before-stop order failed with `already has
   an active writer`; do not repeat it or delete lock files. Avoid concurrently
   running two writers for the same conversation. The standalone controller has source-file
   access and can close/relaunch CM without killing itself. Keep transfer jobs
   supervised independently of the TUI and inhibit laptop sleep during copying.

2. **Capture recovery state and a complete dependency inventory.** Save manifests,
   task/workspace/session IDs, current conversation bindings, queued input,
   process trees, shell working directories, workflow/continuous state, sidebar
   order/sections/pins/collapse state, screenshots or terminal captures, and
   messaging identity/watch state. Identify shell jobs and unsaved editor buffers.
   Inventory cloud projects, SSH aliases, DB endpoints, local services, MCP
   wrappers, OAuth/API credentials, environment files and tool versions privately.

3. **Reduce the payload, then copy the environment.** Execution amendment:
   retain large data locally, per Owner’s later instruction. Individual workspace
   files >=10 MiB and semantic bulk-data directories >=100 MiB are omitted;
   Git history, agent state, credentials and installed tools are exempt. Build
   caches are excluded and native environments rebuilt. Preserve source files
   and use the recorded manifest for deliberate, selective retrieval.
   The original cleanup audit follows: first audit inactive
   workspaces by disk use, active process/session references, task state, pinned
   state, Git changes, unpublished commits, and ignored/untracked files. A clean
   Git status alone does not make a checkout disposable: ignored datasets and
   experiment outputs may be unique. Identify a concrete reap list with recovery
   evidence. Preserve active/unfinished work and all unique changes/data; retain
   branch/history references and archived workspace metadata for completed work.
   Reap proven-reconstructible, unused checkouts and rebuildable caches during
   execution; do not blanket-delete by age or force-delete dirty worktrees. If
   recoverability is uncertain, keep/copy the data. Stage active workspaces first,
   then all retained workspaces and archives, with resumable transfers and
   preserved Git common
   directories, uncommitted/ignored files, hardlinks and symlinks. Retain canonical
   `/home/lucas` paths. Reuse a verified identical cloud data source where possible.
   Measure throughput before predicting completion. Retain the local copy for
   rollback; do not force WIP commits or use the old ephemeral-worker push button.
   Migrate skills, instructions, Claude/Codex settings and histories, plugin/MCP
   configuration, shell dotfiles/history, Git settings/hooks, project environments,
   helper binaries and editor state. Repair host-bound paths and rebuild native
   dependencies that are incompatible with the new OS.

4. **Make external access equivalent.** Configure destination SSH identities and
   aliases for trader, portal, aux, database hosts and other inventoried instances.
   Prefer a dedicated cloud-host SSH key authorized on the destinations. Preserve
   useful gcloud/Git/API authentication and verify cross-project actions needed
   for existing Spot workflows. Test SSH commands, readable paths and actual
   authenticated database queries from the new host, including production and
   read replica. Make required tunnels persistent with restart-on-failure. Migrate
   the 16-GB development DB using a consistent database backup/restore and matching
   PostgreSQL/extensions. Preserve its existing role as the development database;
   never redirect development writes to production to make a test pass. Inventory
   other localhost services and distinguish migration requirements from desktop
   integrations such as Anki and GUI browser/clipboard features.

5. **Make the TUI and editor match the requested experience.** Default fresh
   work to `sessions` and expose an explicit Host choice including `local` in
   every applicable launch dialog: new task/workspace, planning-board launch,
   bare agent or Bash, add-session and workflow launch. Eliminate hardcoded local
   defaults and local fallbacks when cloud is unavailable. Existing-workspace
   sessions/restarts normally inherit that workspace's host, preserving its files
   and one-host-per-workspace invariant. An explicit different-host selection must
   use a separate destination workspace and disclose that existing running work
   is not being migrated. Include headless/API launch defaults where applicable.
   Preserve the existing layout. Remove the magenta remote-host tag/extra host
   presentation from normal
   task rows. Use a subtle inline local marker (proposed: dim `⌂`) only for local
   tasks, without adding a row. Keep exact host information in the detail view.
   Preserve the continuous column, grouping, order and collapsed state. Audit all
   launch/restart paths, especially Bash, empty workspaces, in-place work and
   Neovim actions that might still access local paths. Remove the legacy session
   push/pull actions, shortcuts, help entries and obsolete transfer-specific
   backend/event paths. The current shortcuts are `Alt+9` / `Alt+0`; `Alt+p`
   (search/project selection) and `Alt+l` (navigation/planning launch) now have
   other uses and must remain functional. Retain ordinary Git push/pull, Spot
   instance/backtest workflows, and persistence/resume helpers shared with other
   paths. Do not add a replacement session-transfer UI.
   Install the same Neovim
   version/config/plugins and language tools on the VM; configure terminal
   clipboard integration and verify editing/saving files in the cloud. Preserve
   unsaved buffers as recoverable data before any editor handoff.

6. **Perform one controlled cutover.** Finish destination preparation and targeted
   tests before stopping source sessions. Stop source execution at the boundary,
   take a final file/transcript/DB delta, and resume each conversation exactly once
   on the destination. Recreate shell sessions in their prior working directories
   and explicitly handle shell-owned jobs; process memory is not live-migratable.
   Preserve task/workspace IDs, labels, parent relationships and conversation
   binding. Perform an explicit messaging identity handoff, with the source fenced
   before activating those identities on the destination, so names/DMs/watches do
   not silently become new participants. Do not run cloned daemon identities
   concurrently. Preserve the existing manager's continuous tasks in place.
   Update TUI host metadata while the client is stopped so it cannot overwrite
   the migration or resurrect local sessions. Resume the TUI against the new state.

7. **Prove the working experience.** Check every migrated session's intended
   conversation and worktree; compare file/manifest inventories and verify dirty
   changes. Verify cloud defaults and explicit local choices across launch paths,
   including the currently hardcoded-local planning launch. Check retired transfer
   shortcuts/help are gone and unrelated navigation/Git/Spot features still work.
   Test new agent/shell creation, restart/resume, Neovim edit/save and
   clipboard, MCP tools, database connections, SSH access, skills, the message
   board and continuous-task rendering. Use isolated Rust tests/private build
   targets, rendered terminal fixtures and actual Kitty/TUI inspection. Disconnect
   and reconnect the client and verify cloud process identities remain unchanged.
   Use brain-only deployments and the required ten-minute stability check for
   daemon code changes. Merge tested changes directly to main and push.

## Historical pre-migration handoff and timing — completed

These instructions record the initial move out of CM before migration. The
controller now runs in cloud CM; replaying them would interrupt the live session.

Controller conversation ID: `01a07e0e-7821-7ce2-b906-56d5add424fc`.
Working directory: `/home/lucas/.cm/worktrees/claude-manager-cloud-execution-proposals`.
After the current reply finishes, run the following from that directory in a
separate terminal. The kill targets only this CM session and retains its saved
conversation; allow its owned backend to exit before resuming.

```sh
scripts/cm-op kill_session '{"session_uid":"ts-18d32b77ffec4909-0"}'
sleep 5
codex --yolo resume 01a07e0e-7821-7ce2-b906-56d5add424fc
```

Once resumed, tell the standalone session to begin the migration. Do not restart
the old CM row while the standalone controller owns this conversation.

The one-hour window is a target, not a transfer-time guarantee. Active workspace
readiness takes priority while the complete archive transfer progresses. Report
measured throughput and any remaining copy/verification honestly; do not call
partial data or unverified access complete. The laptop must stay awake and online
until the final required source copy and cutover finish.

Private prior evidence: `~/.cm/migrations/cloud-20260908/`, including current
session inventories, `kitty-layout-before.json`, `all-worktree-sizes.json`,
`inactive-worktree-audit.json` and `largest-inactive-breakdown.json`. Do not commit credential
files or terminal captures. No additional Owner choice is needed for routine
implementation; preserve existing behavior and disclose any concrete exception
that cannot be handled autonomously.
