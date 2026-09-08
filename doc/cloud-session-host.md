# Persistent cloud session host

Provisioned September 8, 2026. Owner selected a separate 16-vCPU machine and
asked to retain the existing resource-management approach. There is no new
scheduler, compute-request workflow, or CPU/memory policy. Existing sessions
and worktree data have not been moved.

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
- The tested messaging/responsiveness release was copied from `cm-manager`.
  Daemon SHA256:
  `b19076d04b972b2284fddea393cffa5ed7fcf42a4ed2cfb6d3c0cef5e4912325`.
  The build label is `0.1.0+dbecd27`; Python includes the later routing fix.
- MCP lives at `/opt/cm-daemon/mcp_server`, with its own virtual environment.
  Preflight registered all 56 tools. Workflows are installed alongside it.
- Claude Code 2.1.263 and Codex 0.153.4 match the existing user installation.
  Both login-status checks passed. Rust 1.98.1, Cargo, uv 0.10.12, Google Cloud
  CLI 583.0.0, Git, Python, Node, tmux and the native build dependencies are installed.
- Both `claude-manager` and `predictionTrading` have fresh Git clones under
  `/home/lucas/code/projects`. These are base checkouts, not migrated worktrees,
  ignored data, project virtual environments or session histories.
- Git access to both private repositories passed. Google Compute API reads passed
  using the same existing project service account as `cm-manager`; no new IAM
  grants were made. Cross-project trader/backtest access has not been migrated.
- Operator credentials are independently generated for this daemon. Auth files
  and pairing tokens are private, mode 0600. Personal SSH private keys were not
  copied to the host. The daemon log is `/home/lucas/.cm/daemon.log`; holder output
  is available through `journalctl -u cm-daemon`.
- The planning API uses the manager's private DNS address on port 8000. The
  manager's own `localhost:8000` configuration cannot be copied unchanged to a
  different VM. Config reload applied this address without a brain restart;
  an authenticated `list_projects` request through the daemon then passed.

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
SSH alias and `/home/lucas/.cm/daemon.sock`. The existing local default is retained.
The TUI reads this file at startup: relaunch the TUI when convenient to expose
the new host in the workspace-creation form, then select **sessions**. Relaunching
the client does not require restarting the remote daemon or its agents.

The new daemon has its own identity,
`83bce20a-1f8e-4dd0-82e1-084fca0881c2`, enrolled as an Owner-authorized replica of
the existing shared space `15b7d0a4-ddcc-4f69-9526-3678e3cadc70`. The coordinator
remains `cm-manager`. Enrollment used the empty destination and the supported
`pair`/`join_space` operations. Sync connected, reconciled, and reported zero
pending publications and no errors.

Host-to-hub sync uses the private VPC route and a dedicated SSH key. The manager's
authorized-key entry forces only the messaging stdio bridge, with SSH forwarding
and interactive shell access disabled. Its host key was pinned from the existing
authenticated manager connection. No test messages were sent to existing agents.

## Validation

A disposable Bash session exercised direct attachment, streamed input/output,
output continuing after viewer disconnect, and a brain-only restart. The restart
advanced the holder epoch exactly once, 1 → 2, with a 1.28-second observed
control-plane gap. The shell retained its PID, kernel start time and holder parent.
All 256 bytes of the pre-restart replay were identical afterward; reattachment
replayed the screen and accepted new input.

The ten-minute stability gate passed: 21 samples over 600 seconds retained
epoch 2, one expected brain restart, a running breaker, healthy MCP and the
same live session count. The disposable session was then removed; the final
daemon and holder session counts are both zero. The existing local and manager
brains retained their prior PIDs/epochs and their 27/20 session counts.

A separate laptop-to-host probe used one persistent SSH Unix-socket tunnel and
the real attach stream. Twelve command/output round trips measured a 42.29 ms
median, with a 37.33–249.56 ms range. This small sample excludes tunnel setup,
TUI drawing and model processing; it is not a latency guarantee or a load test.

## Cost and migration boundary

At the previously quoted us-east4 rates and 730 hours/month: compute is about
$440.75/month and the data disk $112.64/month. A $30 allowance for boot disk,
address and snapshots brings the estimate to **about $585/month additional**.
Actual snapshot use and egress vary; model/API charges and the existing manager
are separate. At those rates, a one-year resource-based CUD would bring this to
about $420/month, or about $340/month for three years. **No commitment was bought.**
Run on demand for a few days before reconsidering sizing and a one-year CUD.

Pricing references: [Compute](https://cloud.google.com/products/compute/pricing/general-purpose)
and [disks](https://cloud.google.com/compute/disks-image-pricing).

The source inventory measured 27 local sessions and 14 active worktrees; active
worktrees occupied 73.26 GiB and the broader selected source roughly 376 GiB.
Migration still needs a staged file transfer preserving dirty/ignored files,
symlinks and Git common directories, followed by one coordinated final delta and
conversation resume. Do not use the old ephemeral-worker push button.

Agent personal messaging identities, DMs and watches do not automatically move
with a conversation. Continuous-task scheduler ownership also needs an explicit
handoff; cloning a live daemon identity is not a supported shortcut. These are
cutover work, along with project-specific credentials/settings, dependencies and
external data access. The requested orchestrator channel and UI work remain
follow-ups. Existing local and manager agents were not interrupted by this setup.

Private setup scripts, inventories and verification evidence are under
`~/.cm/migrations/cloud-20260908/` on the source machine; credentials in that
directory must not be committed. Runtime verification also writes
`~/.cm/setup-verification.json` on `cm-sessions`. A copy of the tested runtime
archive, startup script and service unit is retained in
`~/.cm/provisioning/` on its data disk, which is covered by the snapshot schedule.
