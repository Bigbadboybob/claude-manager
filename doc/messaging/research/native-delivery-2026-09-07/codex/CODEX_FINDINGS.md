# Codex notification-delivery investigation

Date: 2026-09-07. Installed executable: `/home/lucas/.codex/packages/standalone/releases/0.153.4-x86_64-unknown-linux-musl/bin/codex`; reported version **codex-cli 0.153.4**.

## Decision-relevant findings

**Existing ordinary Codex TUI sessions can be woken without terminal input through `codex queue`.** This was demonstrated using a separate queue command against an already running ordinary embedded TUI, with an unsent human draft intact. Delivery uses a persistent cross-process queue. It is a next-turn queue, not active-turn steering. Cross-process idle pickup takes approximately a polling interval: two controlled samples were **9.347 s and 9.561 s**. Calling the queue API in the app-server that actually owns the thread delivered in **0.057 s** with the mock model. These are local test observations, not latency guarantees or real-model response times.

**The native app-server route supports the richer desired behavior.** Two independent protocol clients joined one live thread; the second steered an active turn during a three-second shell command. The command completed, the next model request contained the steering input, and no second thread was created. Closing the initiating client did not interrupt the agent. A new client subsequently rejoined the same history. Idle input started immediately. The official remote Codex TUI connected to that same app-server, displayed externally started turns, and preserved its unsent draft without submitting it.

**Ordinary background shell completion is not a Codex wake mechanism in the tested version.** A native `exec_command` process returned a session ID, then finished after the model ended its turn. Codex emitted UI/protocol completion events but made no model request. The result was not automatically added to subsequent model context. The same held with the actual ordinary TUI attached and when the process finished before an active turn's next model checkpoint.

**A long MCP call also did not gain Claude-like automatic background completion delivery.** A 130-second MCP call remained pending for 130 seconds when the code-mode wrapper was told to wait. No two-minute auto-background transition occurred. Explicit code-mode yielding did let the model continue while the MCP request ran, but completion neither woke an idle agent nor automatically injected the result at the next active checkpoint. The result remained retrievable explicitly.

**Async hooks provide automatic checkpoint context, but do not wake idle agents.** Both behaviors were verified and match the official documentation. A synchronous `Stop` hook can continue a turn at its stop boundary, but does not solve an event arriving later when already idle.

## Isolation and evidence standard

All behavioral tests used separate homes, Codex state directories, working directories, and loopback mock Responses API servers under this directory. The environment was constructed from a small allowlist; production CM, auth, and provider environment variables were not inherited. A custom provider used `requires_openai_auth = false`, with its base URL pointing exclusively to the fixture's local server. No production thread was queued, resumed, steered, interrupted, or modified. No paid model calls were made.

The checked-in `observations.json` contains sanitized measurements. Reproduction regenerates detailed case directories under `CM_CODEX_RESEARCH_ROOT` (default `/tmp/cm-codex-notification-research`). Full raw tool prompts/configs are intentionally not checked in. The mock supplies deterministic text and tool calls. `requests.json` records the actual inputs Codex submitted for each model request; this distinguishes a UI completion event from delivery to the model. `rpc-events.json`, websocket client event logs, raw terminal output, rendered terminal snapshots, and MCP fixture logs corroborate lifecycle events. Root-agent title generation also uses the mock and is identified separately in TUI tests; raw HTTP request count must not be confused with main-agent turn count.

All isolated processes were terminated afterward. A final `/proc` environment inventory found no processes with a `CODEX_HOME` under the research directory.

## Results and reproducible artifacts

Run scripts with `/usr/bin/python3`. They import `harness.py`; TUI fixtures require `pyte`; optionally set `CM_CODEX_RESEARCH_PYTE_PATH` to a separately installed package directory. Set `CM_CODEX_BIN` to choose the executable; otherwise the harness resolves `codex` on PATH. Every command launches only isolated fixture processes and a local mock server. For a pristine repeat, use fresh case directories while retaining the scripts. Tests write evidence inside their corresponding case directories.

| Experiment | Script / evidence directory | Observed result |
|---|---|---|
| Ordinary TUI, external native queue, unsent draft | `tui_fixture.py` / `queue-tui/` | `codex queue --thread UUID --message TEXT` exited 0. Queued message reached the same thread within the 15-second observation window. `HUMAN_DRAFT_DO_NOT_SEND` remained in the composer, absent from the model request until the fixture explicitly pressed Enter afterward. |
| Queue latency, local versus another process | `queue_latency.py` / `queue-latency/` | Same owning app-server: 0.057 s to mock completion. Separate app-server writing the same queue: 9.347 s and 9.561 s. |
| Queue during work and repeated client ID | `queue_behavior.py` / `queue-appserver/` and `queue-appserver-second/` | Input waited until the active turn completed; subsequent queued messages ran in order. Repeated `clientUserMessageId` created different queue IDs and duplicate model-visible user messages. |
| Queue while approval is open | `tui_approval.py` / `queue-tui-approval/` | An explicit harmless command approval remained open for 12 seconds after external enqueue. Only one main model request existed during the approval. Queue input did not approve, reject, or replace the dialog. After the fixture approved the command, the active turn completed and queued input ran. |
| Queue while client closed, then resume | `queue_reconnect.py` / `queue-tui-reconnect/` | External enqueue caused zero model requests during an 11-second closed-client interval. Resuming the existing thread consumed the pending queue, retained prior `INITIAL_HISTORY_MARKER`, and left zero queued rows. |
| Native app-server multi-client join, active steer, idle wake, reconnect | `ws_appserver.py` / `native-appserver-control/` | Second connection's `thread/resume` rejoined the same live ID; `thread/loaded/list` contained one thread. `turn/steer` returned the existing turn ID. Long shell command completed. Original-client disconnection did not cancel work. Idle `turn/start` completed in 0.055 s with mock model. Later reconnect preserved the same two-turn history. Stale steer returned `-32600: no active turn to steer`. |
| Native `turn/start` duplicate retry | `ws_appserver_retry.py` / `native-appserver-control-retry/` | Reusing `clientUserMessageId` after a completed request created another turn and another model request. There is no demonstrated transport-level idempotency. |
| Official remote TUI alongside controller | `ws_appserver_tui.py` / `native-appserver-remote-tui/` | `codex --remote unix://SOCKET resume THREAD` connected to the owning server. Controller-started input appeared in the TUI. `UNSENT_REMOTE_TUI_DRAFT` survived and was not sent to the model. |
| Shell completion, no TUI, idle and active | `background_shell.py` / `background-shell-idle/`, `background-shell-active/` | Native process finished with exit 0 and emitted `item/completed` plus output delta. No idle model wake; no automatic result delivery at the next active model checkpoint. |
| Shell completion with ordinary TUI | `tui_background.py` / `background-shell-tui/` | Twelve seconds after starting a three-second background process, only the original two main model requests existed: launch-tool request and final response. Human draft stayed intact. Completion did not wake the model. |
| Long MCP call held pending | `background_mcp.py` / `mcp-code-long-hold/` | Wrapper pragma `yield_time_ms:180000`, MCP server timeout 180 s, fixture returns after 130 s. First turn lasted 130.252 s; second model request arrived at +130.242 s. No automatic two-minute background transition. |
| Explicit code-mode yielding, idle | Historical run of `background_mcp.py` / `mcp-code-yielded/` | `functions.exec` yielded after 100 ms with a cell ID; MCP completed after four seconds. Model had ended its turn; no subsequent model request occurred. The checked-in script runs both the yielded and long-hold cases. |
| Explicit code-mode yielding, active | `mcp_yield_active.py` / `mcp-code-yielded-active/` | MCP completed before the next active checkpoint, but its content was absent from model inputs. After a later user turn explicitly read a stored result, it appeared in request 5. Explicit background execution is not automatic completion delivery. |
| Async PostToolUse hook | `hooks_behavior.py` / `hook-async-idle/`, `hook-async-active/` | Hook completed after three seconds. Active case received its additional context in the next model request. Idle case made no new request until a new user turn, at which point the context appeared. Fixtures explicitly bypassed hook trust only in isolated state. |

### Invalid or superseded cases

- **Exclude `mcp-direct-long/` from conclusions.** Setting `features.code_mode=false` did not expose a direct MCP tool in the tested configuration. The mock attempted to locate an unexposed tool, failed, and its retry returned a final response. This did not test a long MCP call. The valid replacement is `mcp-code-long-hold/`.
- The three-second initial observation in `queue-appserver/` showed no idle pickup, but **does not mean idle wake is unsupported**. Longer tests demonstrated approximately ten-second cross-process polling. The short observation is superseded by `queue-latency/` and the ordinary TUI case.
- The ordinary TUI issues a separate title-generation request. A raw total of three requests after setup plus queued input consists of setup, title, and notification, not duplicate notification delivery.
- UI `item/completed` events alone are not proof of model delivery. All negative completion conclusions compare actual subsequent model requests, not only terminal rendering.

The final `structured_input_variants.py` follow-up and its sanitized `native-structured-input` observation were added by the root researcher after the initial sub-agent report. Raw evidence is under `/tmp/cm-codex-structured-input/`.

## Native API contracts and practical integration

Generated schemas for the exact installed executable were inspected in the temporary research `schema/` directory (generated with `codex app-server generate-ts --experimental --out ...`); they are not duplicated in this repository. In particular:

- `thread/queue/add`: `threadId`, `input`, required `clientUserMessageId`; returns `queuedSubmission` with a queue ID.
- `thread/queue/list`, update, delete, reorder, and start also exist.
- `thread/queue/start` can request consumption by queued ID on the server owning the thread. The cross-process CLI does not magically acquire the other process's live thread.
- Delivered user-message items retain the originating ID as `clientId`; use it for reconciliation, not as an assumed deduplication guarantee.
- `turn/steer`: `threadId`, `expectedTurnId`, input, optional `clientUserMessageId`; appends to an existing turn and fails if its precondition no longer holds.
- `turn/start` can start idle work, and the schema notes it can steer an already active turn. Do not use a stale idle observation as a concurrency guarantee.
- `turn/interrupt` is a separate explicit cancellation operation; ordinary notifications need not call it.
- `thread/resume` rejoins a running thread **inside the same app-server**. From another server, it is not an attach transport. Live thread-writer locking exists; never attempt competing resumes as delivery.
- The root-agent follow-up also tested `turn/start` with standalone `toolOutput` and `thread/inject_items`. Tool output started an idle turn (~0.040 s through mock completion), remained `function_call_output` in model input, and reached the next active checkpoint without interrupting the foreground command. `thread/inject_items` alone did not wake the model; its context appeared on the next explicitly started turn. Both require the live owning app-server.

Before fixtures, read-only inspection established that production CM Codex processes were embedded TUIs with their own live thread writers. The separate shared local Codex daemon reported zero loaded threads; the current CM thread was `notLoaded` there despite running in its own process. Thus generic turn control of those existing embedded sessions requires a different ownership arrangement, while the **persistent native queue already reaches them**.

A future CM session can use a holder-owned Codex app-server plus the official remote terminal UI. CM becomes another protocol client. This avoids writing a custom Codex chat UI. A server per CM session is a simpler initial way to preserve CM's session-specific MCP credentials/environment and failure containment. A shared server may work but requires verified thread-local config and permissions handling. Keep that app-server outside the replaceable CM brain lifecycle; losing the UI/controller connection was tested, but surviving a CM holder upgrade or app-server process crash was not.

The fixture custom provider demonstrates that the app-server/TUI path can use provider configuration; production local-LB compatibility still needs a separately authorized integration check. Do not silently migrate live sessions while designing this.

## MCP, hooks, SDK, and notification capability limits

The MCP fixture captured Codex's initialize request using protocol `2025-06-18`, with capabilities **`elicitation: {form:{}, url:{}}`**. It did not advertise sampling or tasks. The tested tool calls contained no task/background execution request. No generic MCP auto-background or client-owned shell-job registration contract was established. This is evidence about the installed tested configuration, not a claim that every future Codex/client/server combination lacks these extensions.

An MCP server starting its own subprocess does not register a Codex background terminal. A pending MCP call can be wrapped in a yielded code-mode cell, but the model must subsequently use `functions.wait` or otherwise retrieve the result; the experiment shows completion is not itself an idle wake or automatic active-turn context injection. An unawaited promise in `functions.exec` is discarded when the isolate ends and should not be used as a watcher registration pattern.

Codex async command hooks deliver informational output at safe points and retain idle output for the next user turn. A hook can invoke an existing MCP tool through the documented `mcp_tool` handler, but that is still lifecycle-triggered execution, not a generic external wake channel. `notify` invokes an external command **from Codex**; it is outbound, not an inbound session-control socket.

The SDK is another client layer over local Codex execution/app-server. Official Python SDK documentation says it controls app-server over JSON-RPC and ships a pinned runtime dependency. It does not add a demonstrated attachment mechanism to an unrelated embedded live TUI. No SDK package installation was needed to test the underlying protocols.

One-shot waits should be rearmed after each delivered event batch. The durable CM cursor remains authoritative: return all events after the cursor, advance only after acknowledgement, and tolerate duplicate delivery. Native queue delivery does not require the model to keep a waiter armed; hooks or a proposed waiter can improve active-turn delivery separately.

## Recommendation

For existing sessions, **prototype CM's transport adapter around the existing native queue**, with exact thread UUIDs, delivery markers, a durable CM ledger, and explicit queued-versus-consumed-versus-handled states. Expect up to approximately the observed ten-second cross-process polling interval while idle and delivery after the active turn finishes. Reconcile queue contents and persisted `clientId` values before retrying after an ambiguous disconnect; neither queue add nor turn start deduplicates by client ID in these tests.

For immediate native steering and richer control, **app-server ownership plus the official remote TUI is now a demonstrated viable architecture**, with session-specific configuration and lifecycle integration still to implement. Async hooks are a useful independent path for automatic active-checkpoint context. Plain background shell completion and explicitly yielded MCP calls should not be chosen as a Codex idle wake transport based on the installed version's behavior.

## Official references

Official pages fetched during the investigation (temporary Markdown snapshots were retained outside the repository):

1. [Codex App Server](https://developers.openai.com/codex/app-server/) — transports, official remote TUI, thread/turn lifecycle, start/steer/interrupt, toolOutput, inject_items, and experimental transport status.
2. [Codex hooks](https://learn.chatgpt.com/docs/hooks) — PostToolUse, Stop continuation, async hooks, safe-point delivery, explicit no-idle-wake rule, and MCP-tool hook handlers.
3. [Codex MCP](https://learn.chatgpt.com/docs/extend/mcp) — configuration, default 60-second tool timeout, tool options, server instructions, and supported transports.
4. [Codex SDK](https://learn.chatgpt.com/docs/codex-sdk) — TypeScript and Python control surfaces and runtime dependency.
5. [Codex CLI commands](https://learn.chatgpt.com/docs/developer-commands?surface=cli) — ordinary versus queued interactive input, background terminal controls, resume, and remote-control commands.

The native queue API is underdocumented on the public app-server page; its availability/contracts here are supported by installed CLI help, generated version-specific schemas, and the isolated behavioral results. Full upstream source was not needed to infer undocumented behavior where it could instead be measured directly.
