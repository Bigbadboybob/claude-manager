# Persistent cloud session host

Provisioned September 8, 2026. Owner selected a separate 16-vCPU machine and
asked to retain the existing resource-management approach. There is no new
scheduler, compute-request workflow, or CPU/memory policy. Existing sessions
were migrated later that day: all 26 original CM sessions plus the migration
controller now run here, and fresh work defaults to this host. Large workspace
data remains on the laptop under the selective-fetch policy below. The laptop
is a viewer; its connectivity does not control cloud session lifetime.

## Machine and storage

| Item | Configuration |
| --- | --- |
| VM | `cm-sessions`, `claude-manager-prod`, `us-east4-a` |
| Compute | `e2-standard-16`: 16 vCPUs, 64 GiB RAM, on demand |
| OS | Ubuntu 24.04 LTS, x86-64 |
| Address | Reserved `cm-sessions-ip`, `35.245.98.146`; SSH alias `cm-sessions` |
| Boot disk | 30 GiB `pd-balanced` |
| Data disk | `cm-sessions-data`, 1,024 GiB `pd-balanced`, auto-delete disabled |
| Data mount | `/home/lucas`, ext4, UUID entry in `/etc/fstab` |
| Snapshots | `cm-sessions-daily`, daily window starting 08:00 UTC, seven-day retention, US storage |
| Availability | Automatic restart; live migration during supported host maintenance |

The daemon requires the home mount before starting. The startup script has a
completion marker so normal boots do not repeat package installation. The data
disk is independent of the boot disk and can be expanded later; resize the ext4
filesystem after increasing the GCE disk. Scheduled snapshots are crash-consistent
(`guestFlush: false`); application-consistent database backups remain a separate
concern when transferring live application data.

The existing manager retains its API, messaging coordinator and running agents.
Putting new sessions here separates their machine-level failures from the manager.
Sessions on this new VM still share its resources. CM's existing opt-in memory
controls and runaway-child watcher remain available; no new caps were enabled.

## Runtime and access

- `/etc/systemd/system/cm-daemon.service` runs as `lucas`, under systemd, with
  `/opt/cm-daemon/cm-holder --brain /opt/cm-daemon/cm-daemon`.
- The cloud-first daemon release is `0.1.0+7dcae0a`. The installed laptop TUI
  also includes `9eca337` and `c4c062c` for remote catalogs, redraw and clipboard.
- MCP lives at `/opt/cm-daemon/mcp_server`, with its own virtual environment.
  Preflight registered all 56 tools. Workflows are installed alongside it.
- Claude Code 2.1.265 and Codex 0.153.4 match the source at cutover.
  Both login-status checks passed. Rust 1.94.1, Cargo, uv 0.10.12, Google Cloud
  CLI 583.0.0, Git, Python, Node, tmux and the native build dependencies are installed.
- Fresh CM Codex launches choose an installed terminal editor when both `VISUAL`
  and `EDITOR` are unset: Neovim first, then Vim/vi. Explicit editor choices are
  preserved. This is configured in the native launcher because the systemd daemon
  does not source interactive Bash configuration. On `cm-sessions`, the fallback
  is `/usr/local/bin/nvim` with the migrated Neovim config. Already-running Codex
  processes retain their startup environment: save any unsent draft before using
  `Alt+Shift+R` to restart/resume that session. Reopening only the CM TUI does not
  change an agent's environment. Updating the Python launcher file takes effect
  at the next session launch and does not require restarting the daemon.
- Both main repositories and required worktrees retain their original
  `/home/lucas` paths, Git state and small working files. Conversations, skills,
  agent configuration, shell configuration and Neovim plugins/config were copied.
  Both primary Python environments exactly match the source's Python 3.12.3
  and installed package versions (53 CM packages, 276 predictionTrading packages).
  Seven additional workspace environments were also rebuilt to exact parity.
- Git access to both private repositories passed. Google Compute API reads passed
  during setup. The migration then copied the source's required authentication
  and SSH configuration privately. SSH into all 11 running trading instances
  passed; authenticated production and read-replica queries passed through
  persistent tunnels on ports 5433 and 5434. Biglab timed out from both hosts.
- Operator credentials remain distinct between the cloud host and laptop.
  Auth files and pairing tokens are private, mode 0600.
  The daemon log is `/home/lucas/.cm/daemon.log`; holder output
  is available through `journalctl -u cm-daemon`.
- The planning API uses the manager's private DNS address on port 8000. The
  manager's own `localhost:8000` configuration cannot be copied unchanged to a
  different VM. Config reload applied this address without a brain restart;
  an authenticated `list_projects` request through the daemon then passed.
- The Codex pool is cloud-owned on port 2455. The laptop's `cm-codex-lb.service`
  is only a persistent SSH forward to it. Never reseed the live pool using the
  old laptop database. Keep its HTTP response session bridge disabled as configured.
- The original absolute `node_repl` runtime path is provided by
  `/usr/lib/chatgpt/resources/cua_node` pointing to `~/.local/share/cm-node-repl`.
  All nine migrated Codex app servers reloaded MCP configuration successfully,
  exposing the Node REPL tools without changing loaded conversations.
- `cm`, `git-filter-repo`, `pre-commit`, `py-spy`, Bash and Neovim work here.
  Rust tests still require `scripts/cm-test-isolated` and a private target.
  A scoped AppArmor profile permits the bubblewrap runner; its smoke test passed.

The laptop development `predictiondb` was restored into the cloud's local
PostgreSQL instance, not redirected to production. Verification covered 4,406
physical tables, 326 hypertables and 1,312 foreign keys. The source had existing
Timescale metadata corruption: destination-only repair removed metadata for
12 already-missing chunks and cleared one dangling compressed-hypertable pointer.
The source and original dump were preserved. Maintain PostgreSQL's narrow
traversal ACL on `/home/lucas`; copying source home permissions over it can stop
the database from accessing its data directory.

Routine deployments must use the brain-only procedure in
[the holder/brain runbook](../HOWTO_HOLDER_BRAIN_SPLIT.md), substituting
`--ssh cm-sessions`. Do not use `systemctl restart cm-daemon` or the legacy
`cm-redeploy --manager` path for routine updates: those kill session processes.

```sh
scripts/cm-op --ssh cm-sessions daemon.health
scripts/cm-op --ssh cm-sessions messaging.sync '{"action":"status"}'
```

## TUI and messaging

The local `~/.cm/hosts.toml` now includes `sessions`, using the `cm-sessions`
SSH alias and `/home/lucas/.cm/daemon.sock`. The default is now **sessions**.
Relaunch the TUI normally to view the migrated sessions. Launch dialogs expose
an explicit local option; existing workspaces
inherit their owning host. Remote rows use the usual layout, while local rows
have a subtle inline marker. Session push/pull shortcuts are retired.
Relaunching the client does not restart the remote daemon or its agents.

The cutover fenced source execution before transferring its daemon identity
`37db72a8-da6c-444c-b0d0-64daa6dc6fb3` to the cloud. The empty laptop viewer uses
the former bootstrap identity `83bce20a-1f8e-4dd0-82e1-084fca0881c2`.
This preserves session messaging identities, DMs and retained notification state
in shared space `15b7d0a4-ddcc-4f69-9526-3678e3cadc70`. Never run a second copy
of either identity. `cm-manager` remains the coordinator; sync is connected
with zero pending publications and no errors.

Continuous tasks remain on `cm-manager` with the existing layout and schedules.
Eight ongoing orchestrators and Owner are subscribed to `#orchestrators`.

Other project schedules moved from the laptop to `cm-sessions`:

- `nightly-test-suite.timer` retains 09:10 UTC plus up to ten minutes of jitter,
  its existing 20 GiB limit, idle I/O priority and three-hour timeout. It uses
  the dedicated `~/.cache/predictionTrading-nightly-cloud` checkout. The original
  timer stamp was preserved, so migration did not launch a catch-up test run.
- `cm-worktree-reaper-migrated.timer` runs the existing cleanup command daily
  at 12:55 America/Chicago. The laptop cron entry was removed so retained local
  data stays at the paths used by `cm-fetch-local`.
- `prediction-glossary-migrated.timer` preserves glossary pruning at midnight
  America/Chicago on days 1, 8, 15, 22 and 29 of each month (the original cron).

The latter two timers do not catch up missed runs, matching cron behavior.
A stale Perf chat cron was retired: its target session was absent from both
execution hosts and the source cutover inventory, and it was already failing
before migration. Desktop usage notifications, disk alerts and dictation
maintenance remain on the laptop. No cleanup or full test suite was manually
started as part of this schedule handoff.

Host-to-hub sync uses the private VPC route and a dedicated SSH key. The manager's
authorized-key entry forces only the messaging stdio bridge, with SSH forwarding
and interactive shell access disabled. Its host key was pinned from the existing
authenticated manager connection. No test messages were sent to existing agents.

## Validation

Post-cutover validation preserved all 26 original session UIDs: nine Codex,
seven Claude and ten Bash. All 16 agent conversations retained their exact
conversation IDs and permissions. A ten-minute observation kept every original
session live with unchanged brain PID, holder epoch and restart count.

A shell loop completed during deliberate loss of the laptop TUI's SSH tunnel.
The TUI reconnected and accepted shell input. Closing and reopening the TUI
also accepted input with all 26 process identities and both restored editors
unchanged. The two Neovim views reopened their original files with unmodified
buffers; old source swap files were archived intact to prevent stale swap dialogs.

File verification covered 19 required roots and 134 archive roots, with the
recorded data/cache exclusions. The 28 pre-existing archive paths were reconciled
by filling missing files while preserving cloud contents. All 3,845 inventoried,
source-existing, non-omitted symlinks resolve. Three ordinary directories inherited
their parent's Git context on the laptop; separate checksum verification resolved
those initial Git-status false positives. Seven additional native environments
match source Python/package versions and pass dependency checks.

The following measurements record the earlier, disposable setup probe:

A disposable Bash session exercised direct attachment, streamed input/output,
output continuing after viewer disconnect, and a brain-only restart. The restart
advanced the holder epoch exactly once, 1 → 2, with a 1.28-second observed
control-plane gap. The shell retained its PID, kernel start time and holder parent.
All 256 bytes of the pre-restart replay were identical afterward; reattachment
replayed the screen and accepted new input.

The ten-minute stability gate passed: 21 samples over 600 seconds retained
epoch 2, one expected brain restart, a running breaker, healthy MCP and the
same live session count. Removing the disposable session left the destination
daemon and holder empty at that stage. The existing local and manager
brains retained their prior PIDs/epochs and their 27/20 session counts.

A separate laptop-to-host probe used one persistent SSH Unix-socket tunnel and
the real attach stream. Twelve command/output round trips measured a 42.29 ms
median, with a 37.33–249.56 ms range. This small sample excludes tunnel setup,
TUI drawing and model processing; it is not a latency guarantee or a load test.

## Large data and recovery

Owner requested that large research datasets, experiment outputs and other
bulky workspace files stay on the laptop. The recorded policy omits individual
workspace files at least 10 MiB and semantic bulk-data directories at least
100 MiB. Git history, sessions, credentials and installed tools are exempt;
build caches and native environment directories were excluded separately.
About 191 GiB of worktree data, or 247 GiB across selected roots, stayed local.
No source data was deleted. A general notice was posted to `#cm-general`.

On `cm-sessions`:

```sh
cm-fetch-local /home/lucas/path/to/needed-file --dry-run
cm-fetch-local /home/lucas/path/to/needed-file
```

The helper permits only manifest-listed paths or descendants of listed
directories, restores the original absolute cloud path, and preserves existing
cloud files unless `--replace-existing` is explicit. It uses an authenticated,
read-only laptop export through persistent SSH. The laptop must be online for
fetches; cloud sessions remain independent of it. Details and the exact inventory
are in `~/.cm/migrations/cloud-20260908/LOCAL-DATA.md` and
`local-only-data-manifest.json` on both hosts.

Intentionally missing tracked data is marked `skip-worktree` to prevent accidental
mass deletion commits. Clear that bit before editing a fetched tracked file:
`git update-index --no-skip-worktree -- path/to/file`.

The migration controller also completed its handoff into cloud CM, bringing the
verified total to 27 sessions (10 Codex, 7 Claude, 10 Bash). Session
`ts-18d32b77ffec4909-0` retained conversation
`01a07e0e-7821-7ce2-b906-56d5add424fc`, its workspace and YOLO mode after the
source client exited and released its writer. The receipt is
`controller-handoff/complete.json` in the migration evidence directory. Do not
repeat the handoff or resume another writer for this live conversation. An
unrelated desktop Claude session in `~/whisper-typer` remains local.

Recovery evidence lives under `~/.cm/migrations/cloud-20260908/`, especially
`CURRENT-RECOVERY.md` and `session-cutover/`. Do not rerun the seed, final-delta
or cutover scripts against cloud-owned state. Older archive code is copied
through staging and never streamed over active cloud work. Source swap files,
database dumps and recovery state remain available.

## Cost

At the previously quoted us-east4 rates and 730 hours/month: compute is about
$440.75/month and the data disk $112.64/month. A $30 allowance for boot disk,
address and snapshots brings the estimate to **about $585/month additional**.
Actual snapshot use and egress vary; model/API charges and the existing manager
are separate. At those rates, a one-year resource-based CUD would bring this to
about $420/month, or about $340/month for three years. **No commitment was bought.**
Run on demand for a few days before reconsidering sizing and a one-year CUD.

Pricing references: [Compute](https://cloud.google.com/products/compute/pricing/general-purpose)
and [disks](https://cloud.google.com/compute/disks-image-pricing).

Private setup scripts, inventories and verification evidence are under
`~/.cm/migrations/cloud-20260908/` on the source machine; credentials in that
directory must not be committed. Runtime verification also writes
`~/.cm/setup-verification.json` on `cm-sessions`. A copy of the tested runtime
archive, startup script and service unit is retained in
`~/.cm/provisioning/` on its data disk, which is covered by the snapshot schedule.
