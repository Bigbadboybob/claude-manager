# Brain allocator maintenance

The brain runs one dedicated allocator maintenance thread. On GNU libc it
checks allocator-reported free arena bytes and may call `malloc_trim(0)`.
These are the operator-approved defaults, enabled even when `[memory]` is absent:

```toml
[memory]
trim_interval_secs = 600
trim_min_free_bytes = 1073741824
```

Both fields are hot-reloadable through the existing `daemon.reload_config`
operator route or SIGHUP. `trim_interval_secs = 0` disables future **periodic**
admissions. It does not cancel a C call already admitted, remove the worker, or
disable explicit operator requests. Unknown `[memory]` keys are rejected;
intervals are whole seconds in `0..=4294967295`, with checked clock arithmetic.
There is no minimum positive interval. These defaults are a policy choice,
not measured reclaim or pause guarantees. No arena cap is installed.

The first periodic check runs after a full interval, never at startup. Interval
changes/re-enable start a fresh interval; unrelated or threshold-only reloads
preserve the existing due time. Every wake rechecks the current schedule under
the same short mutex used to publish config changes. Busy slots are skipped;
missed slots never accumulate. After an attempt the next slot is a full interval
from completion, except a future deadline from a concurrent interval change is
preserved. If that changed deadline has already passed, completion starts a new
full interval. There is no catch-up burst. A threshold change affects the next
admitted check, not a check already admitted under the previous configuration.

Periodic checks, explicit trim attempts and `daemon.memory_snapshot` share one
process-wide admission flag. No daemon, Store or trim-control mutex is held
across `mallinfo2`, `malloc_trim`, `/proc` reads or logging. The worker never
takes the daemon/Store lock. The reload path publishes under daemon-state then
trim-control, so concurrent reloads cannot reorder the applied policy.

**No hard latency bound:** glibc takes allocator locks, which can block this
thread and other allocating threads indefinitely. A client timeout cannot
cancel the C call. It also cannot release the worker's admission or spawn a
replacement thread. While an allocator operation is stuck, other attempts
return busy; normal RPCs do not acquire this admission flag, but their ordinary
allocations can still wait on glibc. A dedicated thread is not latency isolation.

Each actual periodic check/manual run logs a `cm-daemon: memory_trim` JSON record
after collecting its measurements and publishing any receipt. It contains
PID/start ticks/holder epoch/build, source, UTC start/end, monotonic elapsed
microseconds, threshold decision, before/after allocator accounting and
`VmRSS`/`VmSwap` from bounded `/proc/self/status` reads, in **bytes** (kernel kB
values multiplied by 1024). Allocator and process reads have separate timings;
the trim call has `trim_elapsed_us` and its raw return code. Unsupported/missing
metrics are marked with status/reason and null values. A skipped trim has null
return, trim duration and after metrics; it did not make an after measurement.
`complete` means the call returned, not that all metrics were available or that
a useful number of bytes was reclaimed. Sections are sequential, not atomic.

`fordblks` measures allocator-reported free space, **not reclaimable bytes**.
It may remain high after successful `MADV_DONTNEED`, so the same threshold can
permit repeated calls at successive intervals. Return 1 does not measure bytes
reclaimed. Assess physical effects with same-process RSS **and swap**, workload,
and call duration; neither a counter delta nor a single return code proves a
leak is fixed. This feature changes neither retained histories nor queue policy.

## Explicit operator attempt

`daemon.memory_trim` is available only with a configured strong operator token,
directly through `scripts/cm-op`. There is no MCP, TUI or API route. Obtain the
current PID, Linux start ticks and holder epoch through existing operator
readback (`daemon.memory_snapshot`); then deliberately bind the attempt:

```sh
scripts/cm-op --timeout 5 daemon.memory_trim '{"request_id":"maintenance-1","brain_pid":123,"process_start_ticks":456,"holder_epoch":7}'
```

Those are illustrative identities; do not copy them for a live call. In
monolith mode explicitly supply `"holder_epoch":null`. Unknown fields are
refused. `request_id` is 1–128 ASCII letters/digits or `-_:.`.
The frame ID is independent. Explicit requests bypass the periodic threshold
and remain available with interval 0. An already accepted manual job still runs
exactly once if periodic work is then disabled.

At most **one explicit attempt per brain epoch** is retained in memory. Auth,
strict parameters and identity are checked before admission. A busy response
has `accepted:false`, consumes no attempt and can be retried. Once admitted,
the handler returns a bounded receipt with `accepted:true` and `attempt.status`
`pending` or `running`; it does not wait for C. Retry **exactly the same params**
to read its `complete` receipt and result, even after timeout/disconnect. A
different request is a conflict for the rest of that epoch. A caught worker
panic records `unknown`, never silently retries. Missing worker/process identity
returns unavailable, without consuming the attempt. Allocation/OS failure can
still terminate the process; a receipt is not durable across a brain restart.
Changed PID/start/epoch refuses an old request rather than repeating it. A new
attempt after restart requires deliberately binding that new identity.

The ordinary memory snapshot wire shape, cooperative budget and read-only
behavior are unchanged. Its legacy `snapshot_in_flight` busy reason now also
covers admission held by a trim check or pending manual job. A snapshot itself
never trims. Unsupported libc targets report unsupported, with no trim call;
periodic checks can log that condition each interval. See [snapshot accounting
and limits](memory-snapshot.md).
