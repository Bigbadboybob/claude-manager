# Fix: frozen remote session (branch `cm/fix-frozen-remote-session`)

## Symptom

A remote (`host=manager`) session freezes: it renders its pre-restart
scrollback, shows no `⟳` reconnecting indicator, is not marked exited, and is
dead to input — while local sessions and other remote orchestrators appear
fine. Onset: a remote `cm-daemon` restart (deploy) or a tunnel respawn.

## Investigation (live evidence, 2026-07-04/05)

Concrete case: session `ts-18beb58662bcb76d-6` ("BUG-013 negative-cash
investigate"), a continuous subtask on the `manager` host.

- **Daemon side is healthy.** `cm-daemon` on cm-manager restarted 00:44 UTC
  (journal: `restored 8/8 session(s)`). BUG-013 was restored and its agent is
  alive: `claude --resume 5e8fb07c-…` at its normal idle prompt. The SSH tunnel
  forwards to the live daemon (a raw probe gets a clean daemon-close).
- **The freeze is entirely client-side (TUI attach).** On the restart all 7
  remote attach streams hit transport-EOF and were correctly marked
  reconnecting + requeued (`cm-tui.log`, incl. BUG-013 twice). Then the reattach
  went silent — no give-up log, not marked exited on disk.
- **Smoking gun:** `cm-tui.log` has **133** `path_provider returned None
  (retrying … 30s)` events across the session, interleaved with
  `Connection reset by peer` on tunnel sockets. `/run/user/1000/cm-tui/` holds
  multiple stale `cm-host-manager-*.sock` generations (tunnel respawned several
  times over the session).
- The network/tunnel itself is fine: a manually-spawned TUI-style ssh tunnel
  connects in ~300ms and stays up 60s+. So the tunnel is not being killed by the
  network — the TUI tears working ones down and respawns (churn), and during each
  gap `path_provider`/`live_socket_path` return `None`.

## Root cause — re-warm starvation deadlock

Reattach recovery depends on a live tunnel socket path that **only the
off-thread watch consumers can (re)produce**, and the main-thread reattach drain
silently gates off — potentially forever — when that path is `None`:

1. `App::dispatch_deferred_remote_attaches` (the production drain) **skips every
   pending session** when `host_pool.live_socket_path(host).is_none()`
   (`app.rs:6014`).
2. `live_socket_path` → `ConnectionHandle::socket_path_nonblocking` **never
   respawns** a dead SSH tunnel (by design — it's the non-blocking main-thread
   probe; `host_pool.rs:458`).
3. The **only** code that respawns a dead tunnel is `ConnectionHandle::ensure_alive`
   (`host_pool.rs:506`), reachable only via `HostPool::for_host`, called only by
   the off-thread `manifest.watch` / `events.subscribe` / `workflow.watch`
   consumers — which **back off exponentially to 30s** on repeated failure.
4. So while the tunnel is between generations (down/respawning — which happens on
   *every* daemon restart and every tunnel churn), the main-thread drain is gated
   off and **cannot self-heal**. It waits on a 30s-throttled consumer to bring the
   tunnel back.
5. The attach **worker's** path (`try_attach_via_daemon_with_deps` → `for_host` →
   `ensure_alive`, `app.rs:993`) *can* respawn the tunnel — but it's only reached
   **after** the gated dispatch decides to dispatch, which it won't while
   `live_socket_path` is `None`. **Deadlock of responsibility:** the code that can
   re-warm is downstream of the gate that requires the tunnel already-warm.

Net: any remote session that loses its attach stream (every deploy, every tunnel
churn) can get stuck `reconnecting` indefinitely — frozen to the user. `A-r`
doesn't help because `nudge_remote_reconnects` only accelerates entries already
in the pending queue / revives `exited` slots; it can't re-warm the tunnel from
the main thread either.

## Slice plan

- **S1 — decouple reattach dispatch from the `live_socket_path` gate (the fix).**
  In `dispatch_deferred_remote_attaches`, stop skipping on `live_socket_path ==
  None`. Dispatch to the attach worker (still throttled per-uid); the worker's
  `for_host` → `ensure_alive` re-warms the tunnel **off-thread**, so the recovery
  path itself rebuilds a down/churning tunnel promptly instead of waiting on a
  30s-backed-off consumer. Distinguish **tunnel-down** failures (host/tunnel not
  reachable → keep reconnecting, do NOT burn the attempt budget) from
  **session-gone** failures (daemon says no such uid → burn budget → eventual
  `exited`). Thread a failure-kind back through `AttachResult`.

- **S2 — robust tunnel liveness (reduce churn + outage windows).** Guard
  `ensure_alive`'s dead-child teardown with a connect-probe so a still-live tunnel
  isn't torn down + respawned on a spurious `try_wait`; unlink the host's own
  stale `cm-host-<host>-*.sock` files on respawn so the runtime dir doesn't
  accumulate orphans. (Confirm the exact churn trigger during impl.)

- **S3 — detect a dead attach that yields no EOF (half-open / idle write path).**
  `transport_eof` only latches on a read EOF, so an idle session whose write path
  is dead never re-triggers reconnect. Add an attach-stream keepalive (or
  write-side error → synthesize `transport_eof`) so those self-heal. Defense in
  depth.

- **S4 — `A-r` force-reconnect for ALL stuck remote sessions.** Make
  `nudge_remote_reconnects` also force a fresh re-attach of remote sessions in the
  healthy-but-limbo state (bound slot, not reconnecting, not exited) and trigger a
  re-warm — so the user always has a reliable manual override.

S1 is the high-leverage fix that turns "stuck reconnecting forever" into reliable
recovery; S2–S4 harden around it.

## Status

- [x] **S1 — DONE.** Removed the `live_socket_path` gate in
  `dispatch_deferred_remote_attaches` (+ one-dispatch-per-host-per-tick bound so
  the worker re-warms the tunnel itself). Typed daemon RPC errors
  (`DaemonRpcError` carrying `ErrorCode`) + a classifier
  (`attach_failure_is_session_gone`) so the reattach distinguishes daemon-
  confirmed `NotFound` (SessionGone → give up after the cap → `exited`) from
  every other failure (TransportDown → retry indefinitely, never tear a live
  session down). Unified BOTH the production off-thread path
  (`drain_attach_results`) AND the inline synchronous path
  (`try_reattach_remote_session` now returns the failure kind;
  `drain_deferred_remote_reattach` branches on it) so tests validate the real
  semantics. Files: `client_session.rs`, `attach_worker.rs`, `app.rs`.
  Tests: 4 classifier unit tests + `transport_down_reattach_never_settles_to_exited`
  + `reconnect_settles_to_exited_after_session_gone` +
  `fresh_deferred_reattach_session_gone_retries_then_gives_up_bounded` (both drive
  a real in-proc daemon that returns `NotFound`). Full TUI suite: 665 pass.
- [ ] S2
- [ ] S3
- [ ] S4
