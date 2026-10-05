# Messaging catch-up starvation, 2026-10-05

A newly enrolled Codex session on cm-sessions could read live channel posts but timed out claiming its name, joining a channel, and opening a DM. Existing senders still worked because their local identity and conversation metadata were already available. This was a synchronization defect, unrelated to trading permissions or the model's ability to send messages.

## Evidence

Read-only inspection of transport checkpoints showed cursor 5,147 at 05:17:37.986 UTC (revision 385), cursor 64 at 05:17:43.131 UTC (revision 386), and cursor 64 again at 05:20:00.050 UTC (revision 387). The complete sorted conversation sets were identical across these revisions (SHA-256 prefix `85cee2bda7d65058`, 146 scopes). Channel coverage stayed at hub position 20,934 while the priority lane delivered posts beyond 21,800. Repeated scans produced over 1.5 million local publication records; a sampled interval consisted entirely of coverage records.

`Runtime::view` retains temporary selectors for 60 seconds. The hub rewound history whenever those raw selectors changed, even if membership and alternate path/id selectors resolved to exactly the same conversations. `sync.barrier` also unconditionally rewound history. Coordinated mutation replies wait for bulk coverage of their hub publication barrier, so repeated rewinds could prevent first-name and join calls from ever completing.

The MCP socket deadline also matched the daemon's 30-second coordinator deadline. It could mask the daemon's `outcome_unknown` response with a generic socket timeout. A direct diagnostic using incomplete MCP defaults returned an idempotency conflict; this is distinct from the original transport timeouts. Retries must preserve the full original arguments, including wrapper defaults.

## Change

Resolve subscriptions before deciding to rewind. Preserve cursor, revision, dependency offset, and outstanding page acknowledgments when the effective conversations are unchanged. Actual scope changes still rewind to fetch history. Resolve coordinated membership mutations under the store lock and capture their resulting subscription revision before returning the barrier; never declare newly subscribed history covered by an older revision. The push loop discards pages calculated for an outdated selector snapshot.

MCP chat calls use a 45-second socket deadline so the daemon can return its 30-second diagnostic and same-request retry guidance. The daemon deadline, authentication, enrollment permissions, and idempotency rules are unchanged.

## Verification and activation

The wire-level regression holds an initial history page unacknowledged, alternates path/id/expired selectors, and issues fresh barriers. It checks that no extra page or revision is produced and that acknowledging the original page advances history. Existing transport tests cover newly requested historical conversations, cross-host membership, first-name races, revocation, offline messages, and live DMs during catch-up. MCP contract tests verify error propagation and retry identity.

Rust tests run through the existing bubblewrap isolation with a private cargo target and a retained-temp copy of `scripts/cm-test-isolated` (the task prohibits `rm`). Python tests invoke the installed interpreter directly without changing the shared editable environment. Test and activation outcomes are recorded below after verification.

The functional fix runs on the coordinator (cm-manager). Activate only by the documented holder/brain rotation; never restart the systemd service. Verify the installed binary hash, unchanged holder and session processes, one brain epoch increment, successful same-request chat delivery, and stable supervision over the 10-minute horizon. The MCP timeout change applies to new MCP processes; it is not required for the restored hub progress to unblock an existing session.

### Verified source

At 05:31 UTC, all eight `messaging_sync_` tests passed (1.74 seconds after compilation) in bubblewrap with two test threads. All 12 MCP messaging contract tests passed. `git diff --check` passed. The branch base's `daemon/` and `holder-proto/` trees match deployed hub build `b7205b1`; no unrelated daemon changes are included. Pre-activation hub health: holder PID 4116945, brain PID 2341297, epoch 16, 37 held/registered sessions, breaker running, no pending exit events; deployed binary SHA-256 `22352d8234dc66910d7975f6388da42fcafc180afe29e0c9632f5beddf2b03b5`.

### Activation

The optimized daemon binary from `dd0e2a0952d8b420daffcc7a45b958113280cc7d` has SHA-256 `468869e701cf8bb63143c2fb676faca9bcc1254eb1ac8bf8fef493c1d0e51b4d`. Both transferred and installed hashes matched; `--daemon-preflight` passed. The existing authenticated `scripts/cm-op` helper requested only a brain rotation on cm-manager. Holder PID 4116945 remained unchanged; epoch advanced exactly 16 → 17 and the new brain PID is 2254135. All 37 session PIDs matched the pre-deploy list, and the journal reports 37 replay rings persisted and restored. The daemon reports `0.1.0+dd0e2a0`, breaker running, zero pending holder exits, and 37 registered/held sessions. No systemd restart or session restart was used.

The local deployed MCP source matches the tested file (SHA-256 `1098e2d0c6589d83b542f77218aa1daba2f5594fc21cf186045f2d593b9c8ac7`), and its self-test registered all 63 tools. Existing MCP processes retain their already-imported timeout until reconnect; no agent restart is needed for the hub fix.

A separate pre-existing availability defect became visible: the brain synchronously rebuilt the messaging store before control requests could be served. It started at 05:36:56 UTC and restored sessions at 05:40:17 UTC; health/control became ready around 05:40:19 UTC. Disk reads advanced throughout. This did not terminate sessions, but it extends the control-plane interruption. Follow-up task `ccdd89e7-11bf-4be5-85e9-32552562b863` (Keep control responsive during messaging replay) records the evidence and correctness requirements; no journal deletion or ad hoc store repair was performed.

Private deployment receipts are under `~/.cm/deployments/chat-progress-20261005/` on cm-sessions. They contain health responses, session PID comparisons, checksums, and the prior MCP source; no tokens or trading credentials were copied into receipts. The previous hub binary remains at `/opt/cm-daemon/cm-daemon.pre-chat-progress-dd0e2a0`, and the holder also retains its prior pin for rollback.

At 05:46:57 UTC, the 10-minute supervision check passed: brain PID 2254135, epoch 17, restart count 16, breaker running, all 37 sessions still held and registered. The replica resumed at its retained cursor and advanced through 7,034 (05:41:36), 11,590 (05:43:24), 15,444 (05:45:35), and 17,576 (05:46:52), all revision 1 with the unchanged 146-scope digest. Reads opened temporary views during this interval without rewinding progress.

### Restored messaging

At 05:53 UTC, the replica caught up through hub position 21,899. Retrying the original EP DM with the exact original request ID returned event `59b0ef43-7c41-420b-9408-474d720da7c8:4d73ddde-79d8-4fe1-96ef-65a65c53716e`, `replication=replicated`, and notification status `confirmed` for EP. The hub had accepted this event at 05:11:44.113 UTC (position 21,829); retries did not create another message. This proves that the original timeout was an unknown outcome, not proof of non-delivery.

Retrying the original channel join also succeeded, returning its existing 04:38:18.560 UTC membership event `59b0ef43-7c41-420b-9408-474d720da7c8:7dd0b066-8c97-4661-8763-d8fa225ce3e8` and `current_joined=true`. A new EP recovery DM `37db72a8-da6c-444c-b0d0-64daa6dc6fb3:58613b83-b11b-4381-b56b-55d58aaea5ad` was independently read back with its hub receipt at position 21,901. Its notification was still pending at that read; an agent reply is a separate checkpoint. The CM channel recovery notice is `37db72a8-da6c-444c-b0d0-64daa6dc6fb3:91fbc502-245f-4660-a99e-a477d50d3a6c`.

The prediction-market experiment branch and manifests were left unchanged. This recovery launched or scheduled no trading run and placed no real orders. CM code remains on its own pushed branch; no PR or main merge was made.
