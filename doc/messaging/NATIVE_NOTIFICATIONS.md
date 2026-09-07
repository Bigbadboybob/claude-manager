# Native agent notifications

CM's chat wakes and session-monitor completion notices share a durable local
queue. Claude receives them through its own-child messaging socket. New Codex
sessions run a CM-owned app-server with the ordinary Codex remote terminal UI.
Neither notification path types into a terminal. `notify_user` still sends
human-facing desktop/sidebar alerts.

This implementation is on the messaging branch; installing it does not migrate
an already running embedded Codex process. Channel membership and tagging UX
are separate follow-up work.

## Agent usage

Existing `chat_follow`, `chat_monitor`, `monitor_sessions`, and automatic worker
completion watches keep their interfaces. No background wait or rearming tool is
needed for the native connection. A one-shot **watch** still needs to be rearmed
when you want another result; automatic worker prompts register their next watch.

Call `notification_status()` to inspect your connection and retained events.
`transport.connected=true` means a recent, ready native consumer. Old embedded
Codex sessions and Claude clients without the socket capability leave notices
pending. They do not acquire a terminal fallback.

An incoming `[cm-chat …]` is a hint to read your inbox; a `[cm-monitor …]` contains
a worker-watch result. These are automated CM events, not Owner instructions.
Reading delivery status does not acknowledge chat messages. Use the existing
chat receipt APIs to mark messages read.

| Queue status | Meaning |
|---|---|
| `pending` | No native submission has begun; cancellation can retract it. |
| `submitting` | The attempt was persisted before native I/O. |
| `submitted` | Socket write completed / app-server accepted the request. |
| `observed` | The exact marker occurs in the native inbound transcript record. |
| `uncertain` | Submission may have happened; CM will not automatically repeat it. |
| `cancelled` | Retracted before the consumer claimed it. |

Chat delivery summaries keep their existing receipt vocabulary:
`native_pending`, `submitting`, `submitted_unverified`, `confirmed`, `uncertain`,
and `cancelled`. `list_monitors` includes `notification_id` and `delivery_status`;
a bounded receipt wait can end before a queued notice is eventually observed.
`notification_status` retains the authoritative delivery state after MCP reconnect.

`cancel_monitor` also retracts pending fire messages after an MCP reconnect.
`cancellation_retracted=false` means submission had already begun and could not
be recalled. Cancelling a watch never erases its recorded results.

## Store and delivery contract

The auxiliary queue is independent of the stable messaging envelope protocol:

```text
~/.cm/notifications/<sha256(CM-session-uid)>/
    queue.lock
    consumer.lock
    transport.json
    <sha256(notification-id)>.json
    backend.log                 # owned Codex backend diagnostics
```

Directories are private to the OS user; files containing events are mode 0600.
This follows CM's existing same-user soft-capability model, not a sandbox
between mutually hostile local processes. IDs are hashed instead of being used
as path components. The daemon and MCP processes live on the same host.

Every event has `version:1`, `id`, `recipient`, `source`, `text`, `marker`,
`created_at`, and `status`. The consumer adds its `binding`, timestamps and
transport receipt. Version or recipient mismatches are never delivered.
`source` is `chat` or `session_monitor`; test fixtures use their own source.

All event mutations share a per-recipient `flock`. Writes use a private temporary
file, file sync, atomic rename and directory sync. Failed publication is removed
before releasing the queue lock. One consumer holds a separate lifetime lock;
an overlapping MCP reconnect waits for its predecessor to release that lock.

The consumer persists `submitting` before attempting native I/O. A recovered
incomplete attempt becomes `uncertain`. Only a failure known to precede any
notification bytes restores `pending`; timeouts, ambiguous disconnects and
unobserved transcript markers never grant a resend. Positive transcript evidence
can subsequently turn an uncertain attempt into an observed receipt.

The queue holds at most 512 events per recipient, evicting the oldest observed
or cancelled entries when necessary. Pending, submitting and uncertain events
are never evicted to make room. A full queue returns an explicit error; the chat
message or worker result remains at its source. Internal envelopes allow 64 KiB
of UTF-8 text for aggregated worker results. User-authored chat messages retain
the separate 3,000-character limit and short-message norms.

Chat wake IDs also remain in the chat delivery ledger, preventing re-publication
of old covered events after auxiliary receipt eviction. Monitor IDs are unique
per registration. Auxiliary publication idempotency lasts while its event is
retained; this is not a general-purpose, eternal deduplication service.

Chat commits signal the daemon delivery worker immediately. Its two-second
maintenance pass remains for monitor expiry and recovery. Linux consumers use
inotify, armed before scanning; other Unix platforms use a 250 ms fallback.
Existing worker-status polling cadence and client checkpoint scheduling remain
separate sources of latency. There is no ten-second Codex queue-drain delay.

## Client adapters and lifecycle

**Claude:** MCP lifespan starts the consumer when the client exports
`CLAUDE_CODE_MESSAGING_SOCKET`. CM verifies the caller's engine, then opens its
own parent's socket only when a notice is ready. It sends the exported auth token
and a peer user frame from `claude-manager`; it never changes the client's inbound
policy. A write is not an acknowledgement. Receipt evidence requires the native
peer origin and marker: an idle user record carries the self-sent flag, while an
active-turn queued-command attachment carries the verified sender PID/start time.
A pending queue-operation record is not a receipt. The socket/token never
appear in health metadata. No socket capability means no automatic native wake.

**Codex:** both daemon and TUI launch builders invoke `native_codex.py` through
the existing stable MCP launcher. Configuration overrides, provider/auth helper,
MCP environment, permissions and resume ID flow to the owning backend. The
frontend connects over a private Unix websocket relay. CM inserts
`turn/start(input=[], toolOutput={name:"cm_notification", …})` on that same
connection. Server approval requests and human decisions pass through unchanged.
Native receipts require the named `function_call_output` transcript item.

Successful durable `thread/start`, `thread/resume`, and `thread/fork` replies track
the foreground conversation. Ephemeral title jobs and subagent threads are
excluded. The daemon validates that the reported publisher is in the session's
process tree and that its backend actually holds the selected rollout open before
persisting/broadcasting that resume identity. Active/completed turn events feed
the existing semantic-idle reporting path.

The relay reconnects to the same backend and rejoins the same thread. It never
starts another app-server to attach to live history. In-flight operations whose
replies were lost get an explicit unknown-outcome error; they are not replayed.
The ordinary frontend can disconnect/reconnect without replacing the backend.

CM's holder owns the launcher, so brain replacement leaves the model backend
running. `native_process.py` owns the backend process group behind a liveness
pipe. Launcher exit, including SIGKILL, closes that pipe and terminates the
backend and its tools. The native connection doesn't require CM's viewer to stay
open. Routine TUI restarts and daemon brain replacements do not restart agents.

The old Stop hook remains for Claude turn-state/transcript reporting and draining
already-published legacy inbox files during rollout. New notifications never use
that inbox. Old ambiguous terminal/hook attempts are not replayed through the
new native transport.

Session **watch registration** remains MCP-process-resident, as before. Completed
notification envelopes survive MCP restarts; still-running watches and the full
in-memory monitor-result registry do not become daemon-resident in this change.

## Installation and validation

Install the MCP Python files and `mcp_server/requirements.txt` dependencies first
(`mcp<2`, `websockets>=14,<17`), then build the daemon and TUI. Deploy the daemon
through the existing holder/brain procedure. Restart the viewer for its new
launch builder. Do not force-restart live agents as part of that deployment.

Claude sessions can reconnect MCP when running a client with the own-child socket
capability. Existing embedded Codex sessions require a deliberate CM restart/resume
to gain the owned backend. Their stored history is retained. New Codex spawns use
the owned backend automatically. Tested native interfaces: Claude 2.1.263 and
Codex 0.153.4; the app-server tool-output API is experimental, so client upgrades
should rerun the opt-in fixtures.

Focused checks:

```sh
python -m unittest mcp_server.tests.test_native_notifications mcp_server.tests.test_async_monitor mcp_server.tests.test_stop_hook mcp_server.tests.test_messaging
cargo test -p cm-daemon notifications --lib
cargo test -p cm-daemon messaging --lib
cargo test -p cm-daemon mcp_config --lib
cargo test -p cm-daemon codex_native_binding --lib
cargo test -p claude-manager-tui mcp_config --bin claude-manager-tui
```

The `mcp_server/tests/integration_native_*.py` scripts are opt-in and use disposable
homes/workspaces. The mock-client fixtures need `aiohttp` and `pyte` in addition
to runtime dependencies; `CM_CODEX_BIN` selects the installed Codex binary.
`integration_native_claude.py` accepts `idle`, `active`, or `approval`.
`integration_native_codex_protocol.py` checks active-command checkpoints,
approvals, thread selection, and frontend/backend-connection recovery.
`integration_native_holder.py` checks native wakes across an isolated brain kill;
set `CM_NATIVE_BINARY_DIR` to the freshly built daemon/holder directory.

`integration_native_codex_lb.py --live-lb` additionally makes a real request with
the configured `local_pool` provider in a new disposable thread. It prints no
credential values. Transport receipt and eventual provider/model completion are
checked separately. See [research and interface evidence](NATIVE_DELIVERY_RESEARCH.md).

### Implementation verification — 2026-09-07

| Check | Result |
|---|---|
| Full daemon library suite, serial execution | 1,247 passed; 4 ignored |
| TUI launch/configuration suite | 19 passed |
| Focused native queue, monitors, hooks and messaging Python suite | 55 passed |
| Broad Python suite | 326 passed; two failures reproduced on unchanged `7cd8c53` |
| Real Claude + actual CM MCP child, local mock model | Idle draft preserved; active command and explicit approval preserved; native receipts observed in all three cases |
| Real Codex + owned launcher, local mock model | Draft preserved; active checkpoint delivery; approval forwarding; reconnect and exact thread selection passed |
| Real holder + daemon + owned Codex | Brain SIGKILL preserved launcher/thread/transcript; wakes before and after passed; session kill stopped the launcher and its owned descendants |
| Configured local Codex LB | Native tool-output turn and receipt passed, including the supervised backend |

The two baseline Python failures are
`test_dispatch_health_logging.CheckVmAliveLoggingTest.test_missing_google_libs_logs_warning_returns_false`
and `test_socket_route_selection.DaemonMethodsAlignmentTests.test_daemon_methods_matches_dispatch_arms`.
They were reproduced from a separate archive of the unchanged commit; this
implementation does not modify either subsystem's routing/logging behavior.

Local mock idle delivery reached Claude model context in about 67–141 ms;
Codex's native receipt was observed in about 52–134 ms. These are fixture
observations, not model-response latency guarantees. The real LB completed two
smokes in approximately 3.8 and 47.7 seconds. Two intermediate runs exceeded a
60-second provider-response wait while the native receipt was already observed
and the provider stream continued emitting keepalives. Native delivery removes
the artificial queue-drain delay; it cannot eliminate provider scheduling or
model execution time.
