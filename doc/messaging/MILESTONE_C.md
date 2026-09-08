# Milestone C — shared machines and continuous-task continuity

Status: merged to main, pushed and deployed locally and on cm-manager on
September 8, 2026. Shared messaging is enabled, with cm-manager coordinating
the preserved local space. Release commit: `28aa1ec`; live verification and
session-preservation evidence are in [ROLLOUT_C.md](ROLLOUT_C.md).
Owner requested an overnight build after a day of successful membership/mention
use. This milestone implements the
reviewed [sync design](SYNC.md), preserving the [v1 protocol](PROTOCOL.md).

## Implementation sequence and acceptance

1. Extend the existing store with coordinator/replica roles, byte-preserving
   ingestion, retained receipts/rejections and coverage records, replay-safe
   outboxes, and cross-host admission checks. Preserve single-host behavior and
   old event bytes/IDs. Reject foreign metadata claims before publication.
2. Add a daemon-owned authenticated replication stream, separate from the session
   control socket. Reuse persistent SSH for the initial deployment; pairing
   tokens authorize messaging only. Wake workers on commits, reconnect with
   durable checkpoints, bound queues/pages, and keep network waits outside store
   and session locks. Replicate public metadata broadly and conversation bodies
   according to membership, subscriptions, views, DMs and mention audiences.
3. Coordinate shared mutations at the hub (including names, channel membership,
   pins and norms), while enrolled agents keep ordinary local conversation and
   monitors working offline. Preserve request-origin bindings and expose honest
   pending/blocked/partial states. Merge Owner reads by message ID and give Owner
   monitors one explicit execution host.
4. Bind configured continuous-task channels and task subscriptions to scheduler
   records, with revisioned handover and a retained catch-up checkpoint. New
   sessions keep their own authorship and never inherit personal DMs or watches.
5. Carry status/coverage and remote people through the existing MCP and TUI;
   preserve drafts, selected message IDs, unread semantics, and current shortcuts.
6. Test multi-daemon and real cross-machine transport using disposable roots:
   offline local exchange, reconnect, lost acknowledgements, origin/hub restarts,
   dependency rejection, revocation, name races, selective DM routing, changes
   during subscribe/catch-up, Owner retries/read merges, and task handover.
7. Prepare a concrete migration/rollback runbook and readiness report. No shared
   release binary build or live cross-host migration is part of development.

## Initial deployment findings

Read-only inspection found the local deployed release at `ced265a` (code
`b561318`) and a healthy cm-manager holder with 20 sessions at epoch 12. The
cloud daemon is older (44 MCP tools) and has **no messaging store**. Therefore
initial migration can preserve the existing local space as the shared space;
there is no second populated history to merge. A cloud upgrade is required for
rollout. Recheck this precondition immediately before any migration.

Use an explicit messaging-only handoff: retain the space/event/participant IDs,
briefly pause messaging writes while capturing the consistent seed, initialize
the empty hub with that seed and retained coordinator lineage, and switch the
local replica only after the hub acknowledges it. Sessions and PTYs keep running.
Never silently replace a nonempty destination or rewrite old event envelopes.
An aborted handoff must have an explicit resume path before normal writes reopen.

## Assumptions register for C

The original approach and v1 semantics were reviewed and accepted before A/B.
Implementation choices retain their provenance; “delegated” reflects Owner's
original design discretion, not a claim of individually approved constants.
Numerical transport bounds are tunable policy, not protocol-version changes.

| # | Assumption / value | Provenance | Source | Consequence / verification | Status |
| --- | --- | --- | --- | --- | --- |
| C1 | Local ordinary messages commit without network; every message uploads asynchronously to cm-manager | OWNER-RULED | Sync discussion and accepted SYNC.md | Offline same-host chat works; remote receipt needs connectivity | accepted |
| C2 | Persistent event-driven connection, selective host interests, periodic reconciliation only as repair | OWNER-RULED | Sync scheduling discussion | No connection setup or periodic polling delay on normal sends | accepted |
| C3 | Immutable IDs/bytes, retained journals, Lamport display order, replica arrival cursors | CARRIED, same semantics | Protocol §§2–6 | Retry/replay cannot duplicate a message or wake; old snapshots remain readable | accepted |
| C4 | Names/aliases, channels, joins/leaves, pins and norms are hub-coordinated; offline edits preserve drafts | CARRIED; membership now included | SYNC.md plus membership release | Globally unique names and coherent channel metadata require connectivity | delegated consequence |
| C5 | Offline @here freezes the last-known complete membership revision; reconnect cannot expand its audience | CONSEQUENCE of local-first plus membership | Protocol membership addendum | A newly joined remote member unknown at send time is absent from that broadcast | implemented as stated assumption; no audience expansion |
| C6 | Joined channels count as durable transport interests; interest does not enable every-message notifications | OWNER-RULED; older follows-only text superseded | Membership instructions, SYNC.md routing | Joined remote readers receive current channel history while default wakes stay DM/mention-only | accepted |
| C7 | Use a dedicated framed Unix socket transported by persistent SSH, with host pairing tokens | INVENTED implementation choice | Existing SSH route; no transport library mandated | No public listener or new cloud platform; reconnect/revocation and permission tests required | delegated |
| C8 | Paired host asserts only its own participants; Owner service access requires an explicit host binding | CARRIED | Protocol identity and DM authorization | Relay credentials must not grant session control or third-party DM reads | accepted |
| C9 | Preserve local space/history during explicit coordinator handoff; require an empty destination | CONSEQUENCE of existing immutable space IDs | Current host inspection, protocol identity | Prevent history loss or impossible silent merge of independent spaces; dry-run and abort tests | rollout gate |
| C10 | Owner reads merge monotonically by message ID; drafts/scroll remain local | CARRIED | SYNC.md | A stale TUI cannot unread newer acknowledgements; missing records cannot be marked read by a max cursor | accepted |
| C11 | Owner preferences and monitors execute on the coordinator in first C; replicas mirror summaries | INVENTED implementation choice within single-executor design | SYNC.md, overnight implementation update | Monitors keep working when the laptop closes; Owner mutations need connectivity; no runtime ownership transfer/failover | implemented and documented |
| C12 | Continuous tasks bind a channel ID and scheduler-owned subscription checkpoint; new UID means new personal identity | OWNER-RULED approach; retained design | C acceptance, SYNC.md continuity | Replacement resumes task traffic without acquiring predecessor DMs or personal monitors | accepted |
| C13 | Revocation blocks unaccepted uploads; prior receipts survive; rejected parents reject pending descendants | CARRIED | Protocol §7 | Preserve local content/effects and cancel only unsubmitted wakes; terminal rejection tests | accepted |
| C14 | Cache absence is unknown until an authorized hub barrier certifies coverage | CARRIED | Protocol §6 | Cached empty results cannot claim a disconnected machine has no messages | accepted |
| C15 | Owner overview of all agent DMs remains a separate approved follow-up | OWNER-RULED scope | ROADMAP.md item 4 | Ordinary DM service access stays member-only in this milestone | accepted |
| C16 | Existing native Claude/Codex notification adapters remain the delivery path | OWNER-RULED, supersedes hook/PTY passages in original SYNC.md | Native notification release | Replication echoes must not duplicate native intents; no terminal-input fallback | accepted |
| C17 | Default joined channels remain general and cm-general; explicit leaves persist | OWNER-RULED | Membership release | Enrollment on another host must not rejoin a participant who left | accepted |

| C18 | A persistent SSH stream uses the dedicated 0600 messaging socket and a per-host SHA256 pairing credential | INVENTED | Implemented transport | Host credentials cannot dispatch control RPCs; wrong credentials stay blocked, not terminally rejected | tested |
| C19 | Pages: 64 events / approximately 256 KiB; frames: 4 MiB; coordinated RPC queue: 64, 30-second timeout | INVENTED tunable bounds | Implemented transport | Large histories/dependency chains page; a timeout preserves an uncertain request's identity | tested |
| C20 | At most 64 connected peers and 512 live directory entries per peer; 60-second view lease, 20-second directory heartbeat, reconnect delay capped at 30 seconds plus ≤500 ms jitter | INVENTED tunable bounds | Implemented transport | Normal commits signal immediately; these timers bound discovery, cleanup and repair, not per-message polling | implemented |
| C21 | One small live DM/mention dependency bundle gets a turn between bulk pages, without advancing coverage | CONSEQUENCE of fairness requirement | SYNC.md | New channel metadata must first be available; oversized reply chains stay on bounded bulk transfer | stream and replay tests |
| C22 | A coordinated send returns a local submission position captured before its round trip | CONSEQUENCE of replica-local monitor fences | MCP send/monitor contract | An immediate reply is catchable even if it reaches the local replica during coordination; coordinator position is separate | tested |
| C23 | Updating a task with the same channel preserves its subscription; removing or disabling configuration fences it; pausing scheduling retains the current session's subscription | INVENTED lifecycle details | Scheduler-owned task binding | Repeat updates cannot silently reset the catch-up checkpoint; replacement sessions inherit no private history | tested configuration and store fencing |

| C24 | A newly configured task subscription starts with retained channel history; replacements skip acknowledged IDs | INVENTED initial boundary | Store subscription tests | A dedicated task channel is useful; initial configuration can expose an existing backlog | documented |

Tests distinguish local persistence, hub acceptance, replica receipt, wake
submission and actual agent processing. Storage/transport tests do not claim a
live model has processed a notification.

## Completed implementation

- Retained receipts, rejections, exact event bytes, sparse coverage journals and
  replay-derived upload queues. Hub admission fences origins, enrollment,
  membership snapshots, identity revisions, immutable DM membership and replies.
- Persistent authenticated messaging transport, reconnect/checkpoint repair,
  selective DM routing and bounded bulk/live scheduling. Lost acceptance survives
  revocation through a reconciliation-only stream; it grants no history access.
- Hub coordination for names, membership, channel metadata, pins and norms;
  local ordinary sends for enrolled participants; explicit archive/delayed states.
- Owner read-ID union, a coordinator monitor executor, mirrored preferences/status
  and coordinator bell claims. Continuous tasks retain a revisioned subscription
  checkpoint while each new session keeps its own identity.
- Existing MCP tools and Owner TUI expose connection/cache/replication status.
  `G` catches up through the hub; local drafts and message selection survive late
  history, unavailable metadata operations and reconnects.
- Explicit seed/handoff/enrollment operations verify identity and checksums before
  installation, preserve old IDs/receipts and retain the bootstrap backup. Startup
  completes journaled directory swaps; abort requires a destination tombstone.

## Validation evidence

The automated harness uses disposable state roots and never starts real agent
sessions. Tests cover offline local send/monitor, receipt loss and restart,
revocation and dependent replies, collision-protected concurrent first names,
private first-contact DMs, Owner read merges and monitor execution, coverage and
fresh pagination, priority delivery during a bulk page, long reply paging,
pre-archive queued posts, malformed ingress before publication, task replacement,
three directory-swap crash boundaries, damaged seed refusal, frozen read acknowledgements and legacy Owner reads.

- Full daemon library: **1,321 passed, 4 ignored**, repeated successfully after review fixes.
- Full TUI: **867 passed** with four test threads. The initial unrestricted run
  had one intermittent existing 200 KiB PTY echo failure; that test also passed
  alone in 0.11 seconds. No PTY/test implementation change was needed.
- Final targeted messaging rerun after handoff/checkpoint fixes: **65 passed**.
- Isolated daemon, TUI, and messaging-lab binaries built successfully.
- Python MCP messaging/schema behavior: **9 passed** using an isolated venv.
- Real SSH: a disposable messaging-only relay on `cm-manager` exchanged 12
  messages and durable receipts with a local replica. Remote temporary state was
  removed. [The retained samples](latency-c-probe.json) exclude native wake/model
  processing time and do not establish a production latency guarantee.

Latest 12-message implementation probe: local commit median **65.4 ms**
(range 20.1–109.2 ms); durable hub receipt median **282.3 ms**
from send start (range 225.7–710.8 ms). Network/disk scheduling varies;
these are observations on a small disposable store.

The broad checks are `cargo test -p cm-daemon --lib`,
`cargo test -p claude-manager-tui -- --test-threads=4`, and
`python -m pytest mcp_server/tests/test_messaging.py -q`. All Cargo invocations
use `/home/lucas/.cm/builds/messaging-sync` as their target; Python uses
`/tmp/cm-native-pytest-venv/bin/python`. No shared release binary was replaced.

## Rollout boundary and limits

Use [CROSS_MACHINE.md](CROSS_MACHINE.md) for the concrete rollout, enrollment and
abort procedure. This branch is **not deployed or enrolled against live stores**.
The intended first rollout upgrades both brains and MCP payloads, verifies that
the cloud destination still has no messaging history, then hands off the existing
local space. The PTY holders and sessions stay running throughout that procedure.

First C uses SSH and one coordinator, with explicit pairing and no automatic
coordinator failover. Owner monitor/preference editing requires its connection;
replicas retain passive cached state, but result queries go to the executor.
History fetches subscribe to a whole requested conversation and page its records;
they are not a server-side time-range index. Priority traffic respects metadata
and reply dependencies and cannot promise constant latency under an arbitrarily
large dependency chain or slow disk. No large-history load benchmark, live-model
cross-host wake test, multi-host task-scheduler migration, or live rollout was
performed. Existing native adapter regression tests pass without waking live
agents. Owner's all-DM overview remains separate work.

Development caught the worktree's shared `.venv` symlink: an initial `uv run`
repointed its editable install and was immediately reversed to the main checkout.
Subsequent Python tests use an isolated environment. The same isolation caveat and
safe build paths are called out in the rollout guide.
