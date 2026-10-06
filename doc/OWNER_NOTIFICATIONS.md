# Owner notifications

Continuous-task workers route routine review requests, progress, recoverable failures and handoffs through their task channel (`ct/<task-slug>`), mentioning their orchestrator. They must not notify Owner for work their orchestrator can handle. Orchestrators notify Owner only for a reviewed decision or blocker that actually needs Owner, preserving any stricter quiet policy. See [continuous reviews and stages](continuous-review-routing.md). This is agent policy; the existing self-scoped notification transport does not enforce review decisions.

`notify_user(message="Decision needed: …")` requests attention for the calling
session. It works for ordinary local/cloud sessions and continuous orchestrators,
without global permissions or an open TUI. Use a concise reason for work ready
for review/deployment, a decision, approval, or a blocker needing Owner. Routine
progress belongs in session chat. A notification does not authorize deployment
or other actions, and does not expand permission to DM Owner or control sessions.

## Delivery and preferences

The session's host daemon saves the alert in `~/.cm/owner-attention.json`, then
sends it through the existing operator-authenticated `manifest.watch` stream.
The updated laptop TUI submits the normal desktop notification and sound, and
blinks the session's sidebar indicator. **Alt+g** prioritizes these indicators,
including in the continuous column. Selecting the row acknowledges the alert.
When a continuous task has started a replacement tick, its pending indicator
follows the matching continuous task on the same host.

With the laptop offline or TUI closed, delivery waits for an updated viewer to
connect. Alerts survive daemon brain restarts. `{"ok":true,"status":"queued",
"alert_id":"…"}` means durable acceptance, **not** desktop delivery or a read
receipt. A disconnected desktop cannot display a notification immediately.

Existing desktop/DND behavior and notification sound remain in effect. The
separate per-session `notify_on_idle` setting is unchanged; explicit requests
have always been independent of that automatic-idle switch. Chat mutes, DND,
follow settings, and native agent wake queues are unchanged. This tool does not
send Telegram messages or alter `notify_command` escalation subscriptions.

An identical reason while that session's alert is pending coalesces to the same
ID. A changed reason replaces it with a new alert. Desktop submission IDs are
retained on the laptop in `~/.cm/owner-notification-receipts.json` (latest 4,096),
so reconnecting/reopening the viewer normally restores the indicator without a
duplicate popup. Each desktop installation has its own receipt file. There is
an unavoidable crash window between desktop acceptance and saving the receipt;
the system does not claim exactly-once delivery. A failed desktop submission is
logged, retains the sidebar indicator, and can retry on stream reconnection.

Messages are limited to 4,096 UTF-8 bytes. Pending storage is bounded (4,096
sessions and 512 KiB serialized); overflow fails explicitly and retains existing
alerts. An alert from an exited ordinary session still gets its desktop
notification on connection, even if its row is gone. It remains stored until an
operator acknowledges it. No agent can acknowledge another session's alert.

## Owner availability

Owner sets how reachable they are: `away` (emergencies only), `around`
(blocking and above), `focused` (decisions and above) or `on-call` (everything).
Set it with `scripts/cm-availability <level> [--note "..."]` on a host whose
daemon serves Owner (cloud hosts: `ssh cm-manager cm-availability away`, once
installed to `~/.cm/bin/`); with no argument it prints the current level, and
`unset` returns to the default. The level is an Owner-only messaging event
(`owner.availability`) published on the messaging hub and replicated to every
enrolled host; each daemon projects the latest value to
`~/.cm/messages/main/OWNER_AVAILABILITY.json`. Agents read it from `ping()` and
`chat_open()` as `owner_availability: {level, set, changed_at, age_s, previous,
source, owner_note}`. The RPC is `messaging.availability {action: get | set,
level, note?, source?, request_id}`; `set` requires the Owner/operator caller.

### The gate

`notify_user(message, urgency)` takes `fyi`, `decision` (default), `blocking`
or `emergency`. A request is delivered when its urgency meets the level's bar:

| Level | Delivered | Held |
|---|---|---|
| unset | everything | nothing |
| on-call | everything | nothing |
| focused | decision, blocking, emergency | fyi |
| around | blocking, emergency | fyi, decision |
| away | emergency | fyi, decision, blocking |

The result is `{status:"queued", delivery:"immediate"|"held", alert_id,
urgency, owner_availability, release_when?}`. Held requests live in
`~/.cm/owner-attention-held.json` (latest per session plus a count, bounded like
the delivered queue), never in `owner-attention.json`, so viewers never show
them. When the level changes (set locally or replicated from the hub), every
daemon's delivery worker releases the held requests the new level delivers into
the normal queue, marked `released_at` (merged into a pending alert for the same
session, up to 4,096 bytes), and wakes once per revision: continuous
orchestrators, live sessions bound to an active initiative's coordinator task,
sessions the work-item board registered, and sessions with held or released
requests. Wakes happen only when the level value changes: re-setting the same
level, or the first run after an upgrade, wakes nobody.
`~/.cm/owner-availability-applied.json` records the last applied revision.

Out-of-band push (`notify_command`, e.g. Telegram, tag `owner-attention`):
every `emergency`; and, once Owner has set a level, every delivered `blocking`
request while no viewer is connected. While the level is unset nothing else
pushes, so behavior is unchanged until Owner first sets one. Pushes are
rate-limited to one per alert key per 10 minutes.

Daemon-side sources use the same gate through `owner_attention::escalate()` /
`withdraw()` (Operator RPCs `owner_attention.escalate` / `owner_attention.withdraw`
for evaluators elsewhere). The notification stall alarm (per session) and the
outbox alarm (one per host per episode: this host's messages still
`pending_sync` after 15 minutes, i.e. a stalled hub link) escalate as
`blocking`. **Behavior change once a level is set:** under `away` both alarms
are held until Owner becomes more reachable; under `on-call` or `around` with
no viewer connected they also reach `notify_command`. Escalations never
displace an agent's own pending request, and a released held request merges
into a pending alert without changing its ID or lowering its urgency.

## Routing and authorization

`notify_user` is in the MCP daemon routing set. Cloud sessions whose legacy
`CM_TUI_SOCKET` points at the daemon also work after the brain update. Older
local clients with only a TUI socket retain the legacy local handler. A pinned
daemon connection failure never silently falls back to another socket.

Publishing requires a known live Session caller. The daemon derives the label,
task, and continuous identity from its registry; target/identity parameters are
rejected. Acknowledgement uses operator-authenticated `owner_attention.ack`
with `session_uid` and `alert_id`. The ID comparison prevents an old delayed
acknowledgement from clearing a newer request. TUI acknowledgement RPCs and
desktop/file operations run on its background push worker, with acknowledgement
retries every five seconds after connection errors.

For a retained alert whose ordinary session row has disappeared, an operator
can inspect `~/.cm/owner-attention.json` on its host and acknowledge the exact ID:

```bash
scripts/cm-op --ssh cm-sessions owner_attention.ack \
  '{"session_uid":"<uid>","alert_id":"<id>"}'
```

## Rollout and verification

Deploy the complete MCP payload and rotate only the daemon brain on each cloud
host using [the split runbook](../HOWTO_HOLDER_BRAIN_SPLIT.md). Do not restart
the holder/service or agent sessions. Install the matching laptop TUI using
[the TUI release runbook](TUI_RELEASES.md), then reopen the viewer. Old viewers
ignore the additive alert stream fields; pending alerts wait for the update.
Existing cloud agents can keep their MCP connection because the tool signature
is unchanged; reconnect MCP to receive the updated usage guide.

For a live check, call `notify_user` from a cloud session and a continuous
orchestrator with an explicit test reason. Verify the saved alert and its stream
frame, then confirm the desktop popup and sidebar indicator in the updated TUI.
Select the row, verify the matching ID was removed on the host, and reconnect
the viewer to check that it does not reappear. A queued RPC result or a cloud
build alone does not establish laptop delivery.

Regression coverage is in `daemon/src/owner_attention.rs`,
`tui/src/owner_notification.rs`, `tui/src/app/events.rs`, and
`mcp_server/tests/test_socket_route_selection.py`. `scripts/test-owner-desktop.py`
checks the production desktop transport against a private D-Bus receiver; it
does not establish that a popup was visible on Owner's laptop. Run Rust tests only through
`scripts/cm-test-isolated` with a private `CARGO_TARGET_DIR`.
