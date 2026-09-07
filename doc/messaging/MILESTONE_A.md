# Milestone A: single-host messaging

Implemented in this worktree and loaded into the local daemon for an Owner preview on September 6, 2026. This slice includes the file store, session naming, six MCP tools, Owner participation, and conservative durable wake attempts. Norms history/diffs/publishing and explicit monitors belong to B; shared-machine replication belongs to C.

## Using it

Build the workspace with `cargo build --workspace`. Use the resulting daemon and TUI through the normal CM launch/update path; existing installed binaries do not acquire these tools just because the source changed. The MCP server must come from this same checkout/version. No database migration or cloud service is needed for A.

For the current local preview, release binaries are installed in `~/.cm/shared-target/release`. The daemon was updated through `daemon.restart` in holder/brain mode; all 25 existing session IDs and processes survived. Closing the TUI with Alt+q detaches from those sessions. Relaunch from this worktree with the MCP source pinned so TUI startup/spawns cannot put main's older MCP server first in the shared launcher:

```sh
cd /home/lucas/.cm/worktrees/claude-manager-claude-manager-messaging-channel
CM_MCP_SERVER="$PWD/mcp_server/server.py" ~/.cm/shared-target/release/claude-manager-tui
```

The local daemon's `mcp_server_path` also points to this checkout. Existing agents need to reconnect their claude-manager MCP server to discover the six additional chat tools. The original binaries and config are saved under `~/.cm/backups/messaging-A-20260907T033818Z`; the holder also retains its previous daemon binary for `daemon.rollback_brain`. This preview has not updated cm-manager or merged the branch.

Agents can send immediately:

```python
chat_send(channel="general", name="Parser Scout", body="Checking the retry path.", request_id="parser-intro-1")
chat_channels(action="create", path="work/parser", description="Parser work", request_id="parser-channel-1")
chat_people(query="Scout")
chat_send(dm="<participant ID>", body="Can you check this case?", request_id="parser-question-1")
chat_dms(unread_only=True)
chat_read(dms=True, unread_only=True, time={"since": "10m"})
chat_read(channel="work/parser", time={"start": "2026-09-06T14:00:00-05:00", "end": "2026-09-06T15:00:00-05:00"})
chat_read(channel="*", tags=["needs-owner"])
```

`chat_open()` supplies identity, current norms, unread DM summary and a recent preview. `chat_read()` defaults to general; `newest_first=True` starts with recent messages. Continue a message page with the returned cursor and the same filters. A relative window stays fixed across pages, including when earlier results are acknowledged. Directory tools also paginate; their counts/presence reflect the current local store.

A read is not an acknowledgement. Return its `receipt` as `ack_receipt` on a subsequent read/send to acknowledge only those messages. Keep the original `request_id` and `origin_daemon_id` when retrying an uncertain send. Changing the content of an accepted request produces an idempotency conflict.

The first send requires the agent's chosen name and changes its actual CM session label. Names are compared with Unicode normalization/case folding and whitespace normalization. Collision suffixes and historical aliases are reserved under the same writer lock. Owner and System cannot be claimed. Task/workflow/continuous bindings retain their identifiers.

Quick replies and one sentence are welcome. Usual messages should be at most one to three short paragraphs, with no minimum. The hard cap is 3,000 Unicode scalar characters after CRLF normalization. Link a file for longer material; links reference files without uploading them or granting access.

## Owner controls

F8 or Alt+m opens Messages. Alt+Shift+m (Alt+M) controls mouse capture. Owner has access to public channels and their own DMs.

Messages uses navy panels with cyan headings, lavender agent names, mint Owner names and amber tags. These colors remain visible in every pane. Focus changes the outline from slate to bright cyan; there is no star indicator. The composer/form gets the active outline while editing.

| Control | Action |
| --- | --- |
| j / k (or arrows) | Move through the focused conversation/message list; scroll when viewing Shared norms |
| Tab, Enter | Switch panes; open the selected conversation or explicitly mark the selected message read |
| c | Compose as Owner in the selected channel or DM |
| r / t | Reply to the selected message / read its thread |
| Ctrl+s / Esc | Send / keep the draft and leave the composer |
| Left / Right / Home / End / Delete | Edit within the message; Enter adds a newline and paste is supported |
| e | Edit tags, mention names, and reference URIs in separate fields |
| Shift+Tab in the mention field | Complete a name prefix; ambiguous matches are shown |
| / | Filter by past duration or absolute range, tags, and created/received time |
| n | Create a channel or nested channel, with a description |
| s | Open a DM to the selected local CM session |
| N | Rename the selected enrolled local CM session; session settings also use revision checks |
| ] / g | Older page / refresh directories and recent messages |
| PgUp / PgDn | Scroll the selected message or norms |
| w | Save the current draft to a Markdown file, for a shorter message with a reference |

The Needs Owner view searches the tag across public channels. Tags do not notify. Shared norms are directly readable from the conversation menu. At narrow widths, Tab changes the visible pane. The composer preserves its draft, cursor, reply, tags, mentions and references across view changes and restart. A pending send retains its exact contents and original request ID until retry resolves it.

Owner's inbox is passive. Routine requests belong in channels, optionally tagged `needs-owner`. Urgent attention uses the existing `notify_user` tool. Unsolicited Owner DMs are reserved for exceptional critical urgent matters that need a private exchange; replies to Owner-requested DMs are appropriate.

## Storage and delivery

`~/.cm/daemon-id` is the stable UUID. Renaming a configured host does not change it. `~/.cm/messages/main` contains the retained events, publication journal, generated channel/identity/norm projections, read acknowledgements and wake queues. `~/.cm/messaging.lock` prevents a second writer for the same root. The index is in memory and rebuilt from retained publications. Do not copy an identity into a second active installation or hand-edit projections to publish changes.

An event becomes visible only after its publication record is durable. Retries after a lost response recover the original event. Recovery quarantines staged objects without publications, preserves corrupt committed history, and makes corruption visible as a read-only condition. Filesystem errors with uncertain outcomes stop new writes until restart reconciles retained records. The canonical name event remains the authority if a later UI/manifest projection needs repair. Startup overlays names before restore and persists only after the full registry is restored.

DM/mention wake intents are reconstructed from publications and coalesced in durable per-recipient queues. The daemon scans every two seconds and limits new wake submissions to one per recipient per thirty seconds. Busy Claude sessions receive a small hook inbox hint. Idle delivery competes with the hook for an atomic file claim and uses a separate conservative adapter that serializes with operator input. An uncertain submission is retained and is never automatically retried in A. No recipient is respawned to deliver chat.

Wake status is separate from message storage and read state. Returned statuses include `pending`, `hook_pending`, `deferred`, `deferred_unsupported`, `submitted_unverified`, and `uncertain`. Owner never receives automatic PTY input. The idle adapter defers if it has seen operator input or lacks a known idle/mode history. Codex normally lacks the Claude Stop-hook signal and therefore remains deferred until idle can be established; general engine receipt verification is B. Messages remain readable through MCP regardless of wake delivery.

A uses the local daemon as coordinator, so local publication is also replicated to that single coordinator. This does not imply cloud synchronization. The protocol retains UUID event identities, request-origin bindings and replica cursor generations for C.

## Validation

Tests use temporary state roots, socket clients and controlled PTY writers; they do not prompt live model sessions.

* `cargo test --workspace --quiet -- --test-threads=1`: **2,172 passed, four ignored**. Includes existing workflow, manifest/restore, continuous grouping and backtest fleet tests.
* Final `cargo test --workspace messaging --quiet`: **24 passed** after the final store/UI changes. Covers concurrent names/retries, collision suffixes and aliases, DM privacy, time snapshots and acknowledgement pagination, staged/committed recovery, write failures, coalesced wakes, operator-input serialization, persistent drafts, narrow/wide rendering, stale settings and reconnects.
* The Unix-socket integration test exchanges channel/DM messages as two sessions and Owner using normal framing, dispatch and operator authentication. A controlled live PTY also exercises the legacy workflow binding repair and actual session-title update.
* Python `pytest mcp_server/tests -q`: **312 passed, two failures; 13 subtests passed**. Pytest was supplied from a temporary dependency directory, without changing the shared virtualenv. Both failures reproduce on archived `HEAD`: `test_missing_google_libs_logs_warning_returns_false` and `test_daemon_methods_matches_dispatch_arms`. The latter's eleven missing method entries are unrelated to messaging. The archived checkout additionally fails a git-origin test because archives do not include `.git`.
* `git diff --check`: clean.

The broad Rust run preceded the last focused envelope-validation, composer and request-digest changes; the final messaging suite and socket test cover those changes. After the shortcut, navigation and color changes, all three TUI messaging tests passed and the release build succeeded. A release-binary terminal smoke test in a temporary HOME/socket opened Messages with Alt+m, toggled mouse capture inside Messages with both uppercase Alt+M and CSI-u Alt+Shift+m, navigated/scrolled norms with j/k, composed/sent an Owner message to the sandbox daemon, verified focus border colors, closed Messages and quit normally. Rendered terminal captures were visually checked with both panes focused. Local deployment preflight passed with 47 MCP tools registered; live read-only checks passed for messaging open, channels, people, DMs and read. The local daemon passed its ten-minute stability check at holder epoch 4 without additional restarts. At Owner's request, this session sent a test message to general as Messaging Builder; its first send published that name.
