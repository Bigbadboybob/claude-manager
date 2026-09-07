# Final independent review

One review agent inspected the design, protocol, sync proposal, and relevant code. It returned four contract issues before implementation and no additional blocker in the naming rules, Owner behavior, or checked delivery primitives. The parent agent made the corrections below; this record does not claim a second independent review of those edits.

| Finding | Resolution |
| --- | --- |
| The protocol still described one global sequence while sync needed independent writers. | Consolidated one [v1 file/journal contract](PROTOCOL.md#2-files-and-authority): permanent origin/event UUIDs, Lamport conversation ordering, replica arrival positions, explicit coverage, and distinct cursor types. Single-host A uses this format; shared C enables transport. |
| Offline posts under stale archive/revocation metadata had no defined outcome. | [Admission policy](PROTOCOL.md#7-replication-admission-and-rejection) accepts valid queued posts from an older open revision, enforces archive once observed, and rejects not-yet-hub-accepted operations after host revocation. Rejected parents propagate to pending replies; local records/hits remain annotated and unsubmitted wakes are cancelled. |
| Public replicas lacked a canonical name source when the first claim occurred in a DM. | Retain [public identity attestations](PROTOCOL.md#public-identities-survive-a-rebuild), containing no private source or conversation information. Claims store prepared attestation bytes for crash recovery; public replicas rebuild from the retained attestation, not disposable profiles or private DMs. |
| An uncertain Owner send retried on another host could create a second event ID. | Persist a [request-origin binding](PROTOCOL.md#request-identity-and-cross-host-retry) before submission. A new host routes/reconciles the original operation or reports `retry_origin_unavailable`; it cannot remint the message. The hub also enforces the global actor/request key. |

The hook check confirmed that [`_drain_inbox`](../../mcp_server/hooks/cm_stop_hook.py) already claims a file through atomic rename before reading it. The proposed Rust reclaim competition can reuse that primitive; durable wake coordination and receipt verification remain implementation work.

The review treated numerical defaults, transport-library selection, optional social actions and a private task mailbox as reasonable deferred choices. It did not recommend removing any explicit Owner requirement. There are no remaining identified design blockers from this review; runtime conformance, delivery integration and failure-path tests remain the implementation acceptance gates in [DESIGN_MESSAGING.md](../../DESIGN_MESSAGING.md#milestones).

Validation of the corrected documents covers local links/anchors, table/fence structure, JSON examples, example ID/count constraints, and the recorded latency statistics. No messaging runtime was implemented or deployed during this review.
