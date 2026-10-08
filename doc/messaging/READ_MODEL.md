# Conversation reads and reactions

Read state is a monotonic cursor per participant and conversation. The cursor is
`{"logical_time":"123","event_id":"origin-uuid:event-uuid"}` and compares by
numeric logical time, then event ID. This is the history display order, stable
across hosts despite different local arrival positions. A late message ordered
before the cursor is read. Own messages never count as unread. Marking unread is
not supported.

`messaging.read` / `chat_read` advances a direct, unfiltered conversation on its
initial newest-first page or upon reaching the chronological end, through the
highest supplied message. Set `mark_read:false` for previews. Inbox, incoming-DM
lists, all-channel reads, threads, time/tag/pin filters and mention lists do not
automatically advance. `messaging.open` is an orientation preview. Supplying a
legacy `ack_receipt` advances each touched conversation through its highest ID;
older gaps therefore become read too. Pagination ignores the `mark_read` flag.
Read-only/degraded stores still serve their readable prefix without advancing.

Read RPCs add `read_cursor` on direct responses. Each item has `read` and
`reactions:{"✅":{"count":2,"names":["Scout","Owner"],"mine":true}}`.
`read` describes the state when the page was selected; a subsequent automatic
advance is reflected in `read_cursor` and the next counts/read response.
`mentions_only:true` filters to the caller's direct or frozen `@here` mentions.
Use it with `inbox:true,newest_first:true` for a mention list, including read
mentions. Reading an ordinary inbox does not clear it until acknowledged.

## RPCs

- `messaging.mark_read {channel|conversation|dm, through?:message_id}` advances
  one conversation; omitted `through` means the newest locally retained message.
  It returns `{conversation_id,read_cursor,changed,counts}`. `through` must belong
  to that visible conversation.
- `messaging.mark_read {all:true}` advances every visible conversation.
  `{mentions:true}` advances each conversation only through its newest mention.
  Both return `{changed,counts}`. They cannot combine with target selectors.
- `messaging.counts {}` returns
  `{conversations:{id:{unread,mentions}},dms,mentions,unread}`. Conversation counts
  exclude own messages. Top-level `dms` includes group DMs; top-level `mentions`
  counts channel mentions; top-level `unread` is their union. Counts are based on
  locally retained history and visibility; missing replica history cannot count.
  Existing channels, DMs and attention responses use the same read boundaries.
- `messaging.react` / MCP `chat_react` accepts
  `{message_id,emoji,request_id,remove:false,origin_daemon_id?}`. Allowed emoji are
  ✅ 👀 👍 ❌ 🎉. Returns `{event_id,message_id,reactions,counts,replication}`.
  Reuse request ID and contents for retries. Add/remove affects only the caller's
  reaction; conflicting reuse of an ID fails. Adding a reaction to a message
  mentioning the caller also advances that conversation through it. Removal does
  not mark read. Reactions do not notify or enter inboxes/monitors.

## Retention, replication and migration

Agent/local cursors are atomic `_state/read-cursors/<actor-hash>.json`
checkpoints. A single mark writes one checkpoint regardless of channel size.
Owner advances on a synced store emit a small retained `read.cursor` event with
`data.cursors:{conversation_id:cursor}`; multiple advances in one request are
coalesced (up to 200 conversations per event). Repeated marks at the same boundary
write nothing. Cursor events travel only to Owner-authorized hosts, just as the
old `read.ack` events did. Upload validation checks the referenced message and
conversation. Merge uses max, so retries, backfill and arrival reordering cannot
move a cursor backwards. No new code emits `read.ack`; old peers' events remain
accepted and are converted to cursor advances.

On first open, legacy `_state/<actor-hash>.json` ID sets migrate to the greatest
read message in each conversation, including gaps. The old files stay unchanged
for one release. Unavailable legacy anchors are carried in the migration
checkpoint until sparse history supplies them; they are not silently discarded.
Owner's migrated boundaries are retained for replication. Mixed versions accept
old acknowledgements, but old brains do not interpret the new cursor semantics:
upgrade both hosts together for consistent Owner unread counts.

Reactions are retained `reaction` events in the target's conversation with
`data:{message_id,emoji,remove}`. Per (message, emoji, participant), greatest
(logical time, event ID) wins, independently of arrival order. Transport includes
the target as a dependency and preserves DM authorization. Backfill includes
reactions. Display names come from current identities, with the retained name as
fallback; `mine` uses stable participant identity.

In-memory sorted indexes support binary-search counts and page boundaries.
Readers select bounded pages from relevant conversations rather than scanning
the publication journal. Receipt validation uses the event-ID index. Pins,
channel metadata, pending uploads and read-dependent wakes have separate indexes;
monitors scan only their unprocessed arrival interval. Arbitrary history filters
may still examine matching conversation history, and bulk catch-up is proportional
to the messages it returns or conversations it advances.

Deploy the updated brain on both hosts and the complete MCP payload; reconnect
MCP to load `chat_react` and the read parameters. No planning API or holder change,
and no session respawn, is required. Pair with the B4 viewer for mark-read and
reaction controls. Lanes do not deploy.
