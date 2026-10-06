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
