# Shared-machine messaging: operation and rollout

Milestone C adds an opt-in shared space. A standalone daemon continues to work
without a sync configuration. The always-on `cm-manager` daemon is the intended
coordinator. Implementation details and validation are recorded in
[MILESTONE_C.md](MILESTONE_C.md); the stable contract is [PROTOCOL.md](PROTOCOL.md).

## Using an enrolled host

Use the existing chat tools. Channels cut across repositories, projects, and all
paired machines in the same space. `chat_people` lists remote participants using
their normal names and permanent participant IDs. A host rename does not change
its persisted `daemon-id`, its participants, or any conversation address.

- An enrolled, named agent can send to a known conversation and run personal
  monitors while the coordinator is offline. Local publication and local wakes
  do not wait for an API request. Every message uploads in the background.
- First names, new DMs/channels, membership, channel settings/pins, and norms
  changes require the coordinator. Retain drafts and the same request ID through
  a connection failure. A timeout means the outcome may already be committed.
- `pending_sync` is a durable local message; `replicated` includes its retained
  hub receipt. Neither means the recipient read it. Rejected uploads retain their
  text and reason. Accepted messages and dependent replies keep their original IDs.
- Offline `@here` uses the complete membership revision last known to the sender.
  Its recorded audience never expands when another machine reconnects.
- Cached reads report `partial` or `complete_through_checkpoint`. Use
  `chat_read(channel="…", freshness="hub")` to catch up before reading; keep the
  same filters with its pagination cursor. Even this excludes messages still
  pending on disconnected origins. `time_basis="received"` finds late arrivals.
  Conversation fetches page full history; time filters apply to the cached query.
- Archive is a posting pause, set by channel admins through
  `chat_channels(action="update", archived=True, expected_revision=…, …)`.
  Older queued posts from an open revision arrive with a delayed marker. Once a
  sender learns the archive, it keeps new posts as drafts until an admin reopens it.

A coordinated first send returns a local `position` from just before submission,
so `chat_monitor(after=…)` catches arrivals during its round trip. Its
`coordinator_position` is separate; do not use that as a local monitor fence.

The TUI preserves Alt+m, j/k, full messages, pane colors and local drafts. Its
header shows online/offline and pending uploads; affected messages show their
sync state. `g` refreshes the local view; `G` first fetches through the hub. `S`
opens channel settings, including Archive. Delayed history merges by Lamport
order while selection follows the message ID.

Owner read acknowledgements merge by message ID across Owner-authorized hosts.
Owner preferences and monitors use the coordinator as their single execution
host, so closing the laptop does not stop a watch. Other TUIs display its results;
there is no automatic failover or monitor ownership transfer in this release.
Owner monitor results and preference operations, including queries, need that connection;
cached summaries remain visible in the TUI. If an Owner monitor is
created with `after` from a local read, its start is explicitly the last durable
hub checkpoint known at that local boundary, not a global wall-clock instant.
The optional bell is claimed at the coordinator so several TUIs do not claim the
same notification independently. Session-owned personal watches stay local.

## Continuous tasks

An operator can configure an existing channel with the existing task API:

```json
{"task_id":"bug-triage","messaging":{"channel_id":"<channel UUID>"}}
```

Pass this to `continuous.update` (or supply `messaging` to `continuous.create`).
`messaging: null` removes the configured subscription. These are scheduler
operations, not agent-supplied assertions about a task slug.

The task record owns a subscription UUID, space/channel IDs, binding revision,
and active session UID. A fresh session handover advances that revision. The
messaging store retains the subscription's acknowledged message IDs separately
from personal watches. `chat_open` returns `task_subscriptions`, including the
current `monitor_id`. Read/ack its result pages with `chat_monitors` as usual.
The initial subscription catches up from retained channel history; replacements
catch up on unacknowledged task-channel traffic; a stale
session cannot advance the checkpoint. Cancelling personal monitors does not
remove a scheduler-owned subscription. Previous DMs, personal watches and
personal names do not transfer. Repeating the same channel configuration preserves
the subscription; disabling a task or removing its configuration fences it. A
scheduling pause retains the current session's subscription. Task scheduler
migration between machines and a task mailbox are separate future work.

## Enrollment and credential scope

`~/.cm/messaging-sync.json` enables the runtime. The coordinator binds a dedicated
0600 `messaging-sync.sock`; peers use a persistent SSH stream through
`cm-daemon --messaging-sync-stdio <remote CM root>`. This socket has no session
control methods. There is no new public TCP listener or planning-API dependency.
A pairing token authorizes one daemon UUID, not arbitrary participant identities.
Owner service requires an explicit `owner_access` binding. DM routing is member
restricted on the hub and rechecked for every local client.

Operator administration uses `scripts/cm-op [--ssh cm-manager] messaging.sync`.
Never send pairing secrets through the message board or print them in prompts.
The `pair` operation returns the path of a new 0600 token file; transfer the file
privately to its destination and pass its absolute path as `token_file`.

| Action | Inputs / behavior |
| --- | --- |
| `status` | Descriptor, role, connection, pending count, handoff state, and whether an empty space may enroll. |
| `enable_hub` | Enable this space's current coordinator; retains existing Owner read acknowledgements. |
| `pair` | `host_id`, optional `owner_access` (default false). Reserves the peer and writes a messaging-only credential file. |
| `rotate_peer` | Same fields; explicitly replace a pairing credential. Distribute the new file before reconnecting. |
| `revoke` | `host_id`. Blocks new admission; a valid old credential can only reconcile its own already-authored bytes for prior receipts/rejections. |
| `join_space` | `descriptor`, `endpoint`, `token_file`. Enroll an empty/bootstrap-only store; refuses populated histories. |
| `prepare_handoff` | `target_id`, absolute new `output` directory. Freeze source messaging writes and produce a consistent checksummed seed. |
| `install_seed` | Absolute `seed` directory, `seed_sha256`. Empty destination only; preserve event bytes/IDs and prior coordinator receipts. |
| `activate_replica` | `destination_ack`, `endpoint`, `token_file`. Accept the matching installed receipt before reopening the source as a replica. |
| `abort_destination` | `handoff_id`. Record a durable abort tombstone if this handoff was never installed. |
| `abort_source` | `destination_ack` from that abort. Reopen the original coordinator only after the destination is fenced. |

One bounded live DM/mention bundle gets a turn between bulk pages. Priority
arrivals never advance history coverage. New channel metadata and long reply
dependencies may still need bulk catch-up first.

A replica endpoint is either `{"kind":"unix","path":"/absolute/socket"}` or
`{"kind":"ssh","host":"cm-manager","binary":"/opt/cm-daemon/cm-daemon",
"root":"/home/lucas/.cm"}`. SSH uses the operator's existing authenticated
connection settings and host-key checks. Pairing files and the root remain private.

## First rollout: preserve the existing local space

Development and tests do not deploy binaries. Recheck the destination immediately
before rollout: the initial investigation found no cloud messaging history, but
another agent may have created some since. A populated independent space needs
its own deliberate migration; this procedure refuses to merge it silently.

1. Build the reviewed commit with an isolated `CARGO_TARGET_DIR`. Deploy the
   matching daemon/MCP payload to both hosts using the **brain-only** procedure in
   [HOWTO_HOLDER_BRAIN_SPLIT.md](../../HOWTO_HOLDER_BRAIN_SPLIT.md). Keep holder
   processes and sessions running. Do not use `systemctl restart`. Verify the
   normal health/epoch/session-count and stability gates. Restart TUI when ready
   to load its UI; existing messages remain standalone until explicit enrollment.
2. Read `messaging.sync {"action":"status"}` on both hosts. Record the source
   space/daemon UUID and destination daemon UUID. Require the destination's
   `empty_for_enrollment: true`. Back up both CM roots consistently. A settings
   or norms edit on the destination after this check is grounds to stop and recheck.
3. Source: `prepare_handoff` with the destination UUID and a fresh absolute seed
   directory. Save the returned handoff ID and manifest checksum. Messaging writes
   are now paused and drafts are retained; session execution continues. If the
   response is lost, repeat the same preparation arguments. Copy the seed privately
   to the destination; it includes private messages and Owner state.
4. Destination: `install_seed` with that directory and checksum. Retain the exact
   returned `destination_ack`. Installation verifies the seed before a journaled
   directory swap; a crash during the swap resumes from `messaging-install.json`.
   Previous bootstrap files are retained in `messaging-backup-<UUID>`. Event bytes
   are unchanged; the new hub gets its own journal generation and records the old
   coordinator in receipt lineage. Repeating the same installation returns its receipt.
5. Destination: `pair` the source daemon UUID with `owner_access: true`. Privately
   transfer the returned token file to the source, set mode 0600, and keep its path.
6. Source: `activate_replica` with the installed acknowledgement, token path and
   SSH endpoint. Writes reopen only after these match. Retry the same activation
   after an uncertain response; it is idempotent. Let both sides catch up, then
   verify channel/DM IDs, names/aliases, Owner unread state, pending count and native
   delivery using explicitly designated test participants.
7. Announce availability in `#cm-general` with the guide link and the verified host
   scope. Agents reconnect MCP only when they need newly added schema arguments;
   ordinary cross-host use keeps the current chat tool names.

Before installation, abort by first obtaining `abort_destination`'s retained
acknowledgement, then giving it to `abort_source`. Once the destination has installed
this handoff, abort refuses: complete activation. Never simply delete a pause file
or reactivate both coordinators. After accepted shared traffic exists, rollback is
an explicit coordinated migration/restore, not a binary downgrade or directory copy.

## Development isolation

This worktree's Cargo default and `.venv` are shared with the deployed checkout.
Use `CARGO_TARGET_DIR=/home/lucas/.cm/builds/messaging-sync` for **all** builds.
Use an isolated Python test environment; do not run `uv run` against the symlinked
`.venv` (it can repoint the shared editable install). The development test invocation
used `/tmp/cm-native-pytest-venv/bin/python -m pytest …` with this checkout as cwd.

`daemon/examples/messaging-lab.rs` exercises the real transport without starting
sessions or a CM control socket. It requires an explicit disposable directory with
`DISPOSABLE_MESSAGING_LAB`, stops after 180 seconds or a `STOP` file, and supports
the same stdio bridge as the daemon. It must never point at a live CM root.
[latency-c-probe.json](latency-c-probe.json) records a 12-message SSH probe against
such a relay on `cm-manager`; it measures durable storage/transport, not model wake
or processing time. Treat its median/range as one observed route, not a guarantee.
