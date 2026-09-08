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

## Second batch: attachment, background output, UI stalls, recovery

2026-09-08 UTC. This batch changes the TUI and daemon brain. It does not change
or restart the holder, and it adds no local prompt editor.

- **One-request attachment.** `attach.direct` authenticates the operator, applies
  terminal dimensions, optionally restores transcript/workflow metadata, and
  binds the same socket to terminal output. Restores include clearing stale
  workflow tags; partial tag pairs are refused by the existing setter without
  breaking attachment. Old clients retain the ticket path. New clients fall
  back only on `unknown_method`, never on authorization failure. Both direct
  and legacy attachment sockets now have handshake timeouts; an accepted but
  silent socket previously occupied an attach worker indefinitely.
- **Bounded, lossless output batching.** Network viewers have a 2 MiB queue;
  ordinary output and initial replay are framed in batches up to 64 KiB.
  Adjacent chunks can share a frame. Hidden panes batch small updates every
  250 ms; the focused pane and panes seen in the last two seconds remain
  immediate. Input temporarily forces immediate output, including programmatic
  input into hidden panes. Large output flushes at the batch-size threshold.
  The producer never waits for a viewer. Overflow produces an explicit stream
  error and reconnect, rather than silently dropping bytes into a live parser
  or accumulating unbounded memory. The old internal fanout subscriptions used
  by daemon consumers remain unchanged. RPC/stream framing writes the prefix
  and payload together, removing the separate four-byte write.
- **Measured UI stalls.** `slow-ticks.log` recorded a 5.964-second manifest drain
  and a 5.282-second `create_subtask` handler. Manifest adoption now uses the
  prioritized attach workers, with duplicate suppression while queued/in flight.
  Manifest drains yield after a four-millisecond budget between events.
  Daemon-confirmed exits remove/tombstone rows without a redundant kill RPC.
  Subtask creation snapshots caller scope on the UI thread, performs API/Git/
  daemon registration in a worker, then applies only the newly created rows.
  Completion preserves newer planning state and saves the manifest before the
  success response. The direct synchronous helper remains for internal callers
  and tests; normal control requests use the worker.
- **Recovery.** A server `Error` frame now delivers transport EOF to Alacritty,
  which notifies the UI and reconnects. Previously it returned a read error and
  Alacritty stopped without delivering an exit event, leaving a frozen pane.
  SSH keepalives are three seconds with two unanswered probes (roughly six
  seconds rather than fifteen). On Linux a background monitor watches default
  routes and wall-clock gaps after sleep; it retires only CM-owned SSH forwards
  and clears the old host push backoff. Local daemon transports are untouched.
  Existing reconnect tests verify queued prompts and session metadata survive.

Hidden output is batched, **not suppressed**: terminal escape sequences and
parser modes still need every byte. This reduces framing, wakeups and small
network writes, not the raw amount of terminal output. Full suspension would
need a validated terminal-state checkpoint/resume protocol. Overflow still uses
CM's existing bounded replay/repaint recovery, not an exact terminal snapshot.
Individual manifest fsyncs and other unconverted synchronous UI commands can
still take time; the per-phase timing remains available to identify them.
Neither batching nor a shorter handshake removes the physical network RTT.

### Verification and test isolation

The isolated full run passed **2,237 Rust tests** (880 TUI tests, 1,306 daemon
unit tests, and 51 integration tests); four existing Rust tests were ignored.
All **27 Python socket-routing tests** passed. Focused subtask tests also passed
after preserving the inherited workspace's host in the worker registration.
Checks cover authentication, old-daemon fallback through the correct host
socket, metadata restore/clear, output ordering, overflow isolation, focus
flushes, manifest adoption deduplication, and UI progress while a subtask waits.

The first full run exposed an existing fixture-isolation bug: an App test
restores `HOME` before later callbacks save its manifest, and wrote `ws-test`
into the real TUI manifest. The running TUI and daemon sessions were intact.
Reasserting an enrolled session's **unchanged name** generated a normal metadata
diff and made the live TUI repersist its full state: 2,444 workspaces / 38 session
rows at that instant. A private recovery copy was saved before further tests.

Use `scripts/cm-test-isolated` for subsequent Rust tests. It runs bubblewrap
with a read-only source/root, private writable CM/runtime state and temporary
files, a private PID namespace, and an isolated network namespace. Tests can
use loopback sockets but cannot reach live daemon sockets or external services.
It keeps the caller's HOME value, so the tests exercise the actual path logic
without writing to the user's directory. The Cargo target remains private:

```
CARGO_TARGET_DIR=/tmp/cm-cloud-responsiveness-target CARGO_BUILD_JOBS=2 \
CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 \
nice -n 10 scripts/cm-test-isolated
```

Rollout measurements and verified host epochs are recorded below after deployment.


### Final recovery check

A socket-backpressure regression reproduced another pre-existing silent-stream
failure: the daemon's outbound writer timed out and returned while its inbound
socket clone remained open. The TUI consequently received neither data nor EOF.
The daemon now shuts down both halves on an outbound write failure, and refuses
to leave a stream open if its writer thread cannot start. The test fills a Unix
socket's 4 KiB send buffer while the peer stays connected but unread; it failed
before the fix and passes after it. All 37 stream tests passed afterward.

Manifest-watch batches also coalesce saves: apply the queued changes, then
clone/serialize/fsync the complete manifest once before returning to input.
This avoids rewriting thousands of workspace rows once per name/session diff
in a reconnect burst. Control-request success acknowledgments retain their
synchronous save. The control budget also covers quickly rejected subtask calls.

### Rollout and measurements

Changes landed directly on `main` through the branch workflow: `a4e0c33`,
merge `67ffc4a` (including the owner's concurrent planning fix), and the final
recovery/persistence correction `578f7c0`. No PR was created.

Five alternating warm probes through the existing SSH tunnel measured median
attachment times of **199.1 ms for the legacy two-request path** and **76.6 ms
for direct attachment** on the updated daemon. Probes opened and immediately
closed read-only viewer connections without input, resize, metadata changes,
or session restarts. These small samples measure attachment setup, not ongoing
keystroke RTT. Earlier separate before/after samples measured 229.3 / 128.3 ms;
the alternating run reduces the effect of changing network conditions.

The first deployment reached local holder epoch 16 and cloud epoch 13 and
passed its full ten-minute stability check. The final socket-timeout correction
required a second short brain reconnect; final epochs are **17 locally** and
**14 on cm-manager**. Both final deploys preserved the holder PID and every
agent child PID/start-time pair: **27 local and 20 cloud agents**, with matching
brain/holder session counts and successful MCP preflights. Neither holder nor
any agent was restarted. The final ten-minute verification is recorded after
its observation window below.

Final release backups, binary hashes, before/after health and process snapshots:
`~/.cm/releases/cloud-responsiveness-578f7c0/`. The TUI is installed at the normal
shared release path; running interfaces keep the previous inode until relaunched.
Relaunch the TUI once at a convenient moment to activate the client changes.
Agents continue running independently. The final follow-up passed all **884 TUI
tests**, all **37 daemon stream tests**, and an additional control-worker check;
the preceding broader integration and Python checks are recorded above.

Final stability verification completed **2026-09-08 02:50:11 UTC**, after 602 seconds of
observation. Epochs stayed at 17 / 14 with no additional brain restarts,
`breaker_state=running`, successful MCP checks, and matching session counts
(27 local / 20 cloud) throughout. No required rollout checks remain.
