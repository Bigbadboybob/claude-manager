# Milestone B — complete single-host messaging

B builds on the accepted A file store and Owner Messages panel. It adds four MCP tools and their Owner controls. Cloud replication and continuous-task conversation continuity are milestone C; the local protocol remains v1.

## Agents

* `chat_norms`: read, actual textual diff, attributed history, publish and revert. Read all pages, then explicitly pass `ack_revision` to acknowledge a supplied revision. Publish/revert uses `expected_revision`, `summary`, and a stable `request_id`. A `status: conflict` response contains the competing revision and diff; retain the draft. Revert creates a new revision. Norms never block a send, and change indicators appear only in messaging responses.
* `chat_monitor`: register once/continuous watches on a channel (optionally descendants), a peer DM including first contact, all incoming DMs, or a thread. No default expiry; 64 active watches per participant. Register before asking, or use a previous read/send's scope-neutral `position` as `after` for catch-up. Only new message creates match; own posts are excluded by default.
* `chat_monitors`: list/get results, acknowledge returned result pages in order, cancel/cancel all, and dismiss. Results are durable before a wake is attempted. Result acknowledgement does not mark messages read. Cancel retracts work that has not been submitted; dismiss retains an idempotency tombstone.
* `chat_follow`: inspect inherited preferences, set/remove own scope overrides, global DND, and Owner's optional bell. More-specific inbox/wake preferences override inherited values. Any matching hard mute or DND suppresses automatic and monitor wakes. New follows start now; changing a mute preserves an existing follow. Omitted fields retain existing values.

Normal messages remain at most 3,000 Unicode characters, usually one to three short paragraphs or less. A single sentence is often enough. Reference a file for lengthy details. Tags remain passive; Owner contact norms and the separate urgent `notify_user` path remain unchanged.

## Owner

Open Messages with **Alt+m** (F8 fallback). **Alt+Shift+m** controls mouse capture. **j/k** moves within the focused pane, **Tab** changes panes. The accepted color palette and focused outline remain.

| View | Controls |
| --- | --- |
| Any conversation | `W` creates a monitor for this channel/DM/thread; `f` opens its preferences |
| Shared norms | `d` diff since acknowledgement; `h` attributed history; `g` current text; `p` edit/resume draft; Enter acknowledges after the complete text/diff has been displayed |
| Norms editor | Enter newline; Ctrl+s enters summary and then diff preview; Ctrl+s in preview publishes; Esc keeps the draft |
| Norms conflict | Review competing changes; `b` rebases the saved draft onto the current revision; review before publishing again |
| Norms history | j/k selects; Enter reads; `v` previews a revert as a new revision; `]` next history page |
| Saved norms draft | `D` archives its text to a local file and releases the draft for a different edit/revert |
| Monitors | `n` new; Enter results; `a` acknowledges the displayed result page; `x` cancel; `D` dismiss; `X` cancel all; `b` list; `]` next page |
| Monitor result | Enter opens the message's thread; message reads and monitor acknowledgement remain separate |
| Preferences | `e` set scope override; Enter inspects selected rule and inheritance; `x` removes override; `D` DND; `B` optional bell |
| Unresolved operation | `R` retries the persisted request through its original daemon |

Monitor/follow scopes accept `#channel`, `#channel/**`, `@Exact Person`, `dms`, or `thread:message-id`. Monitor forms expose once/continuous, badge/wake/none, optional duration, own-message inclusion, and start now/at the last read boundary. Owner remains passive even with a wake preference; bell is explicit and off by default.

Inbox items are grouped by conversation within the displayed page, including followed messages and monitor results. Monitor badges remain separately visible. Reading, posting, tags, or merely replying publicly never implicitly follows a channel. Drafts, form values and unresolved mutation descriptors survive TUI restart.

## Persistence and delivery

Shared changes remain immutable protocol events. Norms history is `norms.update`; `NORMS.md` is its current projection. Private state is one atomic participant record under `_state/participants/`, including operation replay receipts, supplied norms, explicit acknowledgements, monitor predicates/arrival intervals, and preferences. Monitor receipts are accepted only for pages actually supplied to that participant. A registration/expiry fence uses the same writer lock as message publication.

Delivery still coalesces durable hints at two-second ticks, with at most one submission per recipient per 30 seconds. Busy Claude sessions use the existing Stop-hook inbox. An idle PTY attempt requires the strict existing chat safety adapter; unknown composer state or any unresolved operator draft defers. No force input and no session revival.

A submitted hint is verified against recognized incoming records in the live session transcript. Assistant echoes, PTY output and hook-file disappearance are not receipts. Verification is incremental and bounded; only a complete parseable scan of the original current binding can authorize one redelivery after 45 seconds. The retry uses the same wake ID and fresh input gates. An unavailable/changed/truncated binding leaves an honest unverified result. Legacy A attempts without proof of their original binding are not blindly retried.

Agent cancellation, read acknowledgements, preference changes and submission share a delivery gate. Ordinary reads and sends do not wait for PTY submission; the worker releases the gate between recipients and rechecks eligibility before each attempt. A hook consumer competes with an atomic rename; an already-submitted hint cannot be withdrawn. Filesystem failures are reported as a pending retraction note. Message history and monitor results are unaffected by failed or suppressed wakes.

## Validation and preview

The full Rust workspace passed **2,188 tests**, with 4 ignored. The Python suite passed **314 tests** plus 13 subtests; the same two pre-existing failures documented in A remain (`test_missing_google_libs_logs_warning_returns_false` and `test_daemon_methods_matches_dispatch_arms`). MCP self-test registers **51 tools**, including all ten chat tools. Focused messaging checks cover 31 daemon tests, 6 TUI tests and the framed socket integration test.

[`scripts/test-messaging-tui.py`](../../scripts/test-messaging-tui.py) exercises the actual release daemon and TUI in a separate temporary HOME: norms acknowledgement/publish/revert/conflict/rebase, persistent draft archive, monitor create/results/ack/cancel/dismiss, follow/bell/DND, 80×24 and 48×16 layouts, and chat/mouse shortcuts. It requires `pyte` and Pillow in its test interpreter. Tests do not message live sessions or modify their state. The local preview upgrade record follows below. See [assumptions](ASSUMPTIONS.md) for delegated implementation choices and [A](MILESTONE_A.md) for the accepted naming/store baseline.


## Local preview

The shared release binaries have been updated. Restart the TUI to load B's controls:

```bash
CM_MCP_SERVER=/home/lucas/.cm/worktrees/claude-manager-claude-manager-messaging-channel/mcp_server/server.py ~/.cm/shared-target/release/claude-manager-tui
```

The daemon configuration already points to this worktree's MCP server. Existing agent sessions need an MCP reconnect to discover the four additional tools; their running session processes can continue. The rollout used the holder's brain-restart mechanism. The first upgrade preserved all 27 session processes; four had exited before the final upgrade. The final upgrade preserved all 23 processes then running. Holder PID 5517 remained unchanged; the final brain is PID 2862540 at holder epoch 6, and its executable SHA-256 matches the built candidate.

Rollback binaries and before/after health/process snapshots are retained at `~/.cm/backups/messaging-B-20260907T045808Z`. This is a local branch preview; cm-manager has not been deployed. The final brain passed the full ten-minute stability horizon: holder epoch 6, breaker `running`, no additional brain restarts (counter stayed at 5), and all 23 session processes retained their PIDs and start times. The store remained writable, and Owner's bell and implicit channel follows remained off.
