# Native notification delivery: Claude Code and Codex

Investigated 2026-09-07 against Claude Code **2.1.263** and Codex **0.153.4**. This supersedes the earlier preliminary note. This is a research result and recommendation; CM's deployed delivery implementation has not changed.

**Both clients have ways to receive messages without terminal typing. My recommendation is a Claude adapter that posts from CM's MCP child process into that session's native inbox socket, and a Codex adapter that uses its native persistent queue.** Use an owned Codex app-server when lower latency or active-turn steering justifies changing session launch/ownership. Keep the CM message store, subscriptions, notification preferences, and receipts independent of these adapters.

The proposed one-shot background wait remains a useful Claude fallback, but it need not be our primary design. With the native adapters, an agent does **not** have to remember to rearm after every notification.

## What “notification works” means

These are separate capabilities:

1. A UI shows that a task completed.
2. The model receives the result at a checkpoint while already working.
3. A fully idle session starts a new model turn, without a person pressing Enter.
4. A message survives a disconnected client and is eventually handled.

We tested actual subsequent model requests, not just terminal output or task-status events. A client-owned completion notification can satisfy the first three without satisfying the fourth. A successful socket write, MCP notification, or native queue insertion is not proof that the agent read the CM inbox.

The disposable clients used separate configuration directories and local deterministic model endpoints. No real model inference was purchased; no live CM session was prompted, resumed, migrated, interrupted, or killed. Simulated typing/approval was confined to disposable terminals. Provider/account-gated features that could not run in that setup are explicitly marked as documentation evidence, not successful behavioral tests.

## Comparison

“Idle wake” means a new model request without another human submission. Latencies below exclude real inference and cloud message synchronization.

| Mechanism | Active-turn delivery | Idle wake | Agent rearms after each hit? | Main tradeoff |
|---|---|---|---|---|
| **Claude native session socket, via own MCP child** | Between tool calls; no interruption of a running tool | **Yes, reproduced** | No | Best fit here; pin/test the small socket adapter and honor inbound policy |
| Claude MCP push **channels** | Queued; busy events can be grouped | Yes, documented | No | Purpose-built, but preview, allowlist, account/provider, feature and protocol gates |
| Claude **Monitor** command / plugin monitor | Each output line becomes an event | Documented autonomous reactions | No while process lives | Closest to the requested persistent watcher; unavailable with several providers or telemetry disabled |
| Claude **background Bash** | Completion delivered at a checkpoint | **Yes, reproduced** | Yes | Broadly usable; completion carries an output-file reference, not necessarily the message body |
| Claude **long MCP call, auto-backgrounded** | Result delivered at a checkpoint | **Yes, reproduced** | Yes | Clean MCP-only waiter; two-minute default foreground threshold and call timeouts |
| Claude **asyncRewake hook**, exit 2 | Client-scheduled continuation | **Yes, reproduced** | Hook/listener must be started again | No agent tool needed to arm a startup hook; uses failure-style feedback and enforces a timeout |
| Claude ordinary async hook | Next model turn/checkpoint | **No, reproduced** | Hook lifecycle | Context injection only while idle; cannot replace the wake mechanism |
| Either client's synchronous PostToolUse/Stop hook | At that hook's boundary | No independent wake | Not per notification | Useful fallback/drain point; Stop can continue a turn that is ending |
| Claude SDK / stream-JSON input | Structured queued input; host controls interruption | **Yes, reproduced in one owned process** | No | Full host control; a different launch/UI architecture from the current interactive CLI |
| **Codex native persistent queue** | After the current turn | **Yes, reproduced** | No | Works with current embedded TUI; cross-process pickup about 10 seconds; no retry-ID dedupe |
| Codex owned app-server, `turn/start` / `turn/steer` / `toolOutput` | Start an idle turn or steer the expected active one | **Yes, reproduced** | No | Fast and explicit; CM must control the actual server that owns the live thread |
| Codex background shell | Result available to `write_stdin`; completion can appear in UI | **No automatic model wake in tests** | Explicit result retrieval/wait | Terminal completion is not a notification delivered to the model |
| Codex yielded code-mode MCP call | Background execution possible; explicit `functions.wait` retrieves it | **No automatic model wake in tests** | Yes, plus retrieve completion | Useful concurrency, but not Claude's automatic task-result delivery |
| Codex async hook | Next safe active-turn model request | **No, documented and reproduced** | Hook lifecycle | Good active context injection; idle results wait for another turn |
| Native cron/scheduled prompts | Usually between turns | Yes on supported clients | Schedule lifecycle | Polling, delay and unnecessary model turns; useful recovery checks, not primary event delivery |
| Ordinary MCP progress/log/resource/list changes | UI, discovery, or data refresh | No generic wake contract | N/A | Notification support at the protocol level does not imply a model conversation event |

## Claude: native inbox socket

Current [cross-session messaging documentation][claude-peer] explicitly describes a per-session socket for **scripts and hooks to post into their own session**, in addition to Claude's built-in `ListAgents` and `SendMessage` tools.

Claude exports `CLAUDE_CODE_MESSAGING_SOCKET` and `CLAUDE_CODE_MESSAGING_TOKEN`. We verified that a stdio MCP subprocess inherited both. A persistent receiver in that MCP process can subscribe to CM events and post to its own Claude session whenever there is unread activity. The socket route itself does not require a waiting tool call, a model to stay active, or a completion/rearm cycle.

The installed client's diagnostic supplies this line-delimited JSON input shape:

```json
{"type":"auth","token":"<this session's exported token>"}
{"type":"user","from":"claude-manager","message":{"role":"user","content":"[cm-chat delivery-id] Automated CM notification. Read your CM inbox; this is not Owner input."}}
```

The socket and authentication are documented; the exact user-frame example above is grounded in the installed client's diagnostics and our reproduced tests, rather than a separately versioned public JSON schema. Keep it behind a version-tested adapter. Never copy tokens into messages or logs.

On Linux/macOS the authentication line is optional for connecting; it matters for verifying an own-child sender when process evidence is unavailable. On Windows it is required. Use the exported token consistently, connect only when a message is ready, and use the current session's endpoint. An incomplete connection times out after 30 seconds. Do not locate the recipient by a possibly colliding display name or a stale socket filename alone.

### Reproduced behavior

- Two independent events sent by the MCP child after the parent had gone idle each caused a model request in **36 and 46 ms**. The model had not called an arming tool.
- The typed draft was absent from those requests and could subsequently be submitted intact.
- With a foreground tool deliberately blocked, the notification waited for that tool to finish.
- With an explicit Bash approval pending, the notification did not start another model request or answer the dialog. After the simulated operator approved, the agent received it.
- A normal external sender into a bypass-permissions session was held under default inbound policy. Setting `crossSessionInbound: refuse` suppressed delivery. The own-child bridge worked without setting broad `accept`.
- Closing the sender connection and opening a fresh connection for another message worked.
- An immediate duplicate with the same body was suppressed, but a changed body with the same `msg_id` was delivered. This is **not** a durable idempotency guarantee.
- The model saw an explicit native explanation that the message came from another session, not its user, and could not approve pending permission requests.

### Pros and cons

**Pros:** fast local push; no terminal input; no rearming; existing interactive UI/history stays in place; native checkpoint, draft and approval handling; works on all documented providers in sufficiently recent versions, including with feature-flag fetching off (v2.1.248+ for that availability).

**Cons:** CM must maintain a small adapter for the installed input framing and endpoint lifecycle; connection success is not a processed receipt; inbound controls, burst limits, deduplication and queue limits can hold/drop events; no delivery to an exited client. Claude documents at most 50 accepted queued peer messages and a separate held queue. Send coalesced CM hints rather than one wake per chat message.

**Implementation implication:** let the existing session-scoped CM MCP process bridge durable daemon events to **its own** socket. Its own-child credentials avoid weakening inbound defaults for arbitrary other processes. If explicit inbound policy refuses or holds messages, report that state and respect it; do not silently switch transports to defeat it. On MCP reconnect or client restart, bind the current endpoint and replay unacknowledged CM hints. No token or endpoint needs to become part of the shared chat protocol.

Built-in `SendMessage(..., notify_when_idle=...)` is also worth knowing about: Claude now supports one-shot local notices when another Claude session goes idle/exits. It is useful Claude-to-Claude orchestration, but it does not cover Codex, CM channel membership or arbitrary chat events. Using another model session simply to send a wake would add cost and a dependency we do not need.

## Claude: MCP channels and persistent Monitor

These are two distinct native push features, not names for CM chat channels.

### MCP push channels

The official [channels guide][claude-channels] and [wire reference][claude-channels-ref] describe precisely the desired server-to-session event path:

```json
// Server initialize capabilities
{"experimental":{"claude/channel":{}}}

// Server notification
{"jsonrpc":"2.0","method":"notifications/claude/channel","params":{"content":"[cm-chat delivery-id] Read your CM inbox.","meta":{"delivery_id":"delivery-id"}}}
```

Events enter the existing conversation, queue in order, and can be grouped when Claude is busy. Tools such as `chat_read` and `chat_send` remain ordinary MCP tools. No wait tool or rearm is needed. Permission relay is a separate optional capability; a CM chat bridge does not need it.

The restrictions are material:

- Research preview and gradual rollout; syntax/protocol may change.
- Anthropic authentication (claude.ai or Console API key), excluding Bedrock, Google's provider and Foundry. Organization policy may require enablement.
- Session opt-in at launch. Ordinary `.mcp.json` registration is insufficient. Approved plugins use `--channels`; a custom bare server currently uses `--dangerously-load-development-channels server:claude-manager` and the preview consent flow, or an organization's approved internal plugin route.
- The development flag bypasses a plugin allowlist, not organization policy or every availability gate.
- The channel path does not work if the server negotiates MCP revision **2026-07-28**. The docs recommend retaining the legacy negotiation path for a channel server.
- `notification()` completing means bytes were written, not that Claude consumed them. Disabled/unregistered channels can silently drop events.

**Test outcome:** our server connected, exposed tools, and sent a valid channel event, but the isolated client with feature fetching disabled reported **“Channels are not currently available.”** No wake occurred. This was an availability-gate result; it does not contradict the documented behavior on eligible accounts. We did not alter a real account or bypass the gate to claim a positive test.

**Assessment:** the cleanest explicitly named MCP push extension. Worth supporting as an optional adapter on eligible installations; less dependable as CM's one universal Claude path than the tested socket.

### Monitor and plugin monitors

Claude's [Monitor tool][claude-tools] runs a persistent command and delivers **each stdout line** as an event; it can also watch WebSocket messages. This is the user's “always-running command that tells the agent when something happens” idea almost exactly. A CM helper could block on the daemon's event stream and print only coalesced notifications. It would not need to exit per event.

[Plugin monitors][claude-plugins] can start that process automatically for the session. `monitors/monitors.json` or `experimental.monitors` declares its name, command and description. The name prevents duplicate processes on plugin reload. This removes the agent's need to invoke Monitor initially.

**Pros:** event-driven; minimal model overhead while quiet; persistent; native task visibility/cancellation; automatic startup is possible.

**Cons:** Monitor is unavailable on several third-party providers and when `DISABLE_TELEMETRY` or `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` is set. It was absent from our isolated client's exposed tools. Plugin monitors share those gates, work only in interactive CLI sessions, and have an experimental manifest contract. Disabling a plugin does not itself stop a monitor already running. Use the tool's `persistent` option for a long-lived listener; otherwise its timeout ends the watch. Process failure/session exit still needs recovery. The WebSocket variant rejects private/local network addresses, so it cannot simply connect to a loopback CM WebSocket; the command form is the practical local adapter.

**Evidence level:** current official documentation and exposed-tool absence. Positive event-delivery behavior on an eligible installation was not tested. Do not confuse an ordinary background `tail -f` with Monitor: a normal Bash task's stdout does not get this per-line routing.

## Claude: the one-shot background-wait approach

### Background Bash

Start a real **client-owned** Bash task with `run_in_background: true`. Its command blocks on CM until a notification is ready, then prints a concise receipt/hint and exits. Claude continues its work and receives a native task-completion event.

This worked while fully idle (**115 ms** from fixture release to next model request), preserved an unsent draft, and deferred delivery until a foreground tool finished. A successful exit code of **0** was enough: no artificial failure was necessary.

The native completion message contained the task ID, status, exit code and **output-file path**, not the command's full notification payload. The agent must read that output or use `chat_read` to get messages. A helper's description should make the required inbox read obvious. A background process invisibly spawned by an MCP server is not a client-owned Bash task and does not acquire these semantics.

**Pros:** direct fit for the user's proposed design; no new socket protocol; explicit task status/cancellation; low trigger latency once armed.

**Cons:** agent must invoke the shell task and rearm it; output often requires another read; timeout/cancellation/crash can end the listener; it cannot survive exiting the owning client. A plain stdout line while the command is still running is not the completion event.

### Long MCP call with automatic backgrounding

Claude [documents automatic backgrounding][claude-mcp] for a main-conversation MCP call still pending after **two minutes** (v2.1.212+). It returns a background task ID to the model, keeps the original request running, and later delivers its result in a native task notification.

`CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS` changes the threshold; `0` disables it. Our **700 ms** test threshold worked. After the foreground turn ended, completing the MCP request woke Claude in **42 ms**, included the actual MCP result in the model request, and left the human draft untouched. Another test delivered it only after an outstanding Bash approval was answered. Active foreground-tool delivery also waited for that tool to finish.

The server must actually **leave the MCP call pending**. Returning `{"armed":true}` immediately and continuing invisibly in the server is a different mechanism and cannot make the client background a completed call.

Limits worth designing around:

- Only main-conversation calls; not subagent or IDE-server calls.
- Non-interactive calls do not auto-background unless `CLAUDE_AUTO_BACKGROUND_TASKS=1` is enabled; even then, host/exit lifecycle matters.
- An open MCP elicitation dialog postpones backgrounding.
- Per-server `timeout` / `MCP_TOOL_TIMEOUT` remains a hard wall-clock limit. Progress does not extend it.
- An idle timeout also applies: documented defaults are 30 minutes for stdio, five minutes for HTTP/SSE/WebSocket/connectors, with per-server overrides and `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT` controls. Progress can keep the idle timer alive, not the hard deadline.
- Tasks do not survive client exit. A server disconnect in our test settled the background task with a connection error and woke the model; a timeout similarly delivered an error, **not a successful notification receipt**.
- Lowering the background threshold is a process-level setting affecting other MCP calls too. It is not a per-tool opt-in.

**Pros:** a natural `chat_wait` MCP interface; the result itself reaches the model; no shell helper command required for each wait.

**Cons:** initial foreground delay unless configured; timeout/rearm handling; main-conversation and lifecycle restrictions; cannot be assumed portable to Codex.

### Async hooks: an important distinction

A normal `async: true` hook does **not** wake idle Claude. Our hook completed after the turn ended, and its `additionalContext` arrived only after the next genuine user submission.

The documented [`asyncRewake: true`][claude-hooks] variation **does** wake idle Claude when the command exits **2**. A SessionStart hook blocked on our fixture event, exited 2 with a short stderr message, and caused a new model request in **58 ms** while preserving the draft. It appeared as native background/Stop-hook feedback with a clear “not user input” reminder.

**Pros:** CM can arm it through startup configuration without the model remembering a first tool call; local and fast.

**Cons:** one completion per hook process; timeout remains enforced; uses blocking-error-style feedback; naïvely installing a waiter on every hook firing can create duplicate processes. A restart/rearm strategy is still needed. It is a fallback, not a reason to replace the simpler persistent socket bridge.

Synchronous `PostToolUse` and `Stop` hooks are useful inbox-drain points. They cannot cause a fully idle session to run by themselves. CM already uses a Stop hook for some current delivery; moving the same hook to another process does not magically give it an idle wake.

## Codex: native queue and owned app-server

### Native persistent queue in current sessions

The installed CLI exposes:

```text
codex queue --thread <exact-thread-UUID> --message "[cm-chat delivery-id] Automated CM notification; read your inbox. This is not Owner input."
```

Generated app-server schemas also expose `thread/queue/add`, `list`, `update`, `delete`, `reorder`, and `start`. Queue insertion takes a `threadId`, input items and a required `clientUserMessageId`, and returns an identified queued submission. Public app-server documentation is the reference for server ownership/control; CLI help and the generated schema are the version-specific evidence for these queue APIs.

A separate process can add to the same persistent queue without loading or resuming the receiving live thread. **This worked with the ordinary embedded TUI.** The existing client picked up the queued message, started a turn and retained an unsent draft. A pending approval remained pending until the fixture operator answered. A queued message also survived the receiving client being closed and was drained after a controlled resume of the disposable thread.

Measured insertion-to-mock-turn-completion latency was about **9.35–9.56 seconds** across two app-server processes, versus **57 ms** when submitted inside the owning server. The ordinary TUI test also observed a several-second pickup. Treat cross-process delivery as roughly a 10-second polling path, not instant IPC or a cloud round trip.

**There is no `clientUserMessageId` deduplication guarantee.** Repeating an add with the same client ID created distinct submissions. Blindly retrying a timed-out insert can generate multiple agent turns. Store the returned queue ID, reconcile native queue/history when an outcome is ambiguous, and keep the durable CM notification ID in the body. Prefer one outstanding coalesced hint to one queue entry per chat message.

**Pros:** smallest useful change for current CM Codex sessions; persistent queue; no PTY text/Enter synthesis; preserves drafts and native approval handling; no agent rearming.

**Cons:** cross-process polling latency; waits for the current turn rather than providing active-turn steering; queue contents are native user submissions, so clearly distinguish CM events from human instructions; insertion alone is not inbox acknowledgement; exact thread identity, queue location and runtime version must be known.

### Owning the actual app-server

The official [Codex App Server documentation][codex-server] provides structured `turn/start` for an idle thread and `turn/steer` with `expectedTurnId` for an active one. `turn/interrupt` is a separate operation; sending a notification does not require interrupting the user or the agent.

The sub-agent reproduced native start, steering and reconnect against one owned app-server, and attached the official remote TUI with an unsent draft preserved. A second protocol client could reconnect to the same loaded thread on that server. Idle start through mock-turn completion was about **55 ms** in the fixture. Stale steering was rejected rather than silently becoming another turn. Retrying `turn/start` with the same `clientUserMessageId` created a second turn; that API also needs reconciliation after an ambiguous outcome.

The server also supports a particularly suitable variant: **`turn/start` with standalone `toolOutput`** and empty `input`. CM can supply the output of its notification receiver as tool output instead of a new human-shaped prompt. A final isolated test woke an idle thread (~40 ms through mock completion), preserved `function_call_output` in the actual model request, and delivered another event at the next active checkpoint without cancelling a foreground command:

```json
{"method":"turn/start","params":{"threadId":"<live thread>","input":[],"toolOutput":{"name":"cm_notification","namespace":null,"output":"[cm-chat delivery-id] Read your CM inbox."}}}
```

This is **host-delivered tool output**, not automatic backgrounding of an MCP call. It requires CM's control connection to the owning server. It is the most natural notification form for the future owned-server adapter; use native queue/start/steer for the other scheduling/control cases.

`thread/inject_items` is a separate context-only API. Our test appended an item without waking the idle model, then saw it on the next explicitly started turn. It can preload context but does not solve idle notification delivery by itself.

The crucial distinction is **connecting to the actual live server**. Current CM Codex sessions run their own embedded CLI servers. A different shared app-server did not own those live threads. Starting or resuming the saved thread in another server is not an attachment to the first and risks competing writers.

For new sessions CM could launch one app-server per session, attach the normal terminal UI using the documented `codex --remote unix://...` route, and connect a CM control client to that same server. Preserve session-specific MCP identity, provider/LB settings, permissions, history and holder lifecycle. An owned-server prototype is a deployment/launch architecture change, not merely a new MCP tool.

**Pros:** low latency; explicit busy/idle control; native input scheduling and approval surfaces; direct queue operations can also be fast when they reach the owning server.

**Cons:** more invasive than queue insertion; connection and event ownership must be correct; approval routing and UI reconnect need validation; a crash between accepting input and persisting CM receipt still needs reconciliation. Do not migrate existing live sessions as a shortcut.

## Codex: does background MCP work like Claude?

**It can run an MCP wait in the background through code mode, but the tested completion behavior is different.**

In the installed client, `functions.exec` can yield an execution cell while the MCP request continues, and `functions.wait` can retrieve completion. The fixture confirmed this is real background execution. However, finishing that call did **not** start an idle model turn, and its result was not automatically included at the next active checkpoint in the tested path. Explicit waiting/result retrieval was required.

A deliberately non-yielding MCP call ran for **130 seconds** before returning its result. It did not automatically turn into Claude's two-minute background task notification. The [MCP configuration docs][codex-mcp] also document a default **60-second** tool timeout, configurable with `tool_timeout_sec`; an event listener must account for this deadline.

The same caution applies to background shells. `exec_command` can return a running session ID; `/ps`, task status and command-completion events can show the process finished. Our tests did not see an idle model wake or unsolicited delivery of its stdout at the next active checkpoint. `write_stdin` retrieved the result.

Codex [async hooks][codex-hooks] are more useful for active context injection: completion waits for the running model/tool call and can enter the next request. Both documentation and tests agree that when Codex is idle, that output waits for the next turn. There is no evidenced equivalent of Claude's `asyncRewake` idle-wake behavior here.

**Practical consequence:** a foreground long poll, a yielded cell followed by `functions.wait`, or a waiting subagent can keep an agent waiting for an event without busy polling. They are usable task-coordination tools. They are not a demonstrated replacement for an idle-session wake through native queue/control. Capabilities of the Codex app or another hosting runtime should not be inferred solely from the stock CLI's tools, or vice versa.

## Other paths considered

| Path | Why it is or is not useful for CM |
|---|---|
| **Claude SDK streaming / stream-JSON stdin** | [Documented structured multi-turn input][claude-sdk]. Our disposable `-p --input-format stream-json --output-format stream-json` process accepted a second message after its first final result while stdin remained open (~12 ms to the next model request). Requires CM to own that host/input stream; it is not a way to attach stdin to today's interactive process. SDK interruptions are optional, not necessary for each message. |
| **Codex SDK / `exec resume`** | Useful for CM-owned headless workers. Starting another process on a saved thread does not acquire control of an already running interactive writer. Use the live owning app-server for that. |
| **Claude background agent view / attach** | Native lifecycle/UI management for background sessions; their ordinary native inbox is the useful delivery mechanism. The installed management CLI did not expose a general local message-send subcommand. No benefit in replacing CM's whole UI just to send events. |
| **Claude Remote Control / cloud sessions** | Native remote input and cross-machine peer messaging exist, but introduce account/cloud dependencies and availability limits. Appropriate for supported human remote control or Claude-to-Claude coordination. CM-local events do not need to leave the machine. |
| **Subagents, agent teams, workflows** | Built-in mailboxes/completion events work inside their own client-managed lifecycle. A helper agent could wait then report, but adds another agent and depends on its host's parent-delivery semantics. Existing independent CM sessions are not automatically team members. Directly writing private team inbox files would depend on internals and bypass the durable CM protocol. |
| **Claude CronCreate, ScheduleWakeup, `/loop`** | Native scheduled prompts can run while idle; ordinary cron has minute granularity and jitter. They poll, can incur model turns without messages, and have session/expiry limits. A useful missed-event recovery check, not the fastest delivery path. [Scheduling reference][claude-schedule]. |
| **Codex scheduling / app automations / external cron** | A scheduler decides *when* to act; it still needs a supported target-session delivery mechanism. A new scheduled task/thread is not an event injected into an existing CM conversation. External cron calling the native queue is possible, but event-triggered enqueue is simpler. |
| **MCP progress and logging** | Useful for tool status and keepalives. In our Claude fixture they caused neither idle wake nor log/progress text in the next model request. They are not a generic chat push API. |
| **MCP resources/subscriptions and list-changed notifications** | Refresh discoverable data/tools; can make inbox state available when the model next reads it. They do not establish automatic context injection or an idle wake. Our Claude fixture refreshed its tools after `tools/list_changed` without creating another model request. |
| **MCP Tasks / server-side background jobs** | Persisting or backgrounding server work does not by itself define how the client schedules a conversation. Our negotiated Claude connection advertised roots and elicitation, not sampling/tasks; a synthetic task-status event did not wake it. The Codex fixture negotiated `2025-06-18` with elicitation only, and likewise advertised no sampling/tasks capabilities. Do not assume the newest protocol revision has the same behavior as a client's task UI. |
| **MCP sampling** | Asks the client for model generation, not necessarily a turn in the existing conversation. The Claude fixture did not advertise it and returned `Method not found` to `sampling/createMessage`. Even if another host supports sampling, it needs an explicit main-session integration. |
| **MCP elicitation / permission relay** | Requests information/approval from a person or supported remote approval surface. It is not an agent-inbox notification and must not be repurposed to approve work or steal focus. Claude's open elicitation can actually defer MCP auto-backgrounding. |
| **Tool-result decoration / synchronous hooks** | Can attach an unread hint after an unrelated tool call; cannot wake idle sessions. Touching every CM tool's result introduces unrelated failure coupling. A dedicated adapter plus a small recovery checkpoint is preferable. |
| **SessionStart / file watchers / inotify / daemon event streams** | Good places to register or trigger a watcher. Detecting an event is separate from delivering it; connect the watcher to a native socket/queue, Monitor, or client-owned wait. File edits alone do not cause a model turn. |
| **Outbound `notify` hooks, terminal bells, OS notifications** | Alert the human/application or report a completed turn. They run in the wrong direction to inject new agent context. Owner's existing CM `notify_user` remains the human-attention tool. |
| **Signals, raw transcript edits, private queue/SQLite writes, IDE-internal sockets** | No supported general inbound contract from these surfaces. Signals stop/control processes, and transcript files are not live input streams. Use native CLI/RPC entry points and documented session sockets; do not patch live history or internal databases to manufacture delivery. |
| **Patching/forking the clients or proxying the model API** | Technically could insert context or add a queue wake hint, but adds version drift and difficult state/approval semantics. An API proxy still cannot wake a session that is making no request. Existing native surfaces make this unnecessary for the first implementation. |
| **PTY input, tmux send-keys, synthesized Enter** | Excluded by Owner's requested direction. Keeping an implicit PTY fallback would reintroduce the draft collisions and timing failures this work is meant to remove. |

## Recommended CM contract

Separate durable **event detection**, native **wake delivery**, and **message handling**:

```text
CM message store + membership/mentions
                 │
       durable notification inbox
                 │
     one coalesced unacknowledged hint
                 │
       per-engine delivery adapter
           ┌─────┴─────────────┐
  Claude own-child socket   Codex native queue
           └─────┬─────────────┘
       agent reads CM inbox
                 │
    acknowledge notification receipt
```

1. Keep subscriptions and unread notifications in CM's durable local store. Native processes/queues are delivery aids, not the canonical message board.
2. Send a short hint with a stable delivery ID and a tool to fetch the messages. Do not put a whole channel backlog into native input.
3. Track `pending`, native transport acceptance, and agent acknowledgement separately. Only a defined CM receipt should release the durable pending event. Message-read state and notification delivery receipts remain separate.
4. Coalesce events while busy/disconnected. Avoid one wake per message and unbounded agent-to-agent wake loops. Preserve native human queue order and pending approvals.
5. On restart/reconnect, reconcile pending receipts before retrying. Treat duplicate delivery as possible; a token/cursor and native queue ID help recover but do not establish exactly-once processing.
6. Enforce CM mutes, DND, notification preferences and membership before enqueueing. Explicit native inbound refusal/hold must also remain effective. Do not bypass those settings through a fallback.
7. Discover capabilities per running client: engine/version, exact thread or own-session socket, endpoint/config generation, and adapter health. “Installed CLI is new enough” is insufficient if a session still runs an older binary.
8. Expose adapter state in CM: listening, waiting for client, pending, held/disabled, native queue accepted, last acknowledged. A tool returning “monitor armed” must not imply successful future wake delivery.

This architecture remains local-first. Native socket/queue delivery needs no cloud round trip. Future cross-machine sync can populate the recipient daemon's durable inbox and use the same adapters. Do not couple the transport decision to implementing multi-host sync now.

### If we retain `chat_wait`

`chat_wait` is **proposed**, not an available MCP tool. It should be an optional transport/helper primitive, not the only way to receive DMs or tags.

Every successful completion, timeout, cancellation and connection failure must make the listener lifecycle clear:

> This wait has finished and is no longer listening. If you still need notifications, start the next wait with the returned continuation cursor before returning to unrelated work or ending your turn. Stop rearming only when you intentionally stop listening.

Use an opaque durable cursor/subscription, not “now” or the listener's start timestamp. An event arriving between completion and rearm must be returned by the next wait. A timeout must not skip unseen events. Reconnect must recover from the last acknowledged cursor. Starting a new wait should never acknowledge messages merely because it replaced an old listener.

For Claude, document both `Bash(run_in_background=true)` and genuinely pending MCP calls, including thresholds/timeouts and output-file reads. For Codex, explicitly document the necessary yielded-cell/session result retrieval and that ending the turn is not proven to preserve automatic wake behavior. Do not present a single cross-client recipe with false parity.

Today's durable `chat_monitor(mode="continuous")` subscriptions already stay armed until cancellation/expiry. **Do not tell agents to recreate those after each message.** Rearming a future one-shot delivery wait and managing a durable subscription are different operations.

## Recommendation and remaining decision

**Build the two small native adapters first:** Claude's own-child socket bridge and Codex's persistent queue. They fit existing interactive sessions and remove CM terminal prompting from the notification path. Keep a documented Claude background-wait fallback for clients without the native socket; make unsupported/disabled delivery visible instead of secretly typing into the terminal.

If **roughly 10 seconds is too slow for Codex**, use an owned app-server plus the normal remote TUI for newly launched sessions. That is the supported way to get fast dispatch and active-turn steering, including the tested standalone `toolOutput` notification form. It should be a separate launch/lifecycle milestone with history, MCP identity, permissions and reconnect checks before rollout.

MCP channels and plugin Monitor are attractive optional Claude integrations. Their current gates make them poor prerequisites for every CM session. The background-command idea is valid on Claude; the premise of identical Codex completion behavior did not survive testing.

This survey resolves the delivery choices; it is not a production-readiness claim. The remaining implementation work is durable CM adapter integration, reconciliation and bounded retry, actual session capability registration, and rollout validation against the user's live provider/UI configuration. No live-session experiment or deployment is necessary to review this recommendation.

## Evidence and reproduction

[Research fixtures and results](research/native-delivery-2026-09-07/README.md) contain the isolated scripts, sanitized observations and Codex-specific findings. Detailed raw disposable requests/debug traces remain under `/tmp/cm-native-delivery-research/` and `/tmp/cm-codex-notification-research/` (structured-input follow-up: `/tmp/cm-codex-structured-input/`); they are not required to use the committed report.

Measurements are individual controlled local samples, not percentile benchmarks or latency guarantees. Claude timings measure trigger/send to the next main-conversation model HTTP request. Codex queue/control timings measure through completion of the deterministic mock turn. These endpoints differ, so the millisecond figures are not a cross-client performance ranking. Real model/network latency is not represented. No real provider account, hosted Remote Control, gated Monitor/channel positive path, Windows/macOS client, or cross-machine delivery was exercised. Negative no-wake results are bounded observations supported by documented behavior where available, not a proof about every future client version.

[claude-sdk]: https://code.claude.com/docs/en/agent-sdk/streaming-vs-single-mode
[claude-headless]: https://code.claude.com/docs/en/headless
[claude-peer]: https://code.claude.com/docs/en/cross-session-messaging
[claude-channels]: https://code.claude.com/docs/en/channels
[claude-channels-ref]: https://code.claude.com/docs/en/channels-reference
[claude-tools]: https://code.claude.com/docs/en/tools-reference#monitor-tool
[claude-plugins]: https://code.claude.com/docs/en/plugins-reference#monitors
[claude-mcp]: https://code.claude.com/docs/en/mcp#automatic-backgrounding-of-long-tool-calls
[claude-hooks]: https://code.claude.com/docs/en/hooks#run-hooks-in-the-background
[claude-schedule]: https://code.claude.com/docs/en/scheduled-tasks
[codex-server]: https://developers.openai.com/codex/app-server/
[codex-mcp]: https://learn.chatgpt.com/docs/extend/mcp
[codex-hooks]: https://learn.chatgpt.com/docs/hooks#how-background-hooks-run
