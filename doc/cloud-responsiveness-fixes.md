# Cloud responsiveness: first fixes

2026-09-07. Client changes only; the daemon, holder, and agent processes do not need restarting.

## Findings and changes

| Trigger | Previous behavior | Change |
|---|---|---|
| A socket fills partway through an input frame while the remote terminal is silent | Alacritty considers the input accepted; the buffered frame tail waits for another input/read event. The remote cannot echo a frame it has not received. Writable readiness can also spin without draining the tail. | An attach output worker drains on socket writability, sleeps when empty, and shuts down the failed transport on a hard write error. Raw input buffering is limited to one 64 KiB frame per write. |
| Many panes reconnect | The per-tick limit resets before the previous attach completes, filling a single worker's FIFO. Selecting a pane cannot reprioritize already queued work. | Four workers, a rendezvous channel with no hidden FIFO, at most two unfinished attaches per host, and selected-pane priority in the UI-owned pending list. |
| A host stalls during attachment | A single worker holds up every other host. | Concurrent workers keep other hosts progressing; an isolated two-host regression verifies this. |
| A pane is closed/reopened while attachment is in flight | A stale result can recreate the closed pane or overwrite a newer request. | Each reconnect carries a unique request identity. Cancellation clears pending/in-flight bookkeeping; late results and results for closed workspaces are discarded. |
| A tunnel changes while an attach is being constructed | A stale stream could be stamped with the replacement tunnel's generation. | Capture generation before attachment and reject the result if it changes during construction. |
| Local daemon discovery waits on a busy daemon | A local RPC can hold the UI loop for its five-second timeout, freezing remote interaction too. | Poll local session lists in the background and attach from the cache. Size reconciliation uses the session's asynchronous input channel. |
| Several expensive control requests arrive together | The UI drains the entire batch before returning to input. | Yield between requests after a 4 ms processing budget; log individual slow method names, without arguments or prompt bodies. An individual slow handler can still exceed the budget. |
| The TUI exits without destructors running | Its SSH forwarding process survives, leaving unused tunnels behind. | Arm Linux parent-death cleanup from a dedicated lifetime thread, keep SSH in the foreground, and clean up all fallible spawn paths. The lifetime thread matters because Linux tracks the spawning thread, not just the parent process. |

The previous input-drain comments recorded a decision to wait for empirical evidence before replacing opportunistic draining. The new real-Alacritty/socket smoke test supplied that evidence: before the fix a 64 KiB input stalled until the one-second read timeout; afterward it completed without any inbound output or follow-up input. A larger patterned payload checks framing, ordering, and byte preservation across several frames.

## Verification

- Full TUI suite after the implementation and cancellation/host-isolation checks: **875 passed**.
- Added larger-paste regression afterward; all three matching backpressure tests passed. The test executable now contains 876 tests.
- Kernel-level tunnel lifecycle tests cover abrupt client death and a short-lived calling thread returning while its tunnel remains alive.
- A 71-pane queue test runs 100 dispatch ticks without completing work, verifies the cross-tick bound, then switches focus and checks the next dispatch.
- A local-adoption test supplies a cached summary beside a nonresponsive listening socket and verifies that UI adoption never dials it.
- Builds use `/tmp/cm-cloud-responsiveness-target`, two Cargo jobs, and reduced process priority. They do not overwrite the shared runtime target during compilation.

## Live cleanup

Removed **28 unused orphan CM forwarding processes**. Each was checked for the current UID, parent PID 1, CM's private tunnel directory and filename, the expected forwarding arguments, and exactly one listening Unix socket with no accepted client streams. The checks were repeated before signalling through a pidfd; socket removal checked the original inode.

The active forwarding process was left alone and retained its 27 connected Unix streams. No daemon or agent was restarted. Post-cleanup health:

| Host | Holder epoch | Brain PID | Brain/holder session count | State |
|---|---:|---:|---:|---|
| Local | 15 | 3811719 | 28 / 28 | running, MCP preflight healthy |
| cm-manager | 12 | 2893373 | 35 / 35 | running, MCP preflight healthy |

These are snapshots; normal agent work can change session counts later.

## Activation and remaining work

Apply the changes with one TUI relaunch. Keep the current interface running until a convenient break so an unfinished prompt or ongoing interaction is not interrupted. Existing daemon-owned agents continue through the UI relaunch. Do not use a daemon/holder restart or `cm-redeploy --manager` for this client-only batch.

After activation, use the new `control:<method>` timing entries and existing phase timings to identify residual stalls. This pass does not eliminate every synchronous control handler, manifest fsync, or the transmission/parsing of hidden terminals. It also does not claim a measured production latency improvement before the updated TUI is running. The next architectural step remains lightweight fleet status with full terminal subscriptions limited to visible/recent panes.
