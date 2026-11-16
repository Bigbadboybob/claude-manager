# Messaging allocation reductions

The #56 follow-up removes three unnecessary copies. It does not change retained
history or establish a fix for production RSS growth.

Monitor passes clone only active monitor candidates, collect changed candidates
in the original map order, and persist a borrowed participant checkpoint. All
other participant fields, including operation/results, preferences and receipt
histories, remain intact. No-change passes do not write. A scan or serialization
error commits nothing for that owner; earlier owners may already be committed.
The same file write, fsync, rename and directory-fsync sequence precedes the RAM
update. A failed write preserves old RAM and the existing degraded/uncertain
outcome behavior, including failures after rename. Borrowed serialization has
the same JSON data shape; object field ordering is not part of the contract.
Exhaustive Personal destructuring forces additions to be addressed in the view.

Coverage checkpoints overlay one staged mark on the existing map to encode the
same newest 256 marks in descending order. Insert/replacement and checkpoint
failure semantics are unchanged; the full in-memory history is retained. The
ingest durability barrier still precedes checkpoint persistence. Legacy
compaction also borrows the history instead of cloning it.

Closed-only checks read the fanout flag under the same mutex and recover poison
in the same way, without allocating a replay snapshot. Replay/cursors and
subscriber handling are unchanged. No allocator tuning, trimming, timer,
retention, queue dropping or notification policy is added.

Tests compare a frozen pre-change monitor loop and mark-copy path against the
new paths, using operation-heavy participant fixtures, expiry/overflow and
before/after-rename failures. A test-only System allocator wrapper measures
requested allocation bytes/calls on the calling test thread. These totals
include realloc requests, exclude other threads, and measure neither peak
memory, retained memory, resident pages nor production savings. No live probe
is part of this verification.
