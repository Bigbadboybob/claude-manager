# S5 disconnected local spool: contract and limitations

This is an offline library, not an authorization hook, transport, worker, timer,
collector or production setting. Only an explicitly invoked caller operates it.
The event is the #53 fixed-size typed envelope; `Id` and every nested type now
have bounded, strict deserialization. Unknown facts are retained as unknown,
and a recorded claim or checksum never authenticates a process, a member, a
source, a successor or its receipt. A future trusted adapter and authenticated
custody transport must establish those facts independently. No ordinary API or
S4 wire type is changed.

## Disk and state machine

The caller supplies an existing absolute private (owner, mode 0700) directory
whose own creation and parent directory entry have already been made durable.
The implementation opens each path component using `openat`/`O_NOFOLLOW` and
never uses a caller-supplied filename. Its only owned files are `lock` and
`journal`: regular, single-link, caller-owned mode 0600 files, opened without
following a final symlink. The lock must have zero length and zero allocated
blocks, including on adoption; otherwise it is refused without modification. A nonblocking exclusive `flock` is held for the
handle's lifetime. This protects cooperating writers, not an adversarial process
with the same OS UID or a filesystem that violates POSIX locking. Deployment
must place the directory on a verified local filesystem with appropriate mount,
ACL and ownership controls; this library cannot authenticate a filesystem.

Creation uses `O_EXCL`, never overwrites an existing file, and reserves the
whole configurable, block-aligned data budget with Linux `fallocate(KEEP_SIZE)`
before enabling writes. Unsupported reservation or unexpected `st_blocks`
disables the spool. It syncs the lock and parent directory on both creation and adoption, so a
previous interrupted initialization cannot leave only a visible, unsynced name.
The journal is one immutable-header append-only file: capacity and control
reserve in its header, then bounded frames `(u32 length, 32-byte SHA-256,
canonical typed JSON)`. Every append validates the entire transition first,
writes the complete frame, and calls `sync_all` before returning success. The
same ID and same canonical event digest is idempotent; a different digest or a
reused sequence with a different ID conflicts. There is no temporary file,
rename, separate index, compaction, automatic deletion or replacement in this
slice; consequently there is no rename commit window. The file scan itself is
the bounded index. Checksums detect accidental damage; they are not MACs.

The journal persists opens, event frames, counter checkpoints, explicit gap
markers, individual event receipts and closes. The receipt API records a bounded
caller-supplied receipt ID against an exact event digest; it cannot itself prove
a durable successor. Acknowledged ranges are derived only from individually
recorded IDs, with holes preserved. **Nothing is removed**, even after a
receipt or close. Future local removal requires independently verified durable
successor custody *and* explicit review/retention release, with its own reviewed
crash-safe protocol. A timeout, receipt alone, close, or restart never supplies
that release.

An open is unique for the lifetime of the journal by `(producer, boot)`.
Reopening with an unclosed epoch makes its suffix unknown and fences further
event/checkpoint/close writes to that boot. A fresh boot marker is required;
opening it never clears the old uncertainty. Existing receipts and duplicate
retries can still be reconciled. An empty journal is `Unopened`, not evidence
of a clean zero. A system-level manifest is still required to detect a boot on
which this library was never opened. Events are accepted in strictly increasing sequence order; a checkpoint is
allowed only after all admitted events through its boundary have been flushed.
Its `attempted` must equal `next_sequence - 1`, its `admitted` must equal the
number of stored events, and `attempted = admitted + loss`. Consequently holes
cannot be clean; a later event below a checkpoint is refused. Checkpoints are
lower bounds; a clean close requires a matching final checkpoint,
all its admitted events present and individually acknowledged, and the caller's
assertion that its queue has drained. A recorded gap reports a cumulative journal-wide known loss lower
bound and/or an unknown suffix; retries do not add the same loss twice; overlapping checkpoint and gap counts are not
summed as if they were disjoint. If a gap cannot be persisted, or a write or
fsync returns an uncertain result, the in-memory spool becomes uncertain and
rejects further writes. Restart sees the unclosed epoch as unknown even if no
marker survived. No elapsed time turns uncertainty into agreement or readiness.

A failed event admission is returned to the caller and marks an unrecorded loss;
close cannot succeed until an explicit unknown-suffix gap is persisted; a
previously recorded cumulative loss bound cannot clear a new failure.
No logic claims that an event was never offered elsewhere or reissues an action.
A successful write can be followed by a lost response: after restart the caller
retries the same ID/digest; the scan identifies a duplicate and re-syncs before
returning success. A frame may be
visible after a crash without its original fsync having returned; the open
interval remains unknown and replay does not promote that into a durable remote
receipt.

## Recovery, bounds and deviation

Recovery verifies header, record length, checksum, strict typed decoding,
canonical representation, identity/digest uniqueness and transition rules.
Before reporting a complete recovered prefix or clean close, it successfully
syncs the journal again. Failed recovery sync yields an uncertain,
write-quarantined handle. Active epochs report open rather than clean, with
loss and unknown taking precedence. It replays only the verified prefix and never fabricates endpoint, actual,
execution or provenance. A short final header/payload is classified as a
**possible partial tail**, not proof of a clean prior disk or a power-loss
model; an impossible length, invalid checksum, malformed/noncanonical record
or invalid transition is corruption. A damaged length can make those classes
ambiguous. Both cases set an unknown suffix, expose prefix replay and quarantine
all writes. An invalid header prevents opening. No data is silently truncated.

This is a deliberate conservative deviation from the accepted contract's later
truncate-after-durable-gap repair: this slice cannot safely append a gap behind
a torn frame, and does not yet implement a separately reserved, crash-safe
control ledger and repair transaction. It therefore preserves both the damaged
bytes and preceding records, and requires a future independently reviewed
repair protocol. It does not claim recovery of the damaged suffix, exact loss,
or production readiness. Tests of close/reopen and deliberately damaged files
exercise real local I/O and fsync calls, but do not establish power-loss
behavior of every filesystem or storage device.

The inactive caller configuration caps reserved journal data at 64 MiB (or less),
with a nonzero control-byte reserve; the header and every frame count toward
the same total. Event frames cannot consume the control reserve. At most 4,096
records are scanned, with 256 record slots withheld from events for controls;
control traffic can still exhaust the reserve, which makes coverage uncertain.
Each encoded event is at most 4,096 bytes and each whole frame payload at most
8,192 bytes. Scan metadata, per-record parsing buffers and derived receipt
ranges are bounded by these limits; replay is a bounded linear scan. The
implementation checks preallocated file `st_blocks * 512` against capacity.
It owns two fixed directory entries/inodes and no temporary files or separate
index. This bounds the journal's allocated data blocks and object count, **not**
unmeasured filesystem directory/inode metadata, filesystem journal, snapshots,
copy-on-write amplification or physical backup usage. A later activation needs
an independently verified filesystem/quota and headroom probe for those costs;
preallocation or host free-space readings are not evidence of that quota.
Limits are unmeasured engineering ceilings, not a production capacity setting.

The current fixture plan covers real append/reopen, duplicate/conflict and
acknowledgment replay, distinct boot IDs, unknown unclean suffix, checksum damage,
partial trailing frames, full event/control reserves, failed gap recording,
strict bounded input, held data, lock and symlink/mode refusal. The later
integration must additionally exercise actual power interruption, fsync error
injection and filesystem-full behavior on the deployment filesystem, repair
transaction crashes, authenticated remote custody, review release, quotas and
S4/old-client provenance compatibility before any activation.
