# Native notification delivery research

Research recorded 2026-09-07. Delivery implementation is paused for Owner's review of the options. This note distinguishes installed capabilities and documented behavior from behavior still requiring an isolated test. No live sessions were prompted, resumed, interrupted, or migrated for this research.

## Owner's requirements

- Prefer client-managed notification delivery over terminal typing, paste delays, and synthesized Enter keys.
- A client-owned background command or pending MCP call may complete when a notification arrives, allowing the client to schedule its result.
- A completion-based listener may be one-shot. Owner accepts rearming after every completion and explicitly requires prominent instructions so agents do not forget.
- The notification subscription and unread messages remain durable in CM. A listener's completion does not mean its messages were read or handled.
- Evaluate delivery choices before implementing one. Channel membership, mentions, and Owner autocomplete remain the broader feature scope; additional custom-monitor features are deferred.

## Claude Code

The installed binary is 2.1.263. The [official changelog](https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md) records:

- 2.1.212: MCP calls running longer than two minutes automatically move to the background. `CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS` configures or disables this behavior.
- 2.1.234: background-task notifications delivered between turns are sent to the model inside `system-reminder` tags, matching active-turn delivery.

A pending notification-wait MCP call is therefore a candidate for client-managed delivery. The call itself must stay pending and become a client-owned background task. Spawning an invisible server-side subprocess and immediately returning is not equivalent.

Test the configurable backgrounding threshold, completion during active and idle turns, MCP timeouts, and reconnect behavior before relying on this route. The changelog is capability evidence; no behavioral prototype has run yet. CM's currently installed Stop-hook/terminal delivery remains unchanged.

## Codex

The research sub-agent inspected installed Codex 0.153.4, generated protocol schemas, live process ownership, official documentation, and read-only app-server RPCs.

| Route | Evidence | Still to establish |
|---|---|---|
| Native persistent queue | Installed CLI exposes `codex queue --thread <UUID-or-name> --message <text>`. Schemas expose `thread/queue/add`, `list`, `update`, `delete`, `reorder`, and `start`. Existing CM Codex processes open the shared queue database. | Automatic pickup and idle wake for an existing embedded TUI; behavior while working, editing a draft, or showing an approval; retry deduplication. |
| App-server control | `turn/start` starts a turn; `turn/steer` adds input to the expected active turn. The normal Codex terminal UI can attach using `--remote`. | How CM should own the server lifecycle and preserve each session's identity, MCP configuration, and provider settings. |
| Client-owned background wait | Background terminals and configurable MCP timeouts exist. | Claude-style MCP auto-backgrounding is not established. A completed process/UI event is not proof of an automatic model wake after an idle turn. |
| Hooks | Async hook results are delivered at a safe boundary during an active turn. | Official docs explicitly say completion does not start an idle turn; delivery waits for the next user turn. |

The native queue is the first candidate to test. `thread/queue/add` accepts a thread ID, input items, and optional `clientUserMessageId`, and returns an identified queued submission. That client ID must not be assumed to provide idempotency until verified.

CM's current Codex agents are embedded in their individual CLI processes. The separate shared app-server returned no loaded threads; a read of the live research target reported `notLoaded`. It could nevertheless list that thread's persistent queue without resuming it. This supports investigating cross-process queue delivery, but does not prove automatic consumption.

Connecting another server and resuming the same saved thread is not a live-attach strategy. Each live thread has a writer owner. An app-server architecture would need deliberate ownership and a supported UI attachment, not competing resumes. A server per CM session is a possible first prototype because it isolates each session's MCP identity.

Sources:

- [Codex App Server](https://developers.openai.com/codex/app-server/): structured turn control and official remote terminal UI.
- [Codex MCP](https://learn.chatgpt.com/docs/extend/mcp): default 60-second tool timeout and configuration.
- [Codex background hooks](https://learn.chatgpt.com/docs/hooks#how-background-hooks-run): active-turn checkpoints versus no automatic idle wake.
- Installed 0.153.4 CLI help/generated schemas and read-only IPC: queue APIs and current live-session ownership. Public queue documentation was not found; integration must be verified against the installed version.

## Required rearm contract for the proposed listener

`chat_wait` is a proposed tool, not an available MCP action. Its guide, tool description, and every completion result must explicitly say:

> This wait has completed and is no longer listening. If you still need notifications, start the next wait using the returned continuation cursor before returning to unrelated work or ending your turn. Stop rearming only when you intentionally stop listening.

The implementation must provide a durable continuation cursor. Notifications arriving between waits must be returned by the next wait, and a timeout must not advance past undelivered notifications. Reconnecting must recover the subscription and cursor; starting multiple overlapping listeners for one session should not be required. A wait receipt is separate from message-read acknowledgements.

Rearming this future one-shot delivery wait is distinct from recreating today's durable `chat_monitor(mode="continuous")` subscription. Existing continuous monitors already remain armed. Documentation must keep that distinction explicit.

## Proposed isolated comparison

Use disposable client state and a mock model endpoint, without touching live sessions. Test native Codex queue delivery first, then background shell/MCP completion. Observe both active and idle behavior, preserve a human draft and approval screen, retry delivery IDs, and verify one live thread owner. Prototype app-server plus its official remote TUI only if the smaller routes do not satisfy the delivery contract.
