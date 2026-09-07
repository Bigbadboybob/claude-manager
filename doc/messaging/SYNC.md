# Shared messaging across machines

Status: consolidated design, not implemented. Shared-machine messaging and local operation use the [v1 protocol](PROTOCOL.md). That file owns IDs, journals, admission, cursors, and retry semantics; this note explains deployment, routing, and tradeoffs. A single-host release uses the same format before adding replication.

## Recommendation and scope

Use **local-first message creation, background replication through cm-manager, and coordinated metadata**. Each host has a messaging service and readable event files outside task worktrees. A local commit makes an ordinary message available to other authorized sessions on that host immediately after the disk write, even while disconnected. A durable outbox replicates it to the hub, which stores it and forwards it to eligible hosts. Remote delivery necessarily waits for connectivity.

The hub can be a service on the existing `cm-manager` machine. This needs a new replication service and authenticated host pairing, but not a new cloud platform or a planning-API call for every message. Keep a persistent connection with reconnect/backoff and resumable catch-up. The daemon owns that connection and pending work, so neither a laptop TUI nor an individual agent MCP process must remain open for the cloud machine's agents to communicate.

This is more work than one cloud writer, but manageable if ordinary messages are immutable. Synchronizing independent messages means copying missing records and deduplicating their IDs. We do not need to merge two authors' paragraphs into one message or build a collaborative text editor. General document merging is a materially larger problem; the [local-first research paper](https://martin.kleppmann.com/papers/local-first.pdf) discusses both the benefits of local operation and the difficulty of ad hoc conflict-resolution systems. Its observation that a server can provide communication also fits this proposed hub topology.

| Approach | Local behavior | Cost and fit |
| --- | --- | --- |
| One cloud writer plus cached reads | Reads are local; sending and delivery to another local agent wait for the hub. An optimistic composer/outbox can hide some waiting but cannot provide offline local conversation. | Simplest implementation; credible fallback given the measurements below. |
| Local message writes plus coordinated metadata | Existing conversations work locally; remote messages catch up later. First enrollment and metadata publication need the hub. | Recommended balance for Owner's preference. Requires IDs, cursors, and status designed for replication. |
| Arbitrary offline changes to every file | Messages, names, memberships, and norms can all change independently. | Avoid: unique names and concurrent shared-text edits need additional conflict policies; copying directories does not supply them. |

## When to sync and where to send

Use **event-driven push with host-level subscriptions**. A committed message immediately wakes the local outbox worker; normal delivery does not wait for a periodic sweep. The worker streams over the existing authenticated connection, and the hub pushes accepted messages to interested destination hosts. Receivers acknowledge only after durable local ingestion. Small records already waiting together can share a bounded network batch without adding a deliberate per-message delay. Agent wake debounce remains separate from replication latency.

Keep two decisions distinct:

1. **Origin to hub:** upload every accepted local message asynchronously, even when all current recipients are local. This supplies shared history and the hub copy promised by the design. Local delivery never waits for that upload. Suppressing upload based on today's remote interest would leave future readers dependent on the original machine being online. Start with immediate background uploads; only add a slower local-only archival lane if measured traffic justifies the added scheduling complexity.
2. **Hub to other hosts:** push message bodies only where needed. An idle machine does not need a full copy of every channel. The hub keeps the complete accepted history; newly interested hosts fetch their missing history and then follow live updates. Channel directory and public identity/norm revision metadata can remain broadly replicated without broadcasting every body.

The hub maintains authenticated participant-to-host bindings and a revisioned map of each host's aggregate interests. Many sessions following one channel on one host require one network subscription. Channel interest is derived from existing follows, active monitors, a configured continuous-task subscription, or an open conversation view. A one-off history query fetches its requested range; it need not create a permanent subscription. A past post alone is not reliable evidence of who needs future traffic. This follows the established [interest-based publish/subscribe pattern](https://docs.nats.io/concepts/subjects); it is a routing design, not a decision to add NATS as a dependency.

| Traffic | Destination and timing |
| --- | --- |
| DM, including first contact | Immediately push to hosts authorized to serve its participants, using identity bindings even if the recipient never opened the DM. Retain it when the recipient's host is offline. |
| Channel with remote interest | Immediately push one copy to each interested connected host; local fan-out handles its sessions and views. |
| Channel with only local interest | Deliver locally and upload the hub copy in the background; no onward body broadcast to unrelated machines. |
| Explicit channel mention or followed-thread reply | Route the eligible event and required context to the recipient host even without an open channel view. Preserve the existing notification preferences. |
| Newly opened channel or historical time/tag query | Fetch the missing authorized range on demand, report cache coverage, then establish live interest if the view remains open. An unreplicated range is unknown, not empty. |
| Reconnected host | Resume uploads and downloads from durable acknowledgements, and restore durable interests. Deduplicate any records whose acknowledgement was lost. |

Transport interest is not notification consent or access control. Viewing a channel may keep its local cache current without following it for inbox activity. Owner's passive tag view and ordinary public replies still do not subscribe Owner to alerts. DM access checks apply before routing and again before serving a local client. A public identity update must not expose the private source event that established the name.

**Presence determines whether delivery can happen now; durable interest determines what needs catching up.** Closing a TUI can release its view-only interests; a transport disconnect must not cancel follows, monitors, task subscriptions, or their checkpoints. An exited session may stop receiving automatic pushes, but its history stays available for authorized same-UID revival. Owner's DM routes use authenticated Owner clients/host bindings, not whichever machine last sent a message.

Subscribe-and-catch-up must be one ordered operation at the hub: install the interest and capture a stream boundary under the same sequencing lock, serve authorized history through that boundary, and retain subsequent events for live delivery. Persist received records and acknowledgements before advancing the replica checkpoint. This prevents a message falling between “fetch history” and “start listening.” A limited preview does not certify full earlier history coverage. Existing monitor local-arrival semantics still apply: catching up remote backlog can produce a new local arrival, and registering offline cannot establish a global time boundary.

On reconnect, resubmit the current revisioned interest set and reconcile stream cursors before declaring a scope caught up. Bound in-flight bytes and replay pages; a slow destination must not stall local sends or other hosts. Prioritize small live deliveries over bulk historical transfers while respecting conversation dependencies, and give backlog fair progress. Replays are idempotent and replication does not generate a fresh outbound message event.

Use heartbeat/connection-failure detection and periodic lightweight checkpoint reconciliation as a recovery backstop, not the primary message transport. An initial reconciliation interval around a minute, with jitter, is a tunable recommendation rather than a latency guarantee. Retry failures with capped exponential backoff and jitter, resetting on a healthy reconnect. Startup also scans durable pending work, so losing an in-memory “new message” signal cannot strand the outbox. No busy polling or recurring full-directory diff is needed for normal sends.

Acceptance includes changing an interest while a message commits, multiple local subscribers sharing one stream, first-contact DMs, disconnect without subscription loss, opening an uncached channel, and replay after a lost durable acknowledgement. These must preserve channel history and quiet Owner behavior as well as avoiding duplicate delivery intents.

## What sync merges, and what stays coordinated

* **Messages:** immutable creation records with globally unique origin-assigned IDs. Receiving the same ID and bytes again is a no-op; the same ID with different bytes is a visible consistency error. Retries use the original request ID and return the original message ID. Sync acknowledgements do not rewrite message files or reply links. Requests stay pinned to the requesting daemon; switching TUI hosts routes/reconciles the original operation instead of creating a second origin event.
* **Names:** one hub owns the space-wide namespace and historical aliases. Preserve the existing normalization and collision-suffix rules. First send/name claim is an online coordinated operation, including the first message, with one idempotent result. Its recoverable public identity attestation is retained canonically, so public replicas can rebuild the name without its private source. Subsequent ordinary sends can use that committed identity locally. An offline unnamed agent gets a connectivity error with its draft intact; never provisionally grant a globally unique name and silently rename it later. Owner settings use the same coordinator.
* **Channels and DMs:** creation and membership metadata are coordinated initially. Offline posting is limited to existing, locally known conversations. A first DM or new channel may therefore wait on the hub. This preserves the single conversation for an unordered participant pair and avoids divergent channel paths. Group membership changes remain outside the initial scope.
* **Norms and replaceable preferences:** publish against an expected hub revision. Offline changes are drafts; conflicting publications return the competing revision and a diff. Do not automatically splice two normative documents together or discard one via last-writer-wins. Receiving stale norms does not block ordinary messages.
* **Read acknowledgements:** merge acknowledged message identities monotonically. A stale device cannot mark somebody's newer read state unread. Checkpoint compaction must account for missing events; a maximum timestamp or maximum local sequence is insufficient. Owner's shared read state replicates; drafts and scroll positions remain local.
* **Monitors and wakes:** evaluate newly ingested messages at the recipient's daemon. Persist a hit/wake intent keyed by monitor and message ID before delivery. The hub echo of a locally created event cannot create a second hit. Hook/PTY delivery still uses the safe coordinator and its existing uncertainty rules; deduplicating transport does not promise exactly-once agent processing.

Offline availability temporarily permits old norms, names, or archive state. [Protocol §7](PROTOCOL.md#7-replication-admission-and-rejection) fixes the outcome: valid queued posts against an older open-channel revision still sync after archive, and are labeled delayed/stale-revision arrivals. Once a daemon learns the archive, it refuses new posts. Host revocation instead blocks any not-yet-hub-accepted operation. Rejections retain local context, propagate to unsynchronized dependent replies, cancel pending wakes, and annotate existing monitor results. They do not erase already delivered local effects. Immutable DM membership and the existing same-operator trust boundary still apply.

## Fixed protocol choices

These choices now live in [PROTOCOL.md](PROTOCOL.md), including for the first single-host slice. They replace the earlier global-sequence design; this table is a summary, not another contract.

| Concern | Fixed choice |
| --- | --- |
| Immutable ID | Origin daemon UUID plus event UUID, assigned once. A pinned requesting daemon/request key prevents cross-host retry duplication even when the accepting daemon is the hub. |
| Ordering and files | Retain event files plus atomically published local journals. Local arrival positions drive feeds/watches; a Lamport counter plus origin/event UUID orders conversation display deterministically. |
| Reply dependencies | Validate parent and conversation before local publication; upload dependencies before replies. Rejected parents reject unsynchronized descendants. |
| Pagination and monitors | Distinct opaque position tokens and filtered pagination cursors bind replica, generation, and fixed snapshot. A foreign cursor requires resync. |
| Time queries | Origin creation time is the default; `time_basis="received"` filters this replica's first receipt time. Relative bounds freeze once on the queried daemon. |
| Completeness | Cached results state their coverage. `freshness="hub"` requests a catch-up barrier; even hub-complete history excludes pending records on disconnected origins. |
| Public identities | Retain canonical public identity attestations without private DM/source references; profile JSON and SQLite remain disposable. |
| Recovery | Publication/status journals retain IDs, request bindings and receipts. Restore changes cursor generation and reconciles before reopening writes; no automatic coordinator failover. |

“New DM” means newly received and unread at the queried replica. A DM composed an hour ago on an offline host can be new on arrival now while falling outside a creation-time query for the past ten minutes. MCP should expose both timestamps and permit an arrival-time query. Monitors use ingest positions, not creation timestamps. Registering while disconnected watches subsequent local arrivals, including remote backlog; it cannot claim to start at a globally simultaneous boundary. Expiry likewise applies to local observation unless an explicitly different replay policy is selected.

Replicate public identity projections broadly; replicate public channel history according to host interest or an authorized history request, as described above. Replicate a DM only to hosts authorized to serve one of its members, and enforce membership again for every local client. A hub is a trusted storage intermediary, not an extra chat participant; its storage role does not let Owner browse other agents' DMs in the product. Do not broadcast private creation records, snippets, norms, monitor results, or receipts on a public sync stream. Pairing assertions are messaging-specific and confer no session-control rights.

## Agent and Owner behavior

`chat_send` should return a durable local message ID plus independent replication and notification status. Suggested states are `saved_locally / pending_sync`, `replicated`, and a visible replication error; none means that the recipient read the message. Same-host agents can read and react to a locally accepted message before hub acknowledgement. An automatic retry never reposts it. A caller that specifically needs hub durability can request or await that acknowledgement; normal sends do not wait.

Reads are local by default and report freshness in messaging responses only. Preserve the existing quiet Owner inbox rules. The TUI shows “Saved locally · syncing” or “Saved locally · offline” beside pending posts, plus a compact connection indicator. After replication, it can show “Synced”; recipient offline and notification deferred are separate facts. Restart preserves the draft/request association and pending message rather than inviting a duplicate send. Owner gets the same local send, time/arrival query, and monitor capabilities as agents.

Owner can have several TUIs, so replicated read state must not multiply wake ownership. Recommend designating one notification-owning daemon per Owner monitor; other TUIs display its replicated results. Moving that ownership is an explicit revision-checked operation while connected. No automatic partition failover in the first shared release. Session-owned monitors run on their session's host; a new session does not inherit them merely by using a similar name.

## Continuous tasks

Cloud-resident continuous tasks benefit immediately: agents co-located with the hub do not cross the laptop-to-cloud link for their own local conversation. Laptop agents join the same channels through background sync; closing the laptop does not stop cloud-side exchange.

There is a separate lifecycle issue. The scheduler reuses a live persistent session, but a fresh run or dead-session replacement mints a new UID; see [the continuous trigger implementation](../../daemon/src/control/methods.rs) and [`ContinuousTask`](../../daemon/src/continuous/task.rs). That struct has a task slug and optional backing planning UUID. Neither a display name nor a task slug alone should be treated as a globally unique cross-host mailbox identity.

Start with a configured channel ID for each continuous task, such as `#tasks/bug-triage`. Store that binding with the task and return it during messaging orientation. A replacement orchestrator reads the same channel, retains its own participant identity, and receives a scheduler-bound task subscription with a durable catch-up checkpoint. Handover records the active session binding/revision, so a stale orchestrator cannot also own the subscription. Ordinary channel history already survives session replacement; no private DM inheritance is needed for that continuity.

A stable **task mailbox** is a useful later option when callers need to address “Bug Triage” regardless of which session is active. It needs its own registered task identity, explicit scheduler-controlled access and transfer rules, and visible attribution such as “Latency Scout for Bug Triage.” Address it separately from a person's DM. Do not silently hand a predecessor's private DMs to a new session, or let a session claim a mailbox by supplying a task slug. Channel continuity belongs in C; a private task-mailbox feature needs its own bounded design if desired.

## Measured latency and its limits

Read-only probes from the current workspace host to the configured `cm-manager` host on **2026-09-07 UTC** (September 6 locally):

| Probe | Samples | Results |
| --- | --- | --- |
| ICMP ping | 8, no packet loss | Minimum 45.0 ms, mean 79.7 ms, maximum 147.3 ms. |
| Small application echo over one established SSH connection | 12 sequential requests | Minimum 47.6 ms, median 164.3 ms, maximum 279.4 ms. |
| Establish SSH and start the echo helper | One setup | 3.60 seconds; not a per-message cost when the connection is reused. |

The [measurement record](latency-probe.json) contains the application samples and probe description. These are a small sample on one real route, not a future CM messaging API benchmark, a production percentile, or an estimate for every machine. Echo includes SSH and scheduling overhead; the future API adds validation, queuing, and durable storage work. The difference from ping demonstrates why network RTT alone is insufficient. Measure warm send-to-durable-ack and send-to-recipient-visible latencies in the implementation, separately from cold connection setup and safe agent wake delay.

For this route, a centralized warm send would plausibly be on the order of a few tenths of a second plus service/storage overhead. That inference is fast enough for many agent conversations. The proposed two-second wake debounce already exceeds this measured network delay, so local-first is primarily valuable for independent local operation, responsive UI/tools, and resilience to disconnects. Local commit latency itself has not been measured. Do not advertise a sub-10-ms guarantee, treat half an RTT as a measured one-way delivery time, or add a fresh SSH handshake to every message.
