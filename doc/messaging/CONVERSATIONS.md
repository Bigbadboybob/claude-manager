# Conversation UI and group DMs

September 7, 2026 — Owner feedback after milestone B.

Messages now appear in one timeline, oldest at the top and newest at the bottom. Every body is expanded, including paragraphs, tags and file references. j/k selects a message with a background highlight; moving above loaded history fetches older messages. PgUp/PgDn scrolls within long content. `]` also loads older history. Enter explicitly marks the selected message read. The composer opens beneath the timeline with `c` or `r`; Ctrl+s sends and Esc keeps the draft.

Unread ordinary posts have a muted dot. Unread explicit Owner mentions show `● @you`; incoming DMs show `● DM`. The channel list shows an unread dot or mention count. The DM section shows a total unread count and individual counts. Owner's own messages are not unread. These markers are passive; existing Owner notification preferences still apply.

The **DMs** section expands/collapses with Enter or Space and remembers that choice. It lists only started conversations, ordered by recent activity. `d` opens **New DM**: type to search names, aliases, task summaries or IDs; use arrows (or Ctrl+j/k) to move. Enter chooses one person. For a group, Tab toggles each recipient before Enter opens the draft. Opening or cancelling a draft does not publish an empty conversation.

Groups include the sender and up to 31 others. Membership is fixed. Starting with the same set reopens the same conversation; changing recipients starts a separate one with no copied private history. Owner has the same access as any member and cannot browse groups that exclude Owner.

For agents, `chat_open`, `chat_read`, and `chat_send` accept `dm="participant-id"` or `dm=["participant-a", "participant-b"]`. Use `chat_people` to resolve IDs. First send creates the conversation. Subsequent requests may use its `conversation` ID. `chat_dms` includes groups with `members`, `peers`, `group=true`, and `peer=null`; `peer=...` filters any conversation including that recipient. Exact request contents and IDs must be preserved across retries.

Monitors, unread queries, time filters, threads, mentions, inboxes and preferences work in groups. Use `scope={"conversation":"group-id"}` for a group monitor/follow; `scope={"dm":"peer-id"}` remains a pair-only scope. `scope={"dms":true}` covers all incoming pairs and groups. The TUI's `W` and `f` actions use the current conversation.

The envelope remains v1. The service advertises `group_dms`; older binaries with a two-member reader restriction must not be used over group history. Storage and privacy details are in [PROTOCOL.md](PROTOCOL.md).

Validation: 43 Rust messaging tests and 7 Python messaging tests passed. The isolated release-terminal test exercises full messages, chronological navigation across pages, catch-up after a burst larger than one page, stable selection during refresh, unread mentions/DMs, recipient search, group creation/replies, section collapse, and the existing B norms/monitor/preferences flows. Rendering was checked at 80×24 and 48×16, with screenshots inspected. MCP self-test registers 51 tools.

Local rollout uses the existing holder/brain restart path. The holder stayed at PID 5517; epoch advanced from 6 to 7, preserving all 25 session processes. The previous binaries and deployment evidence are in `~/.cm/backups/messaging-conversations-20260907T144406Z`. The shared preview TUI is `~/.cm/shared-target/release/claude-manager-tui`. Quit/relaunch only the TUI to use it; pin `CM_MCP_SERVER` to this worktree's `mcp_server/server.py` as in the B guide. Existing agents need an MCP reconnect to discover the array form of `dm`.

The final ten-minute stability check passed: holder epoch remained 7, the breaker stayed running, and the brain restart counter did not advance after the deployment.
