# On-demand brain memory snapshot

`daemon.memory_snapshot` is a strong-operator-only diagnostic, called directly
with `scripts/cm-op --timeout 5 daemon.memory_snapshot '{}'`. It takes no
parameters and has no MCP, TUI or API integration. Independent review and release
approval are required before the #56 diagnostic is used on a live brain.

The response identifies the brain PID, Linux `/proc/self/stat` start time in
clock ticks since boot (not seconds), holder epoch when split mode is present,
build ID and messaging daemon UUID when readable. `started_at`/`finished_at` are
UTC; total and section `elapsed_us` are monotonic elapsed microseconds. Sections
are sequential, not an atomic snapshot of the whole process. Neither the holder
nor engine/session child memory is measured.

On GNU/Linux, allocator fields map directly to glibc `mallinfo2`:

| Field | Source | Unit / meaning |
|---|---|---|
| `arena_bytes` | `arena` | Bytes obtained for non-mmapped arenas |
| `used_arena_bytes` | `uordblks` | Bytes marked allocated in those arenas |
| `free_arena_bytes` | `fordblks` | Bytes marked free in those arenas |
| `mmapped_bytes` | `hblkhd` | Bytes in separately mmapped allocations |

These are allocator accounting, **not RSS, logical object size, or a leak
verdict**. Arena used/free exclude the separately mmapped bytes. Tcache and
allocator bookkeeping affect what appears allocated. Arenas are visited under
their individual allocator locks, not one atomic global snapshot. Other libc
targets report `unsupported` with null data. No trimming is performed.

**There is no hard total-time bound.** `mallinfo2` can wait for internal glibc
arena locks and has no cancellation API. It executes on the existing RPC
handler, never on a detached diagnostic worker. A process-wide admission flag
remains held through the C call, collection and serialization, even if cm-op
times out or disconnects. Concurrent retries return `status=busy`,
`reason=snapshot_in_flight`, with unavailable data. A stuck call can occupy that
one handler indefinitely; later requests cannot spawn additional allocator
workers. Ordinary methods do not acquire this diagnostic flag. All allocation,
including JSON serialization, also remains subject to normal allocator/OS
scheduling; response delivery is not guaranteed within the client timeout.

The cooperative collection budget is 20 ms from capture entry, including time
spent in the allocator call. If it is exhausted, subsequent sections are
unavailable/partial. Registry and Store acquisition use `try_lock`, never wait,
and never overlap each other or the C call. Poisoned locks are unavailable.
Participant iteration checks the deadline before every entry and visits at most
4,096 entries. Preemption and the allocator call can exceed the budget; this is
not a real-time deadline. All locks are released before JSON serialization.

Messaging reports O(1) event length/capacity, shared request, replication
receipt/rejection and coverage scope/mark counts. It visits only participant
map entries and reads their operation/monitor map lengths, never the operations,
monitors, events or payloads themselves. `participants.total` and `visited` tell
how much was observed; `operations_visited_sum`/`monitors_visited_sum` are only
those visited entries, and are null when nothing could be visited in a nonempty
map. They are full totals only when `participants.complete=true`. Search active
and building bytes are existing **estimates**, not exact heap consumption;
warm conversation count and the local retired fallback Vec length are O(1).
The separate mpsc disposal queue has no safe depth accessor: its count is null
with an explicit reason. No hot-path instrumentation or queue policy changed.

Unavailable is not zero. `messaging_ready=false` means the Store slot was read
and was empty; null means readiness could not be established (busy, poisoned,
deadline or another diagnostic in flight). Store data/UUID are null when
unavailable. A populated Store may still have lazy search/backfill work; ready
does not assert steady state. Sections have independent status/reason/timing,
and top-level `partial` does not invalidate successful sections. This RPC does
not open/reconcile the Store, warm caches, scan transcripts, traverse heap/event
bodies, change retention or drop work. Existing callers and auth policy for
other methods are unchanged.
