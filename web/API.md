# Board web API (Batch 5)

The app shell and static files live at `/`; JSON endpoints live under `/api/`.
The server acts as **Owner** through the local daemon's operator-authenticated
socket using the daemon-issued scoped board token. No caller can choose a different actor or invoke arbitrary daemon RPCs.
Every route except `GET /healthz` requires a valid Cloudflare Access assertion.
Use same-origin requests. JSON writes require `Content-Type: application/json`
and `X-CM-Board: 1`; the server also rejects foreign Origin headers. No CORS.

Success is HTTP 200 with the JSON described below (daemon-shaped responses keep
their extra metadata). Failures are `{error:{code,message}}`: 403 authentication
or origin failure, 422 bad input, 409 conflicts, 503 daemon/push unavailable.
Do not automatically retry writes with a new request ID. Generate a UUID per
intended send/react/availability change and retain it for retries. A transport
timeout can mean that the daemon committed the operation.

## Directory and counts

| Method/path | Input | Result |
|---|---|---|
| `GET /api/bootstrap` | — | `{actor:"owner", user:{email}, features:{alerts_seen:true,push:true}, owner_availability:{…}}` |
| `GET /api/channels` | `query?`, `cursor?`, `limit?` (1–200, default 100) | `{items:[channel,…],next_cursor:object|null,…}` from `messaging.channels` |
| `GET /api/dms` | `cursor?`, `limit?` | `{items:[dm,…],next_cursor:object|null,…}` from `messaging.dms` |
| `GET /api/people` | `query?`, `cursor?`, `limit?` | `{items:[person,…],next_cursor:object|null,…}` from `messaging.people` |
| `GET /api/counts` | — | `{conversations:{id:{unread,mentions}},unread,dms,mentions}` |

Query-string cursors are `JSON.stringify(next_cursor)`, URL encoded. They are
opaque objects: never edit or synthesize them. Channel rows include `id`, `path`,
display metadata, `unread`, `mentions`, and membership metadata. DM rows include
`id`, `members`, `unread`, `mentions`. People include stable `id` and `name`.
Search/filter the loaded directory locally too. Sort mentions ahead of other
unread rows; the badge value is top-level `counts.unread` (DMs plus channel
mentions, already deduplicated), not the sum of every channel's unread count.

## Conversations

`POST /api/read` accepts:

```json
{"conversation":"conversation-uuid","newest_first":true,"limit":50,"mark_read":true}
```

Choose one target: `conversation`, `channel` (path), `dm` (participant ID),
`inbox:true`, or `dms:true`. Optional fields: `cursor` (opaque object), `thread`
(root message ID), `mentions_only`, `unread_only`, `newest_first`, `mark_read`.
The default order is newest first. Reverse the returned page for newest-at-bottom
display, preserving pagination cursors. Older-page requests send the same target
and filters plus `cursor:next_cursor` and `mark_read:false`.

Response is the daemon read shape:

```json
{
  "items":[{
    "id":"origin-uuid:event-uuid","type":"message.create",
    "conversation_id":"conversation-uuid","created_at":"2026-10-12T22:00:00Z",
    "actor":{"id":"agent:…","name":"Scout","kind":"agent"},
    "body":"Message text","read":false,
    "data":{"reply_to":null,"thread_root":null,"mention_recipients":["owner"]},
    "reactions":{"✅":{"count":1,"names":["Owner"],"mine":true}}
  }],
  "context":[],"next_cursor":null,"read_cursor":{"logical_time":"123","event_id":"…"},
  "receipt":{"actor":"owner","space_id":"…","ids":["…"]}
}
```

Only direct, unfiltered newest pages/end-of-history advance automatically.
Background refresh while scrolled back, inbox, mentions and thread reads are
previews. Use `mark_read:false` for those refreshes. Item `read` flags describe
the state before auto-advance; refresh counts after opening/marking. Render text
as text, not HTML. Unknown event types can be shown as informational rows.

`POST /api/send` takes one `conversation`/`channel`/`dm` target, `body` (1–3000
characters), `request_id`, and optionally `reply_to`, `mentions:[participant-id]`,
`mention_here:bool`. Result is `messaging.send` (`event`, `event_id`, replication
metadata). Replies use `reply_to` and the same conversation target. The daemon
resolves thread roots and authorizes membership. If it returns `join_required`,
`POST /api/channels/join {conversation,request_id}` joins that public channel.

`POST /api/mark-read` accepts one conversation target, optionally `through`
(message ID), or `{all:true}`, or `{mentions:true}`. A target plus
`mentions:true` clears that conversation through its newest mention. Omitted
`through` otherwise marks through its newest retained message. Result:
`{changed,counts,conversation_id?,read_cursor?}`. Clearing mentions also reads
earlier messages under the shared cursor semantics.

`POST /api/react {message_id,emoji,remove:false,request_id}` returns
`{event_id,message_id,reactions,counts,…}`. Allowed emoji: ✅ 👀 👍 ❌ 🎉.
Toggle with the aggregate's `mine` flag. Reactions do not notify; adding one on a
message mentioning Owner also advances the read cursor.

## Alerts and availability

`GET /api/alerts` returns `{items:[{key,id,session_uid,label,message,task_id,
continuous_task_id,urgency?,held_since?,released_at?},…],seen_supported:true}`.
Only delivered, pending Owner alerts appear. Core B5 is explicitly limited to
cm-manager alerts (`scope:"local", scope_label:"cm-manager alerts only"`);
B5b adds the approved cross-host relay and `features.alert_hosts` in bootstrap.
`key` is the daemon queue key.
`POST /api/alerts/seen {key,alert_id}` acknowledges only the matching alert ID,
returning `{ok:true,acknowledged:bool}`. A stale acknowledgement cannot clear a
newer alert. The list is read-only apart from this seen action.

`GET /api/availability` returns `{owner_availability:{level,set,changed_at,
age_s,previous?,owner_note?}}`. `POST /api/availability` takes
`{level,request_id,note?}` and returns that shape plus `event_id`.
Levels: `away`, `around`, `focused`, `on-call`, `unset`. The existing daemon policy
is authoritative: away = emergencies; around = decisions and above;
focused/on-call = everything; unset = legacy delivery. Notes are optional,
single-line, at most 200 characters.

## Web Push

- `GET /api/push/vapid-public-key` → `{public_key:"base64url",enabled:true}`.
- `POST /api/push/subscribe` takes the browser's `PushSubscription.toJSON()`
  object directly: `{endpoint,expirationTime:null,keys:{p256dh,auth}}`.
  Returns `{subscribed:true}`; repeat subscriptions update the same endpoint.
- `DELETE /api/push/subscribe {endpoint}` → `{removed:bool}`. No endpoint list or
  device secrets are returned to the browser.
- `POST /api/push/test {endpoint?}` → `{accepted:true}`. Optional endpoint must
  already be registered. Omit it to test all Owner devices. This explicit test
  ignores availability and is rate-limited. Acceptance does not prove iOS showed
  a notification; the device must confirm that separately.

Push requires a user gesture, iOS 16.4+, and a Home Screen installation. The
service worker receives exactly this application payload:

```json
{"title":"Scout · #channel","body":"First line, at most 140 characters…",
 "url":"/#conversation=conversation-uuid&message=origin%3Aevent",
 "tag":"cm-message:origin:event","badge":3}
```

Alert URLs are `/#alerts&alert=<encoded-id>`. Test URLs are `/`. All URLs are
same-origin relative links. Notification tags deduplicate retries. No full body
or thread history is included. Use `badge` with `navigator.setAppBadge` where
supported; also refresh the badge from `/api/counts` on app foreground.

The backend consumes daemon notification decisions; it does not poll full
message history or implement a second availability table. Vanished push
subscriptions (404/410) are removed. Temporary failures retry with bounded
per-endpoint backoff, honoring Retry-After; 400/403/413 are not retried. The app may refresh visible data on focus and every 30 seconds while
visible; stop refreshing while hidden. No frontend event stream is required in
v1. Access/session expiry should show a sign-in/reload action and preserve drafts.
