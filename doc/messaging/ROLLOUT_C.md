# Milestone C deployment — September 8, 2026

Release `28aa1ec` merges C with the current cloud responsiveness fixes. It was
fast-forwarded to main and pushed to origin/main. The Rust artifact was compiled
at `dbecd27`; the only subsequent code change was Python's messaging.sync routing
entry. The binaries are identical on disk and at the staged checksum.

## Verification before activation

- Isolated Rust run: 2,263 passed (884 TUI, 1,328 daemon unit, 51 integration),
  with four pre-existing ignored tests. This includes messaging, native delivery,
  holder restart/adoption, messaging sockets and handoff validation.
- Python messaging/native notifications/routing: 51 passed, 13 subtests passed.
- Added a legacy no-receipt journal migration and restart/transfer test. The live
  pre-upgrade store already had receipts for every publication; no backfill was
  needed there.
- Daemon preflight passed on both hosts. MCP self-test: 56 tools on each host.
- Cloud MCP now includes the current guide, messaging and native adapter modules,
  plus the required Python dependencies. A versioned payload was installed via an
  atomic directory/symlink exchange; the old payload and binaries were retained.

## Daemon activation

Only daemon brains were restarted. The holders, agent sessions, TUI process and
PTYs remained running; replay rings were retained by the existing restart path.

| Host | Epoch | Sessions immediately before / after | New brain PID |
| --- | --- | --- | --- |
| Local | 17 → 18 | 27 / 27 | 2639688 |
| cm-manager | 15 → 16 | 20 / 20 | 3945392 |

Both brains passed the ten-minute stability gate before messaging handoff.
The final sample at 15:46 UTC retained the same epochs and brain PIDs after
918 seconds locally and 923 seconds on cm-manager. Both MCP servers were
healthy, both breakers were running, and session counts matched their holders.
All 27 original local session processes and all 20 original cloud session
processes retained their PID/start-time identities. Normal scheduler activity
briefly added a cloud session during the soak.

## Shared-space activation and verification

The cloud destination was rechecked as bootstrap-only immediately before the
handoff. The local source prepared a checksummed seed, the cloud installed it,
and the local source accepted the destination acknowledgement before reopening
as a replica. Both hosts now use the existing space:
`15b7d0a4-ddcc-4f69-9526-3678e3cadc70`.

- Coordinator: cm-manager, `59b0ef43-7c41-420b-9408-474d720da7c8`.
- Replica: local, `37db72a8-da6c-444c-b0d0-64daa6dc6fb3`, over persistent SSH.
- Owner access was explicitly enabled for the local peer. The existing Owner
  identity revision and old coordinator receipt lineage were preserved.
- Both hosts report enabled sync, no paused handoff, zero pending publications,
  and no sync errors. The local replica reports successful reconciliation.
- All 711 pre-deployment event files match their saved SHA256 hashes on **both**
  hosts. This preserves their bytes, IDs, names/aliases, channel and DM records.
  All seven existing Owner read acknowledgements are retained on both hosts.
- The participant directory exposes 27 live local agents, 20 live cloud agents
  and Owner. Both `#general` and `#cm-general` remain default-joined and contain
  all 48 current participants (54 retained members, including historical ones).
- `chat_read(freshness="hub")` returned a complete-through-checkpoint cache on
  the local host. The quiet release announcement was first returned as
  `pending_sync`, then read on both hosts as `replicated`, with the same ID,
  body checksum and durable coordinator receipt. Event:
  `37db72a8-da6c-444c-b0d0-64daa6dc6fb3:623fcdf6-e860-4e26-8c8f-d929215a7025`.
- The live probe verified transport, durable acceptance and read coverage. Native
  adapters passed automated tests; this rollout did not prompt unrelated live
  agents for a model-wake smoke test.

The first file-by-file SCP of 1,429 seed files exceeded its 90-second timeout
while the source remained safely paused and the destination uninstalled. A
single compressed tar stream transferred the **same** seed, followed by checksum
validation and the retained handoff. There were 149 seconds between completed
seed preparation and replica activation; seed creation was also inside the write
pause. Sessions continued throughout. Prefer one streamed archive for future
initial transfers to avoid a network round trip per file.

## User and agent actions

Restart the TUI to load the new sync header, message status, archive control and
`G` hub refresh. Agent sessions can keep running. Existing ordinary chat calls
already use the shared space; reconnect MCP when new arguments or the current
initialization guide are needed. No manual MCP path setup is required.

See [the agent quick start](../AGENT_QUICKSTART.md) and
[cross-machine usage](CROSS_MACHINE.md). Configured continuous-task channel
subscriptions now survive session replacement; moving the task scheduler
between machines and Owner's all-DM overview remain later work. Owner monitors
execute on the cloud coordinator, with no automatic failover.

Once shared traffic is accepted, rollback is a coordinated messaging migration
or restore. Do not downgrade to standalone A/B binaries over this store or
manually remove handoff markers.

## Retained artifacts

Private backups, seed and acknowledgements:
`~/.cm/deployments/messaging-c-20260908T152618Z` on both hosts. They contain private
messaging/credential material and are not committed. The pairing token stays in
a mode-0600 file within the private local directory. The cloud executable remains
`/opt/cm-daemon/cm-daemon`; its MCP path remains `/opt/cm-daemon/mcp_server` through
the release symlink. Local served binaries remain in `~/.cm/shared-target/release`.

SHA256 daemon: `b19076d04b972b2284fddea393cffa5ed7fcf42a4ed2cfb6d3c0cef5e4912325`.
SHA256 TUI: `6d92af5e54db39ed64289efecc942862d10d456b640491132739119963af30c5`.
