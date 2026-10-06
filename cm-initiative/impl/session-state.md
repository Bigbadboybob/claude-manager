# Implementation plan: reliable session state

Repo `/home/lucas/.cm/worktrees/claude-manager-swarm-focused-design-cm` @ `56ada60`. This builds on `idle-research/claude-code.md` and `codex.md`. I re-checked the claims below against the code.

## 0. Facts checked in the code that shape the plan
- **Claude settings are frozen at launch.** `daemon/src/mcp_config.rs:363` `claude_settings_hook_arg` registers **Stop only**. The `--settings` argv stays fixed for the life of the process, but the command is `launcher 'hooks/cm_stop_hook.py'`, which resolves against the *current* payload. So **editing `cm_stop_hook.py` reaches existing Claude sessions**, while **registering new hook events needs a respawn (A-R)**. The TUI uses the same function (`tui/src/mcp_config.rs:301`).
- **The status file is pinned to CM's pid.** A live `~/.claude/sessions/<pid>.json` has a pid whose parent is `cm-holder`, so `DaemonSession.pid` (`session.rs:1130`) is the claude pid. `procStart` equals field 22 of `/proc/<pid>/stat`. The daemon already has `proc_starttime` (`adopt.rs:433`). `.key` files sit beside the JSON and must be ignored.
- **Every keystroke counts as input.** The attach stream calls `write_and_stamp_operator` (`control/stream.rs:689`), which stamps `last_input_at` (`session.rs:2835`) for every byte. Three things read `last_input_at`: `semantic_idle()` (`session.rs:3002`), `reported_done()` (2979, so a stray arrow key cancels a done report), and the codex drain's `after` value (`control/continuous_drain.rs:52`).
- **Codex reports can arrive out of order.** `native_codex.py:187` `Relay.report()` starts one task per event, each retrying with 0.5 s sleeps, so arrival order is not guaranteed. The relay's Python is loaded once at launch, so **existing Codex sessions keep the old relay until A-R**.
- **TUI attention uses the screen-activity status only.** `SessionStatus{Running,Idle}` (`tui/src/app/model.rs:26`) drives glyphs (`draw.rs:1411,1929`), rollups (`draw.rs:2419`, `sections.rs:339`), A-g (`nav.rs:53,1593`), notify-on-idle and pending-prompt delivery (`events.rs:700-760`).
- **There is a pattern for partial `manifest.watch` updates.** `ManifestDiff::Updated{uid, entry}` with partial keys already exists (`owner_attention`, `events.rs:2163`), and the snapshot is built in `control/dispatch.rs:~2528`. New keys are ignored by old TUIs, so **no new enum variant is needed**.
- **Restart persistence lives in holder-proto.** `holder-proto/src/reexec_manifest.rs:307 SessionRecord` is shared with the near-frozen holder, so the plan adds **no new fields there** (see S1 for the sidecar).
- **Test isolation.** `scripts/cm-test-isolated` binds `~/.cm` only, so the presence root must be injectable in tests.
- **Codex version.** The fleet is held at 0.153.4 (`codex-update-config hold_version`). Fixtures are in `daemon/tests/fixtures/codex-0.153.4/`.

## 1. The state model (one contract for all consumers)
`AgentState` lives in new file `daemon/src/agent_state.rs` and is serialized on the wire as `agent_state`:
```
{state: working|working-background|waiting-on-human|errored|idle|starting|exited|unknown,
 since: unix_s,            # entered this state; == idle_since when idle
 detail: {waiting_for?, error_kind?, resumes_at?, retrying?, open_tool?},
 source: presence|hooks|relay|transcript|pty, observed_at, engine_version?,
 turn_seq: u64,            # ++ on every turn start (prompt submit, turn/started, agent send_input)
 last_turn: {ended_at, status: completed|interrupted|failed|null},
 background: {complete: bool, observed_at, jobs:[{id, kind: shell|subagent|monitor|workflow|terminal|thread,
              label, pid?, cpu?, first_seen_at, wakes_agent}], crons:[{id, schedule, recurring}],
              ended:[{id,label,ended_at}] (ring of 10)},
 stalled_since?: unix_s}
```
**Precedence**, implemented as one pure function `derive(inputs, now)`:
1. `exited`: holder pidfd or tombstone.
2. `starting`: no engine report and no transcript yet, within 120 s of spawn.
3. `unknown`: an engine source was valid before but is now missing or invalid (Claude status file gone, unparsable, or `procStart` mismatch while the pid is alive), or the Codex relay heartbeat is older than 90 s or it reports `backend_connected=false`. Show `unknown`; never treat it as idle.
4. `waiting-on-human`:
   - Claude: status file says `waiting`, or a PermissionRequest/Notification edge is newer than the last prompt-submit/Stop.
   - Codex: a pending approval, `requestUserInput` or elicitation request on any thread in the tree, or `activeFlags` set. `item/tool/call` and auth refresh do not count.
5. `errored`:
   - Claude: StopFailure after the last prompt-submit, or the status file says idle and the transcript tail is a synthetic API error. Classify with the existing `continuous/probe.rs::probe_transcript_tail`.
   - Codex: last `turn/completed.status=failed`, or `systemError`.
   - Cleared by the next turn start.
6. `working`:
   - Claude: status file `busy` and the main turn is open (prompt-submit newer than Stop/StopFailure; without hooks, transcript tail `MidTurn`).
   - Codex: foreground thread `active`; `detail.retrying` when the last `error.willRetry`.
7. `working-background`:
   - Claude: status file `busy` with the turn closed, or `shell`.
   - Codex: foreground thread idle while a child thread is active or the background-terminal list is non-empty. Background terminals carry `wakes_agent=false`.
8. `idle`, with `since` set as follows:
   - Claude: the status file's `statusUpdatedAt`.
   - Codex: the `turn/completed` time.
   - Fallback: the last turn-end edge.

**Fallback for old sessions** (`source=pty`/`transcript`) applies to bash, Claude with no status file ever seen within 60 s, embedded Codex, and pre-upgrade relays. It produces only `working`/`idle`/`starting`, from the PTY plus the fixed `semantic_idle`. Transitions from working to idle are debounced 1.5 s, but `since` keeps the true time. `stalled_since` is set when working\* shows no transcript or subagent-dir mtime growth for 15 min. It is an overlay, not a state.

**Compatibility:** `idle` on the wire becomes `state ∉ {working, starting}` when the engine reports state, and the PTY heuristic otherwise. Add a new field `pty_idle` that carries the raw PTY value. `semantic_idle` keeps working.

## 2. Phased slices (each can be merged on its own)

### S0: stop viewer input from resetting idle (daemon, small)
- `session.rs`: `write_and_stamp_operator` always stamps `last_activity_at` and `last_operator_input_at`. It stamps `last_input_at` only when the bytes contain a submit key (`\r`, or kitty `CSI 13 u`/`CSI 13;…u`). Esc, arrows, mouse, focus and query replies no longer stamp it.
- No change to `stream.rs` itself; the fix is in the handle.
- Tests (in `methods.rs` tests near 7214): arrow, mouse, Esc and draft input leave `semantic_idle`/`reported_done` alone; Enter supersedes them.
- Deploy: brain restart. Delivery timing is unchanged because `await_operator_quiet` and the tracker paths read different cells.

### S1: daemon core: state cell, derivation, publish, one write RPC (daemon)
- **New `daemon/src/agent_state.rs`** containing:
  - input types: `PresenceObs`, `HookEdges`, `RelaySnapshot`, `LegacyObs`;
  - `derive()`;
  - `AgentStateCell` (an `Arc<Mutex<…>>` on `DaemonSession`, `session.rs:959`);
  - `recompute_and_publish(state, uid)`, which broadcasts `ManifestDiff::Updated{uid, entry:{"agent_state":…}}` only when `(state, detail, background)` changed;
  - a 1 s tick thread (handles staleness to `unknown`, the debounce and `stalled_since`).
- **New RPC `session.agent_report`**, wired in `control/dispatch.rs` near :613 and self-scoped like `session.turn_ended`. It has two kinds:
  - `{kind:"hook", event, payload…}` for Claude.
  - `{kind:"snapshot", epoch, seq, …}` for Codex. It is applied only if `epoch` is new or `seq` is greater than the last seen. It also stamps the legacy cells (`stamp_turn_end` / `stamp_activity`) and does the transcript rebind/`observe_codex_rollout_once` that `session_turn_ended` (`methods.rs:2839`) does today, so old consumers keep working.
- **Wire changes in `methods.rs`:**
  - `resolve_authorized_session` (:2746) and `list_sessions` (:2455) add `agent_state` and `pty_idle`; `idle` is recomputed as in §1.
  - The tombstone path returns `{state:"exited"}`.
  - `daemon.health` (:3721) gains `sessions_by_state`.
- **`dispatch.rs:~2528`:** the `manifest.watch` snapshot gets `"agent_states": {uid: …}`.
- **Persistence:** a brain-side sidecar `~/.cm/daemon-agent-state.json`, keyed by uid + `child_start_time`, written debounced and loaded at adopt. It keeps `since`, `turn_seq`, hook edges and background across `daemon.restart` without touching holder-proto.
- **Tests:** a table of `derive()` cases for every row of research §3 (both engines); seq/epoch ordering; that `Updated` diffs are emitted only on change; sidecar round-trip; manifest snapshot test (`stream.rs:1928` style).
- Deploy: brain restart. With no engine sources yet, behaviour equals today's (`source=pty`).

### S2: Claude status-file reader (daemon)
- **New `daemon/src/claude_presence.rs`.**
  - Root is `$CLAUDE_CONFIG_DIR` or `~/.claude`, joined with `/sessions`. It can be injected in tests.
  - Each tick it `stat`s `<pid>.json` for every live `claude-code` session and re-parses only when the mtime changed.
  - It checks `pid == session.pid`, `procStart == proc_starttime(pid)` and `kind == "interactive"`, and records `version`.
  - If the file for the pid is missing, it scans the directory for a file whose pid descends from `session.pid` (for wrapper launches).
  - Fields are optional and unknown `status` values map to `unknown`; it never infers idle.
- The transcript error probe (`probe.rs`) runs only when the status file says `idle` and the transcript mtime changed.
- **Fixtures** in `daemon/tests/fixtures/claude-presence-2.1.291/`: busy, idle, waiting(permission prompt / input needed / worker request / dialog open), shell, dead pid, procStart mismatch, unknown status, `tempo` extra fields, truncated JSON. Run with `scripts/cm-test-isolated`.
- Deploy: brain restart. **Existing Claude sessions benefit immediately; they need no action.**

### S3: Claude hooks (daemon `mcp_config.rs` + `mcp_server/hooks/`)
- `cm_stop_hook.py`: forward `background_tasks`, `session_crons`, `last_assistant_message` (truncated) and `stop_hook_active` via `session.agent_report{kind:"hook",event:"Stop"}`. If the daemon returns method-not-found, fall back to `session.turn_ended`, since the payload and brain can be out of step. Keep the inbox and `continuing` behaviour unchanged.
  - **This reaches existing sessions once the MCP payload is copied.**
- **New `hooks/cm_state_hook.py`**: generic and fail-open. It reads stdin, double-forks the RPC and exits in under 50 ms, so it never blocks a prompt. It handles `UserPromptSubmit` (`source`, `prompt_id`), `StopFailure` (`error`, `error_details`), `PermissionRequest` (`tool_name`) and `Notification` (`notification_type`). It prints nothing, so it never makes a permission decision.
- `mcp_config.rs:363`: emit these events alongside Stop (Stop stays synchronous, timeout 15). Add `"async": true` on the new events if 2.1.291 accepts it in settings; the double-fork keeps the hook non-blocking either way. Extend tests at :1120.
- Tests: `mcp_server/tests/test_stop_hook.py` (forwarding plus fallback), new `test_state_hook.py` with captured payload fixtures for each event (`mcp_server/tests/fixtures/claude-hooks/`).
- Deploy: payload copy plus brain restart. **New events only reach sessions spawned or A-R'd afterwards.** Without them, the status file and transcript probe still produce correct state, just with less exact edges.

### S4: Codex relay as the authoritative publisher (`mcp_server/`)
- **New `mcp_server/codex_state.py`**: a pure model with no I/O, fed each upstream message and each reply to the relay's own calls. It tracks:
  - foreground thread status (`thread/status/changed`, plus `status` from `thread/start|resume|fork|read` replies). If no status notification has been seen (to check on 0.153.4), status is derived from `turn/started`/`turn/completed`.
  - turn id/status, the last `turn/completed{status,error.codexErrorInfo}`, and the last `error{willRetry}`;
  - pending server requests by class, with `threadId` and age (from the existing `server_requests`);
  - child threads (`thread/started` whose `parentThreadId` is in the tree) and their status;
  - background terminals.
- **`native_codex.py`**:
  - Replace `report()` with a **single-flight latest-value publisher task**: a dirty event plus a 30 s heartbeat; each send carries `epoch` (a uuid per relay) and a monotonic `seq`; retry/backoff always resends the newest snapshot. This fixes the out-of-order report bug.
  - Hook the model into `read_connection` (:284).
  - Poll `thread/backgroundTerminals/list` through `self.call` (reserved ids are not forwarded) on `turn/completed` and every 30 s.
  - **Detect support by version**: a method error marks it unsupported, and the snapshot then says `background.complete=false`.
  - Republish after `reconnect()` and when the daemon RPC fails (the brain restarted).
- **Optional wake (S4b, behind `daemon.toml codex_bg_wake=true`, default off):** when a listed terminal disappears and the foreground thread is idle, enqueue a notice through the existing durable `Queue` (the same path as monitor wakes) with the command, pid and duration. Deduplicate by `processId`.
- Tests:
  - new `test_codex_state.py` with notification-sequence fixtures (`mcp_server/tests/fixtures/codex-app-server/`, built from `idle-research/codex-upstream/schema/v2`): approval, user input, failed with usageLimitExceeded, interrupted, retry, child active while parent idle, terminal list appear/disappear, unsupported method;
  - extend `integration_native_codex_protocol.py` with an ordering test (completed acknowledged before started must still end idle).
- Deploy: payload copy. **Existing Codex sessions need A-R to load the new relay.** Until then they use the S0-fixed fallback.

### S5: Codex rollout parsers (daemon `continuous/`)
- `codex_probe.rs`: replace the `_ => return None` arms (:117-150) with an allowlist that skips 0.160 record kinds. Skip: `world_state`, `turn_context`, `compacted`, `thread_settings_applied`, `thread_goal_updated`, `inter_agent_communication_metadata`, `response_item/agent_message`, `item_completed{ContextCompaction, SubAgentActivity, FileChange, CollabAgentToolCall, Extension, ImageView}`. A truly unknown record still returns `None`.
- `completion.rs:178`: skip the same bookkeeping records after `task_complete`.
- Where a relay snapshot exists, `codex_turn_finished_after` and the scheduler (`scheduler.rs:1429,1986`) should prefer `agent_state.last_turn`.
- **Fixtures**: add `daemon/tests/fixtures/codex-0.160.1/` (scrubbed real rollouts) and keep 0.153.4. Both sets must pass.

### S6: MCP consumers (`mcp_server/`)
- `monitor.py`:
  - `_session_status` (:26) takes `agent_state`. Mapping: working → `working`; idle → `awaiting_input`; working-background → `awaiting_input` with `agent_state` set and the existing background caveat; waiting-on-human → **`needs_human`** (new); errored → **`errored`** (new); unknown → **`unknown`** (new); `reported`, `starting` and `exited` are unchanged.
  - Loop (:476-509): when `source ∉ {pty}`, done means `state ∈ {idle, errored, waiting-on-human}`, or working-background after a turn end. The edge is `turn_seq > baseline.turn_seq`, which replaces the transcript fingerprint. The transcript/semantic path (:113, :494) is used only for `source=pty`.
- `async_monitor.py:137`: same arm logic; the baseline includes `turn_seq`.
- `server.py` call sites to change: `list_sessions` (:1093, also return `agent_state`), `read_session_output`/`read_last_turn` (:1847,1931), `wait_for_session_idle` (:2319-2357), `_await_reply` (:2585-2649), `wait_for_any_session_idle` (:2863), `wait_for_workflow_stop` stuck check (:2190; use `state==idle`, not `idle`).
- `AGENT_GUIDE.md`: the new status words, and that `needs_human` is not "done".
- Tests to extend: `test_session_monitor.py`, `test_wait_for_session_idle.py`, `test_final_monitors.py`, `test_async_monitor.py`, `test_semantic_idle.py` (resolved dicts with and without `agent_state`, to show both paths).
- Deploy: payload copy. **Orchestrators reconnect MCP**; old MCP servers keep working on the recomputed `idle`.

### S7: daemon consumers of the old idle (daemon)
| Consumer | Decision |
|---|---|
| `continuous_drain.rs:57-69` (claude `semantic_idle`, codex rollout `after` = `last_input_at`) | **Migrate**: `state ∈ {idle, errored}` when engine-reported. Otherwise keep the old check, which S0 already fixed. |
| `methods.rs:3721` `sessions_mid_turn` | **Migrate**: `state ∈ {working, working-background}` when engine-reported, else the old rule. |
| `methods.rs:16720` terminal-task sweep (`compute_session_state_and_idle().1`) | **Migrate**: kill only when `state ∈ {idle, errored, waiting-on-human}`, or `unknown`/`pty` and PTY-idle. Never kill working-background. |
| `continuous/migration.rs:208` report | Add `agent_state` as a field. |
| `workflow/poller.rs:1029` on_idle (transcript `assistant_turn_completed_since`) | **Keep**. Add a gate behind `workflow_state_gate` (default on after a soak): skip while working, working-background or waiting-on-human. |
| `workflow/finalize.rs`, `fresh_reset.rs`, `methods.rs:7690-7825` delivery (`pty_tracker::quiet_for`), `claude_channels.rs:64` | **Keep**: these are prompt-delivery timing. |
| `continuous/probe.rs`, `scheduler.rs` wedge/auth | **Keep**. They are reused by S2. |

### S8: TUI (`tui/`)
- `model.rs`: `TerminalSession.agent_state: Option<AgentStateWire>`. It is applied from `Updated.entry.agent_state` in `events.rs:2163` (helper next to `apply_owner_attention`) and from the snapshot's `agent_states`.
- Add a `ts.display_state()` helper: the daemon value if present, otherwise `SessionStatus`. **`SessionStatus` stays for pending-prompt/clear delivery and border animation** (`events.rs:700-760`).
- `draw.rs` glyphs (:1411, :1929):
  - working: spinner (green);
  - working-background: a dim spinner or `◐` with a job count;
  - waiting-on-human: `?` (yellow, bold);
  - errored: `✗` (red), with the kind on hover/label;
  - unknown: `·` (grey);
  - idle: the existing afterglow buckets, using `since`.
- Rollups: `draw.rs:2419`, `sections.rs:339` add ⚠/✗ counts.
- `nav.rs:53` A-g priority: alerts, then waiting-on-human, then errored, then idle (sorted by `since`).
- notify-on-idle fires on the daemon transition into idle or waiting-on-human. PTY transitions do not notify when `agent_state` is present.
- `tui/src/control/methods.rs:375`: pass `agent_state` through.
- Tests: the A-g picker table in `nav.rs:2174`, glyph mapping, and that diff application is idempotent.
- Deploy: `doc/TUI_RELEASES.md` (`cm-tui-release` skill). It is safe to ship after S1.

### S9: long-job feed for the board
- Data comes from S1 (`background`, `ended` ring, `crons`, `last_turn`) plus S3/S4. The board planner reads `list_sessions`/`manifest.watch`. It needs no new RPC unless it needs cross-host push.
- **"Declared waiting but nothing running"**: `state==idle` and `background.complete` and `jobs==[]` and `crons==[]`.
- **"Job finished but agent idle"**: an `ended[].ended_at > since` with `state==idle`. Claude marks `wakes_agent=true`, so this holds only if no turn followed. For Codex terminals it is always a candidate, which is where S4b's wake helps.
- **Undeclared long background work**: working-background with `first_seen_at` older than N.

## 3. File ownership for parallel workers
Land the shared contract first: S1's `agent_state.rs` types plus a short `doc/SESSION_STATE.md` wire spec.
- **A (daemon core):** `agent_state.rs`, `session.rs`, `control/methods.rs`, `control/dispatch.rs`, `control/continuous_drain.rs`, `workflow/poller.rs` (gate), `continuous/migration.rs`. Covers S0, S1, S7.
- **B (Claude):** `claude_presence.rs`, `mcp_config.rs`, `mcp_server/hooks/*`, Claude fixtures. Calls only `agent_state::apply_presence` / `apply_hook`. Covers S2, S3.
- **C (Codex):** `mcp_server/native_codex.py`, `codex_state.py`, `continuous/codex_probe.rs`, `continuous/completion.rs`, Codex fixtures. Covers S4, S5.
- **D (MCP):** `monitor.py`, `async_monitor.py`, `server.py`, `AGENT_GUIDE.md` and their tests. Covers S6.
- **E (TUI):** `tui/src/app/{model,events,draw,nav,sections}.rs`, `tui/src/control/methods.rs`. Covers S8.

Only A touches `methods.rs`. B and C register through A's API.

## 4. Testing and live verification
- Rust: `CARGO_TARGET_DIR=/tmp/cm-ss-target scripts/cm-test-isolated cargo test -p cm-daemon agent_state claude_presence codex_probe completion -- --test-threads=2`, plus `-p claude-manager-tui`. Python: `uv run pytest mcp_server/tests -k "state or hook or monitor or wait or codex_state"`.
- **Scratch daemon** (memory `reference_scratch_daemon_sandbox`: isolated `HOME=$SB/home`, `CM_DAEMON_SOCKET`). Note that Claude then writes `$SB/home/.claude/sessions`. Checklist:
  1. Claude busy turn → `working`. Turn end → `idle` with `since` equal to `statusUpdatedAt` (within 1 s).
  2. `Agent(run_in_background)` then end the turn → `working-background`, jobs listed. When it completes → working, then idle; `ended` gets an entry.
  3. `sleep 600 &` background Bash → `shell` → working-background with kind shell.
  4. AskUserQuestion → `waiting-on-human`. Repeat on a session started without `--dangerously-skip-permissions` to see the permission prompt.
  5. Esc mid-stream → `idle`, not stuck.
  6. Typing a draft or arrow keys in the attach → state unchanged and `reported_done` kept.
  7. Bad API key or forced 401 → `errored(authentication_failed)`; StopFailure seen in the daemon log.
  8. `kill -STOP` claude → stays `working` and `stalled_since` appears after the threshold (shorten it for the test). Delete the status file → `unknown`.
  9. Codex: long `sleep 30` exec → working. With approvals on, an approval → waiting-on-human. A background terminal (`unified_exec`) → working-background with pid. Terminal exits → `ended`, plus a wake when S4b is on. Forced failed turn → errored. Kill the relay socket → `unknown` within 90 s.
  10. `daemon.restart` on the scratch daemon → states and `since` survive (sidecar), and the relay republishes.
  11. On both 0.153.4 and 0.160.1, check whether `thread/status/changed` arrives and whether background-terminal support is detected.
  12. Prompt delivery: `send_input` to idle Claude and Codex still submits (`submitted=true` and a turn starts).

## 5. Rollout
Order: local → cm-sessions → cm-manager. For each host, stage the brain binary plus the **complete `mcp_server/` payload** (including `AGENT_GUIDE.md` and `hooks/`), preflight, then `daemon.restart` (`scripts/cm-redeploy` locally; `scripts/cm-op --ssh <host> daemon.restart` remotely). Verify with `daemon.health` (`holder_epoch` +1, `breaker_state=running`, new `sessions_by_state`). The TUI ships separately after S1.

What existing sessions need:
| Session type | After S0–S2 | After S3 | After S4 | After S6 |
|---|---|---|---|---|
| Claude (existing) | nothing; the status file is read immediately | nothing for Stop background-job forwarding; **A-R** for new hook events (optional) | – | **MCP reconnect** for new status words and monitors |
| Codex native (existing) | fallback, with the S0 fix | – | **A-R** (resume) to load the new relay; until then `source=pty` | MCP reconnect |
| Embedded/legacy Codex, bash | `source=pty` forever; restart to native | – | – | – |
| Continuous workers | pick it up on their next spawn | same | same | same |

## 6. Risks
- **Undocumented status file** (`~/.claude/sessions`): fields or location can change. Mitigations: defensive parsing; never infer idle; record `engine_version`; log when the status file and hooks disagree; the hooks plus transcript fallback; check upstream on every Claude update (add it to the update-check notes).
- **Experimental Codex API** (background terminals, child-thread subscription scope): detect support by method error and set `complete=false`. Child-thread notifications may not reach the shared connection; fallback is `thread/read` of known child ids every 60 s.
- **`working-background` that never ends:** Claude `shell` with a dev server, and plugin monitors (`kind:"monitor"`). The board gets the job list; the policy (ignore-list) belongs to the board.
- **Status word changes** (`needs_human`, `errored`, `unknown`) could break orchestrator prompts that pattern-match on `awaiting_input`. Document it, keep `idle` for compatibility, and roll out S6 after S1–S3 have soaked.
- **Hook cost or blocking:** the double-fork keeps it under 50 ms. A misconfigured PermissionRequest hook could auto-decide; ours never prints.
- **Mixed versions:** an old MCP payload against a new brain is fine (additive fields). A new hook against an old brain falls back to `turn_ended`.
- **S0 submit detection:** a bracketed paste containing CR counts as a submit, which over-counts input. That is harmless when the status file or relay is the primary source.
- **Prompt delivery into a waiting-on-human session** could answer the dialog. Follow-up: have `send_input` warn or queue when the state is waiting-on-human. This is not in these slices.

### Critical files for implementation
- `/home/lucas/.cm/worktrees/claude-manager-swarm-focused-design-cm/daemon/src/control/methods.rs`
- `/home/lucas/.cm/worktrees/claude-manager-swarm-focused-design-cm/daemon/src/session.rs`
- `/home/lucas/.cm/worktrees/claude-manager-swarm-focused-design-cm/mcp_server/native_codex.py`
- `/home/lucas/.cm/worktrees/claude-manager-swarm-focused-design-cm/daemon/src/mcp_config.rs` (+ `mcp_server/hooks/cm_stop_hook.py`)
- `/home/lucas/.cm/worktrees/claude-manager-swarm-focused-design-cm/mcp_server/monitor.py`

---

**Summary (≤300 words):** The plan adds one engine-reported state per session, built in a new `daemon/src/agent_state.rs` that applies a fixed precedence: exited, starting, unknown, waiting-on-human, errored, working, working-background, then idle. The result is published as an `agent_state` object (with `since`, `turn_seq`, `last_turn`, `background{jobs, crons, ended}` and `stalled_since`) through `list_sessions`, `resolve_authorized_session` and `manifest.watch` `Updated` diffs plus the snapshot. It needs no new diff variant, no holder-proto change (a brain-side sidecar keeps it across restarts), and one new write RPC, `session.agent_report`, used by both Claude hooks and the Codex relay.

Checks in the code that change the rollout:
- **Claude:** editing `cm_stop_hook.py` reaches existing sessions through the launcher, but new hook events need A-R. The status file can be read immediately with no session action.
- **Codex:** the relay is loaded when the session starts, so existing Codex sessions need A-R.
- **Viewer keystrokes:** they reset `semantic_idle`, `reported_done` and the codex drain. S0 fixes this by stamping input only on submit.

There are ten slices (S0–S9). The order is S0 then S1, which is the shared contract. After that, S2/S3 (Claude), S4/S5 (Codex), S6 (MCP), S7 (daemon consumers) and S8 (TUI) can be built by five separate workers that don't share files.

S7 decides for every daemon consumer of the old idle signal:
- **Migrate:** continuous drain, mid-turn counts, the terminal-task sweep.
- **Keep:** prompt-delivery timing and the workflow `on_idle` check (which gains a gate).

The main risks are that the status file format can change without notice, that the Codex background-terminal API is experimental (so support is detected per version), and the new status words (`needs_human`, `errored`, `unknown`).

