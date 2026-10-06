# Implementation plan: messaging fixes, Owner availability, orchestration skill

Repo: `/home/lucas/.cm/worktrees/claude-manager-swarm-focused-design-cm`. Sources: `cm-initiative/shared/coordination-brief.md` §4–§8, EP handoff A1–A10/B1/B5/B6/B9. Out of scope: items/board and session-state detection. The only thing this plan defines for them is the escalation hook in B6.

## A. Messaging fixes (§6)

**A1. Body `@Name` becomes a real mention, or a warning (EP A5/B6)**
- Root cause: `Store::send` (`daemon/src/messaging/store.rs:1411-1424`) only reads `p["mentions"]`, and each entry must be a participant ID (`not_found` otherwise). The body is never scanned. The MCP docstring (`mcp_server/server.py:361`) says so, but agents don't read it that way.
- Fix (daemon, inside `send()` after `members` is computed):
  1. Pull the name-or-alias resolution in `resolve()` (`store.rs:1197-1235`: normalize, then name_key, then drop released names) into a helper, `resolve_participant_ref(&str) -> Option<String>`.
  2. Let `mentions[]` take names as well as IDs (B6).
  3. Scan the body for `@token`, skipping text inside backticks or fences, emails and URLs. A token that resolves to exactly one current member of the conversation (channel `memberships[conv]`, or the DM members) is added to `mentions`, so it notifies.
  4. An unresolved, ambiguous or non-member token goes into `warnings: [{code:"unresolved_body_mention", token, hint}]`. `@here` in the body gets a warning that points at `mention_here=true`. It is not promoted.
  5. Return `mentions_resolved: [{token, id, name, source:"body"|"param"}]`.
- Resolution happens on the executing store. Forwarded sends run on the hub, so the hub must be deployed first. The stored `data.mentions` stays IDs only, so replication and recipients are unchanged. Idempotent retries return the prior event (`request()`), so they stay deterministic.
- Files: `daemon/src/messaging/store.rs` (send, plus the helper); MCP docstring in `mcp_server/server.py` `chat_send`; `mcp_server/AGENT_GUIDE.md:26` and `doc/messaging/MEMBERSHIP_AND_MENTIONS.md` ("Body text alone never notifies" changes).
- Tests (store.rs tests module):
  - body `@lane` promotes for a channel member;
  - a non-member, an ambiguous name and a token in a code span each produce a warning;
  - a name in `mentions[]` resolves;
  - a released alias yields to the current holder;
  - a retry with the same request_id returns the same event.

**A2. Inbox newest-first by default, plus bulk "mark read before T" (EP A4/B5)**
- Root causes:
  1. `read()` sorts by logical time ascending, and `newest_first` defaults to false (`store.rs:1905`; MCP default `server.py:399`).
  2. `monitor_status.unacknowledged` (`store/watches.rs:269-283, 597`) counts monitor hits past `m.acknowledged`. That counter is advanced only by `chat_monitors(action="ack")`, never by message read receipts. So reading and acking inbox pages can't lower the "179". There are two separate ack systems.
- Fix:
  - (a) In `read()`, `inbox`/`dms` reads default to `newest_first=true` when the parameter is absent. MCP changes the default to `newest_first: bool | None = None` and omits it when None.
  - (b) New `chat_read(..., mark_read_before=RFC3339, request_id=...)`, valid only with `inbox` or `dms`. It adds every eligible message with `created_at < T` to `reads[actor]` and advances each matching monitor's `acknowledged` through those positions. It returns `{marked, monitors_advanced}`. For Owner, publish `read.ack` in chunks of ≤200 IDs, which keeps the existing receipt limit.
  - (c) `monitor_summary` excludes hits whose message is already in `reads[actor]`, so reading clears the count.
  - (d) Covers EP A6's remainder: `chat_read` defaults to `view="slim"` for inbox/dms reads and stays full otherwise.
- Check: `delivery::read_boundary` (`messaging/delivery.rs:537`) must still advance the wake boundary when a newest-first page is supplied. Add a test.
- Files: `store.rs` (read, acknowledge), `store/watches.rs`, `messaging/delivery.rs` (boundary check only), `mcp_server/server.py` chat_read.
- Tests:
  - inbox order;
  - newest-first plus cursor paging with unread_only (anchor logic `store.rs:1909-1937`);
  - bulk mark: count drops and the monitor count drops;
  - the wake boundary advances;
  - `rpc.rs:781-796` style end-to-end.

**A3. Authoritative send time shown to the sender (EP A10, part of A2)**
- Root cause: `send_response` (`store.rs:1523`) buries `created_at` in `event`, and the MCP layer passes it through unchanged. A forwarded send gets its `created_at` at the hub. A 40-minute hang shows nothing to the sender.
- Fix:
  - `send_response` adds a top-level `created_at`.
  - The forward branch in `rpc.rs:499-514` records `submitted_at` before `sync.request` and adds `delay_s` when it exceeds 60 s.
  - Every messaging response adds `outbox: {pending_sync, oldest_age_s}` when the caller has own events still `pending_sync`. Count them in `store/replication.rs` next to `event_replication`.
  - Guide norm: "never hand-stamp times; CM's `created_at` is authoritative."
- Files: `store.rs`, `messaging/rpc.rs`, `store/replication.rs`, AGENT_GUIDE.
- Tests: response fields exist; a forwarded send with an injected delay; outbox count while sync is offline (`replication.rs:2007` fixtures).

**A4. `read_last_turn` returns empty content for Codex (EP A8)**
- Root cause, verified on real rollouts (`~/.codex/sessions/2026/10/04/rollout-…01a107a7….jsonl`): 18 of 72 `phase:"final_answer"` assistant messages are `content:[{"type":"output_text","text":""}]` with `content_item_kinds:["unknown"]`. The following `task_complete.last_agent_message` is null. Python `_render_content` (`mcp_server/transcripts/codex.py`) appends `""` and returns `""`, so an empty Message is created. `_last_assistant` (`mcp_server/monitor.py:281`) then returns it and masks the earlier `commentary` message that has text.
- The Rust parsers already drop empty text (`daemon/src/workflow/transcript.rs:142-160`, `tui/src/agent/codex.rs:251`), so this is Python only.
- Fix:
  - `_render_content` keeps only non-empty text.
  - `_last_assistant` skips whitespace-only messages.
  - Optionally carry `phase` on `Message` and set `last_assistant.phase`.
  - Record `response_item/agent_message` (inter-agent, encrypted) and `custom_tool_call` as known and skipped. `custom_tool_call` could also render as a Tool line.
- The same bug feeds monitor fire text (`async_monitor`, `server.py:2562`).
- Files: `mcp_server/transcripts/codex.py`, `mcp_server/monitor.py`, `mcp_server/transcripts/types.py` (optional phase).
- Tests: `mcp_server/tests/test_transcripts.py` with a fixture of commentary text, then an empty final_answer, then task_complete null: expect the commentary text.

**A5. `context_status.stale_scopes` noise (EP A9)**
- What acknowledging means: `acknowledge_scoped_norms` (`store/norms.rs:128`) records in personal state that the actor received revision R. It is set only via `chat_send(norms_seen={scope:rev})` or `chat_norms(ack_revision=…)`. It does two things: it sets the default base for `chat_norms(action="diff")`, and it removes the scope from `stale_scopes`. Nothing is gated on it. `context_response` says "never rejects the primary operation".
- Root cause: `context_response` (`norms.rs:184-262`) adds `context_status` to every response, and `stale` stays non-empty until an explicit ack that agents never perform. The field gives no instruction.
- Fix:
  - Emit `context_status` only when the response actually supplies a document (new or changed revision, `force`, or `chat_open`).
  - When it is emitted, add `ack_with: {"norms_seen": {"global": "<rev>"}}` and a one-line note: "pass on your next chat_send; diffs start from here."
  - When a complete document was inlined, set `stale_scopes` to `unacknowledged_scopes` and keep `changed` for compatibility.
- Files: `store/norms.rs`, plus docstrings in `server.py` (chat_send at `:381`, chat_norms).
- Tests: update `norms.rs:660-695` and `rpc.rs:818-823`. Add "a second ordinary call carries no context_status" and "a changed revision brings it back with ack_with".

**A6. Does c097938 close EP A1–A3/A6?**
- Covered:
  - A1: daemon stall alarm, `delivery/alarm.rs`, Owner alert after 15 min unobserved.
  - A1: consumer handoff plus `health: degraded`.
  - A3: wake text with sender, preview and slim hint (`delivery.rs:598`).
  - A6: `view="slim"` exists.
  - A4: `monitor_status.recently_expired` surfaces watches that expired silently.
- What remains:
  - (i) The A1 alarm only reaches the laptop TUI, so EP's B1 "phone" ask is unmet. In B5, route the stall alarm through the gate as `blocking` and send it to `notify_command` (Telegram) when no viewer is connected or the level is away/around.
  - (ii) A2 (sends stuck in `pending_sync` with no signal) is not addressed. Fixed by A3's `outbox` plus a sender-side alarm: own message `pending_sync` > 15 min raises an alert through the same alarm.
  - (iii) A6: slim is not the default and the full view still repeats `norms`/`context`. Fixed by A2(d) and A5.
  - (iv) A4: the backlog count and ordering are fixed by A2. Re-arming an expired DM monitor is still manual, but now visible.

## B. Owner availability and gated `notify_user` (§4)

**B1. Storage: a replicated messaging event, not the planning API**
- The level is a new shared event, `owner.availability`. Actor is `owner`, `conversation_id` is null, data is `{level, changed_at, previous, source:"tui"|"cli", note?}`.
- Why this store:
  - Events with no conversation replicate to every enrolled host (`store/replication.rs:1406-1408` returns true).
  - They are Owner-authenticated and durable.
  - Every daemon can read the value locally, which keeps `ping` network-free, and the last known value survives a hub outage.
- Why not the planning API: it would need polling, and laptop daemons may have an empty `api_url` (`daemon/src/config.rs:504`).
- Publishing:
  - `publish()` enforces `shared_mutation_allowed` (`store.rs:910`).
  - Non-coordinator hosts forward to the hub: add the method to `needs_coordinator` (`replication.rs:2463`).
  - Reduce into `Store.owner_availability` (`store.rs` reduce, around `:700`).
  - Journal compaction (`ca49fa0`) must keep the latest `owner.availability`, like norms.
- Unset means legacy behavior: every alert is delivered.
- New method `messaging.availability {action:get|set, level, note?, request_id}`. `set` requires `kind=="owner"` (operator caller). Wire it in `messaging/rpc.rs` `execute_with_freshness`.

**B2. Exposure**
- `ping`: add `owner_availability: {level, changed_at, age_s}` in `dispatch_ping` (`daemon/src/control/dispatch.rs:448`).
- `chat_open`: add the same to the JSON at `rpc.rs:588`.
- `notify_user`: include it in the result.
- `board()`: reads it (other planner).

**B3. Setting it**
- TUI: **F7**. It is free: F8 is messages (`tui/src/app/messages.rs:748`) and F9 is settings (`global_settings.rs:217`).
  - F7 opens a 4-row picker: keys 1–4 or arrows, Enter sets, Esc cancels.
  - New `tui/src/app/availability.rs`, modeled on `global_settings.rs` (consume keys while open). Route it in `tui/src/app/events.rs` ahead of the PTY.
  - The status bar in `tui/src/app/draw.rs` shows `● focused 14m`.
  - Sends `messaging.availability` to the messaging host the messages pane uses.
- CLI: `scripts/cm-availability [away|around|focused|on-call] [--note …]`.
  - A thin wrapper over the `scripts/cm-op` `rpc()` that calls `messaging.availability`. With no argument it prints the current value.
  - Installed to `~/.cm/bin/` on cloud hosts so the phone can run `ssh cm-manager cm-availability away`.
  - The target host must have `owner_access`.

**B4. Gate model (new `daemon/src/owner_availability.rs`)**
- Ordered enums: `Level` is away < around < focused < on-call; `Urgency` is fyi < decision < blocking < emergency.
- Delivery bar per level: away=emergency, around=blocking, focused=decision, on-call=fyi. This mapping is my assumption: confirm it with Owner.
- `notify_user(message, urgency="decision")`, in `server.py:1704` and `owner_attention.rs` `NotifyParams` (keep `deny_unknown_fields` and add the field).
- Result: `{ok, status:"queued"` (kept for compatibility)`, delivery:"immediate"|"held", alert_id, urgency, owner_availability, release_when:"<level>"}`. The docstring explains that "held" means "keep working; you'll be woken on release".
- Held alerts go in a separate `~/.cm/owner-attention-held.json`, bounded, latest per session with a count. They must not go in `owner-attention.json`, because `manifest.watch` snapshots that file (`dispatch.rs:2528`) and old TUIs would display held alerts.
- `Alert` gains `#[serde(default)] urgency, held_since, released_at`. The TUI shares the type, and it has no `deny_unknown_fields`, so old viewers ignore the new fields.
- Emergency (any level), and anything delivered while the level is away/around with no viewer connected: also call `notify::notify_operator(config.notify_command, "owner-attention", …)` (`daemon/src/notify.rs:32`), rate-limited per session.

**B5. Level change: release and wake**
- The delivery worker tick (`messaging/delivery.rs:275`, 2 s) compares `store.owner_availability.revision` with a persisted `~/.cm/owner-availability-applied.json`. On a change it calls `owner_attention::on_availability_change(old, new)`.
  - This runs on every host, including after restarts and for replicated arrivals.
- On change:
  1. Release held alerts whose urgency now meets the bar into the alerts map, flagged `released_at`. If a pending alert already exists for the session, merge the text up to 4096 bytes.
  2. Wake, via `notifications::publish(root, uid, "owner-availability:<rev>", "owner", text, marker)` (`daemon/src/notifications.rs:51`), these local sessions:
     - (a) continuous orchestrators, from `messaging/tasks.rs` orientation;
     - (b) live sessions whose `task_id` is an active initiative's `coordinator_task_id`, from a cached `GET /initiatives` via `planning_client.rs` (best-effort, refreshed every 10 min);
     - (c) uids registered through `owner_availability::register_orchestrators(source, uids)`, the board's hook;
     - (d) sessions with held or released requests.
  3. Wake text: "Owner availability: away→focused at 14:02Z. N of your requests were released/still held."
  - Other sessions only see the new level on their next `ping`, `chat_open` or `notify_user`.
- TUI digest: `owner_attention` diffs with `released_at` that arrive within 5 s of an availability change produce one desktop popup, "N held requests released", instead of N popups. Files: `tui/src/app/attention.rs`, `tui/src/owner_notification.rs`.

**B6. Shared escalation interface (stall alarm and board)**
```rust
pub struct Escalation { source: String /* "stall:<uid>" | "board:<board_id>" */, dedupe_key: String,
  urgency: Urgency, summary: String, session_uid: Option<String>, task_id: Option<String> }
pub enum GateOutcome { Delivered{alert_id}, Held{alert_id, release_when: Level}, Coalesced{alert_id} }
pub fn escalate(state: &DaemonState, e: Escalation) -> Result<GateOutcome, String>;
pub fn withdraw(state: &DaemonState, source: &str, alert_id: &str) -> Result<bool, String>; // wraps clear_system
```
- `raise_system`/`clear_system` (`owner_attention.rs:148-203`) become thin wrappers. `delivery/alarm.rs` calls `escalate(urgency=Blocking)`.
- Alerts that are not tied to a session key the map by `source` (for example `board:<id>`). The TUI already renders alerts whose session row is gone.
- The operator-only RPC `owner_attention.escalate` lets a board evaluator elsewhere (API or another host) call it.
- Board contract: when flags stay unresolved while the orchestrator is idle, call `escalate(source="board:<id>", dedupe_key=board_id, urgency = Blocking if oldest ≥ 1h or flags ≥ 3 else Decision, summary="<n> unresolved flags, oldest <age>, orchestrator <name> idle <t>")`. Call `withdraw` when the flags clear.

## C. Orchestration skill and agent-guide norms (§7)

**Where it lives**
- Personal skills are canonical in `~/.claude/skills/<name>/SKILL.md` on the source host. `~/.cm/skill-sync.json` names it as `source_hostname = cm-sessions`, and that is the host this session runs on.
- Codex gets a symlink in `~/.agents/skills/`. `scripts/cm-sync-skills` pushes to cm-manager and the other targets (`doc/personal-skill-sync.md`). setup-initiative, create-continuous-task and triage-review all live there, outside the repo.
- Plan:
  - Canonical text in the repo, `doc/ORCHESTRATION.md`. Deploy it to `~/.cm/policies/orchestration.md` through `scripts/release-continuous-docs.py`, extending the `POLICY_SRC/DST` pattern at `:28-29`.
  - A short skill, `~/.claude/skills/orchestrate-swarm/SKILL.md` plus the `~/.agents/skills` symlink, that points to that policy.
  - Cross-link it from the setup-initiative and create-continuous-task skills.
  - The core norms also go in `mcp_server/AGENT_GUIDE.md`, which ships in the MCP init to every project.

**Content outline**
1. Engine choice:
   - Codex: a smart individual contributor for hard, technical, low-volume work, with little internal parallelism.
   - Claude: a lead with up to about 6 native subagents for broader or higher-volume work.
2. Reuse sessions that hold good context: send new items with `send_input`, not new `start_session` calls.
3. Every piece of work in flight is an item with a holder. Check the board's flags instead of DMing lanes for status. Board tools arrive with the items work.
4. A job longer than 20 min must be declared `waiting` with an ETA and a one-line note.
5. Use structured `mentions`, or a body `@Name` once A1 lands, to wake someone. Tags are passive.
6. Owner requests go through `notify_user(urgency=…)` with honest urgency. Check `ping().owner_availability`. A held request means keep working.
7. Keep logs, results and Owner questions separate (EP B9). An Owner question states the options, a recommendation, and who is blocked.
8. Read the inbox with `chat_read(inbox=True, unread_only=True)` (newest-first and slim by default). Bulk `mark_read_before` after a long absence. Never hand-stamp times.
9. After compaction: own last posts, then replies, then the board.

**Guide changes**
- `AGENT_GUIDE.md`: sections on mentions, notify_user, inbox and the orchestration pointer.
- `doc/AGENT_QUICKSTART.md`: examples.
- `CLAUDE.md`: one paragraph, plus the F7 key in the keybinding list.
- Post the advance notice and the follow-up in `#cm-general`.

## Phased slices (each independently mergeable)

| # | Slice | Files | Tests | Deploy |
|---|---|---|---|---|
| 1 | Codex last-turn fix (A4) | `mcp_server/transcripts/codex.py`, `monitor.py`, `types.py`, `tests/test_transcripts.py` | `uv run pytest mcp_server/tests/test_transcripts.py test_final_monitors.py` | Copy the MCP payload to `/opt/cm-daemon/mcp_server/` on manager and sessions, run a brain `daemon.restart` per CLAUDE.md, then reconnect MCP |
| 2a | Send side: mentions, send time, outbox (A1, A3, A6-ii) | `store.rs` (send, send_response, helper), `rpc.rs` forward branch, `store/replication.rs`, `server.py` chat_send | `scripts/cm-test-isolated cargo test -p cm-daemon messaging::` with a private `CARGO_TARGET_DIR`; `pytest mcp_server/tests/test_messaging.py` | Brain plus MCP payload. **Hub (coordinator) first**, then the other hosts (`HOWTO_HOLDER_BRAIN_SPLIT.md §3`), then the laptop daemon |
| 2b | Read side: newest-first, slim default, mark_read_before, unack fix, norms quieting (A2, A5) | `store.rs` read/acknowledge, `store/watches.rs`, `store/norms.rs`, `delivery.rs` (test only), `server.py` chat_read | same | same |
| 3 | Orchestration policy, skill and guide (C) | `doc/ORCHESTRATION.md`, `scripts/release-continuous-docs.py`, `mcp_server/AGENT_GUIDE.md`, `doc/AGENT_QUICKSTART.md`, `CLAUDE.md`; skill outside the repo | doc-only; check that `release-continuous-docs.py --dry-run` stages the policy | Release the docs script, copy the MCP payload (guide), `cm-sync-skills`, `#cm-general` post |
| 4 | Availability state (B1–B3 minus TUI) | `owner_availability.rs` (new), `store.rs` reduce + compaction, `replication.rs` needs_coordinator, `rpc.rs`, `dispatch.rs` ping, `scripts/cm-availability`, `server.py` ping docstring | Rust: event round-trip, non-owner rejected, replica applies it, survives compaction and restart; python: route selection | **All daemons before first use**, so older replicas never meet the unknown type; hub first |
| 5 | Gated notify_user, release, wakes, escalate (B4–B6) | `owner_attention.rs`, `owner_availability.rs`, `delivery.rs` tick, `delivery/alarm.rs`, `notify.rs` call site, `planning_client.rs` (initiatives GET), `server.py` notify_user, `doc/OWNER_NOTIFICATIONS.md` | Rust tests in `owner_attention.rs`: held vs delivered per level matrix, release on raise, merge with pending, emergency calls notify_command (stub), wake published once per revision, restart idempotent; `test_socket_route_selection.py` | Brain plus MCP on every host |
| 6 | TUI: F7 picker, status bar, release digest | `tui/src/app/availability.rs` (new), `events.rs`, `draw.rs`, `attention.rs`, `owner_notification.rs`, `app.rs` | `scripts/cm-test-isolated cargo test -p claude-manager-tui availability` plus the existing attention tests | `doc/TUI_RELEASES.md` (`cm-tui-release` skill); no daemon restart |

Order: 1, 2a and 2b, and 3 in parallel. Then 4, then 5, then 6. Slice 6 can be built against 4 while 5 is in progress.

## File-ownership boundaries for parallel workers
- **W-transcripts (1):** `mcp_server/transcripts/*`, `monitor.py`. Coordinate with the session-state planner, who touches the Codex parsers for 0.160.
- **W-send (2a):** `store.rs` limited to `send`/`send_response`/`resolve` plus the helper; `rpc.rs` forward branch; `replication.rs` outbox counter.
- **W-read (2b):** `store.rs` limited to `read`/`acknowledge`; `watches.rs`; `norms.rs`. Both 2a and 2b edit `store.rs`, in disjoint functions: merge 2a first and rebase 2b.
- **W-docs (3):** docs, release script, skill dir. `AGENT_GUIDE.md` is also touched by 2a and 2b docstrings, so W-docs owns the guide and the others send it text.
- **W-availability (4, 5):** `owner_availability.rs`, `owner_attention.rs`, `delivery.rs` tick, `alarm.rs`, `dispatch.rs` ping, `notify_user` in `server.py`.
- **W-tui (6):** `tui/src/app/availability.rs`, `draw.rs` status segment, `attention.rs`.
- **Board planner:** only calls `owner_availability::register_orchestrators` and `owner_attention::escalate`/`withdraw`. It does not edit these modules.

## Risks
- **Unknown event types on old replicas.** `reduce` uses if-chains and appears tolerant, but this is unverified on every receive path (`replication.rs` preflight). Mitigation: deploy slice 4 everywhere before the first set, and add a test that a replica built without the reducer ignores the event.
- **Journal compaction could drop the availability event.** Pin the latest one, as norms are pinned.
- **Mention auto-promotion could wake people unintentionally** (a name quoted in prose). It is limited to conversation members and exact names or aliases, skips code spans, and always reports `mentions_resolved` so the sender sees what happened.
- **Newest-first or slim defaults could break callers that parse the full view.** The wake read-boundary semantics must hold. Covered by tests, and the change is announced in `#cm-general`.
- **The level-to-urgency bar mapping is an assumption.** Owner should confirm it. Unset defaults to deliver-all, so nothing changes until Owner sets a level.
- **Held alerts and emergency Telegram.** A misused `emergency` spams the phone: rate-limit and log. Held requests must never be lost: bounded file with explicit overflow errors, as today.
- **Initiative-coordinator lookup depends on the planning API.** If it is unreachable, the wake set falls back to continuous and registered orchestrators, and this is logged.
- **The stall alarm becomes gated.** At `away` the stall alarm is held. That matches §4 but differs from today's behavior. Call it out in `OWNER_NOTIFICATIONS.md`.

---

## Summary (≤300 words)

**A. Messaging.**
- `@Name`: `Store::send` (`daemon/src/messaging/store.rs:1411`) only reads ID `mentions`. Plan: resolve names in `mentions[]`, promote body `@tokens` that match conversation members, and return `warnings` and `mentions_resolved` for the rest.
- Inbox: ascending order is the default (`store.rs:1905`). The stuck "179" is `monitor_status.unacknowledged` (`store/watches.rs:269`), which message read receipts never advance. Plan: newest-first and slim by default for inbox/dms, `mark_read_before=T`, and skip already-read hits in the count.
- Send time: return top-level `created_at` and `submitted_at`/`delay_s`, plus an `outbox.pending_sync` count.
- Codex empty `read_last_turn`: confirmed on real rollouts. Codex writes empty `final_answer` messages, and Python `_render_content` returns `""`, which `_last_assistant` (`mcp_server/monitor.py:281`) picks over the real commentary. The Rust parsers already drop these.
- `stale_scopes`: acknowledging only sets the diff base and gates nothing. Plan: emit it only when a document is supplied, with `ack_with`.
- c097938 covers A1, A3, part of A4, and the slim view. Still open: phone escalation for the stall alarm, A2 `pending_sync` stalls, slim as default, and the inbox backlog.

**B. Availability.**
- Stored as a replicated, Owner-only `owner.availability` messaging event. Events without a conversation reach every host. Readable locally in `ping`/`chat_open`.
- Set with TUI F7 (free; F8 is messages, F9 is settings) or `scripts/cm-availability` over SSH.
- `notify_user` gains `urgency`. Alerts below the level's bar go to a separate held file, so old TUIs don't show them, and come back as `delivery:"held"`. Emergencies also go to `notify_command` (Telegram).
- On a level change the delivery tick releases held alerts as one TUI digest and wakes continuous orchestrators, initiative coordinators, board-registered orchestrators, and sessions with held requests.
- The board and stall alarm share one `escalate()`/`withdraw()` interface.
- The level-to-urgency bar mapping is my assumption and needs Owner's confirmation.

**C. Orchestration.** Canonical text in `doc/ORCHESTRATION.md`, deployed to `~/.cm/policies/`. A short `~/.claude/skills/orchestrate-swarm` skill on cm-sessions, the skill-sync source, symlinked for Codex. Core norms also go in `AGENT_GUIDE.md`.

**Phases:** 1 (Codex fix), 2a/2b (messaging), 3 (docs) in parallel; then 4, 5, 6. Each has its own tests and deploy steps.

### Critical Files for Implementation
- /home/lucas/.cm/worktrees/claude-manager-swarm-focused-design-cm/daemon/src/messaging/store.rs
- /home/lucas/.cm/worktrees/claude-manager-swarm-focused-design-cm/daemon/src/owner_attention.rs
- /home/lucas/.cm/worktrees/claude-manager-swarm-focused-design-cm/daemon/src/messaging/rpc.rs
- /home/lucas/.cm/worktrees/claude-manager-swarm-focused-design-cm/mcp_server/server.py
- /home/lucas/.cm/worktrees/claude-manager-swarm-focused-design-cm/mcp_server/transcripts/codex.py
