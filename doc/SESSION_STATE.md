# Session state wire contract

Contract for the staged reliable-session-state rollout. The daemon owns one
`agent_state` per session; consumers use it instead of deriving engine activity
from terminal repainting. Old daemons may omit it. Ignore unknown object keys;
an unrecognized state must be treated as `unknown`, never as idle.

## Wire shape

Live sessions carry this object on `list_sessions` and
`resolve_authorized_session`. `manifest.watch` snapshots carry
`agent_states: {session_uid: agent_state}`; updates use the existing
`Updated {uid, entry: {"agent_state": ...}}` diff. An exited tombstone may carry
only `{"state":"exited"}`; consumers must accept that shorter shape.

```text
agent_state = {
  state: working | working-background | waiting-on-human | errored |
         idle | starting | exited | unknown,
  since: unix_s,
  detail: {waiting_for?, error_kind?, resumes_at?, retrying?, open_tool?},
  source: presence | hooks | relay | transcript | pty,
  observed_at: unix_s,
  engine_version?: string,
  turn_seq: u64,
  last_turn: {ended_at: unix_s | null,
              status: completed | interrupted | failed | null},
  background: {
    complete: bool, observed_at: unix_s | null,
    jobs: [{id: string, kind: shell | subagent | monitor | workflow |
                             terminal | thread,
            label: string, pid?: integer, cpu?: number,
            first_seen_at: unix_s, wakes_agent: bool}],
    crons: [{id: string, schedule: string, recurring: bool}],
    ended: [{id: string, label: string, ended_at: unix_s}]
  },
  stalled_since?: unix_s
}
```

This is schema notation: `?` means optional. Timestamps are Unix seconds
(fractional seconds allowed). `since` is the time the current state began,
not the latest poll time; while idle it is `idle_since`. `observed_at` is the
source observation time. Unobserved turn/background times are null, not zero.
`detail` is empty when no detail is known: `waiting_for`, `error_kind` and
`open_tool` are strings, `resumes_at` is Unix seconds, and `retrying` is boolean.
`cpu`, when supplied, is the engine-reported CPU percentage.

`turn_seq` increases on each new turn start (prompt submit, relay `turn/started`,
or accepted agent `send_input`); viewer navigation and drafts are not turns.
`last_turn` describes the most recent ended turn, including interruption or
failure, independently of the current state. A done report remains the separate
`reported_done` signal: an idle session has not necessarily finished its work.

`background.complete=false` means enumeration is unavailable or partial; empty
lists then do not prove that nothing is running. `ended` retains the latest ten
observed job endings. `wakes_agent=false` for Codex background terminals; live
background work does not establish whether that work is still relevant to an item.

## Derivation and freshness

One pure `derive(inputs, now)` applies this precedence, highest first:

| State | Evidence |
|---|---|
| `exited` | Holder exit or tombstone. |
| `starting` | No engine report or transcript yet, within 120 seconds of spawn. |
| `unknown` | Previously valid engine source missing/invalid, PID identity mismatch, relay heartbeat older than 90 seconds, or relay backend disconnected. |
| `waiting-on-human` | Claude waiting status or permission/notification edge newer than prompt-submit/Stop; Codex pending approval, user input or elicitation in its thread tree, or waiting flags. Tool calls and auth refresh alone do not count. |
| `errored` | Claude StopFailure after the last prompt-submit, or idle presence plus synthetic transcript API error; Codex failed turn or system error. Cleared by the next turn start. |
| `working` | Claude busy with its main turn open (hook edges, otherwise transcript); Codex foreground active, with `retrying` detail when applicable. |
| `working-background` | Claude busy with its turn closed, or shell status; Codex foreground idle with active children or background terminals. |
| `idle` | Engine idle; `since` comes from Claude `statusUpdatedAt`, Codex turn completion, or the fallback turn-end edge. |

Legacy fallback (`source=pty` or `transcript`) covers bash, embedded Codex,
pre-upgrade relays, and Claude with no presence file ever seen after 60 seconds.
It uses PTY activity plus `semantic_idle`, yielding `working`, `idle` or
`starting` while alive. Working-to-idle transitions are debounced 1.5 seconds,
preserving the original idle time. A previously valid engine source becoming
unreadable yields `unknown`, rather than falling back to a guessed idle.

`stalled_since` overlays either working state after 15 minutes without transcript
or subagent-directory growth. It does not replace the state. `since`, `turn_seq`,
hook edges and background survive a brain restart in a daemon-side sidecar keyed
by session UID and child start time; they must not transfer to a replacement child.

## Compatibility

`pty_idle` always exposes raw PTY quietness. For engine-reported state, the legacy
`idle` boolean is `state` outside `{working, starting}`; `unknown` and fallback
retain the existing PTY boolean. `semantic_idle` remains available. Consequently `idle=true`
is a compatibility signal, not proof of true idle: new consumers must inspect
`agent_state.state`, especially for waiting, error, background work and unknown.
The board's holder-idle clock counts only `state=idle`.

## Initial prompt confirmation

For an initial Claude or Codex prompt, `mcp_start_session` returns a per-launch
`prompt_delivery` receipt. It does **not** add a top-level `submitted` field:
old MCP callers retain their existing response behavior after a brain upgrade.
The same receipt is available through `resolve_authorized_session`:

```text
{id: UUID, status: pending | confirmed | delivered | unconfirmed, submitted: bool,
 attempts: 0 | 1 | 2, confirmed_by: relay | hooks | presence | transcript | write | null,
 reason: string | null}
```

The MCP tool's default `wait=false` returns promptly after the daemon accepts
the launch, usually with a pending receipt and no top-level `submitted`. Its
auto-monitor checks that receipt on the original daemon before watching for
completion. Failed confirmation produces an uncertainty notification, not a
completion report. With `notify_on_done=false`, `read_last_turn` and
`read_session_output` expose the current receipt for inspection.
`wait=true` checks confirmation before waiting for the worker's reply. Only a
terminal receipt adds top-level `submitted`; pending does not mean failure.
Transient socket timeouts and restart conflicts retry within a 360-second poll
budget covering startup, the existing operator-quiet wait and confirmation.

A fresh engine turn edge or a new prompt record confirms submission. Claude's
new main-thread user text counts even if the engine expanded or normalized it;
sidechain, metadata and tool-result rows do not. Codex retains exact matching.
CM's own `turn_seq` increment and PTY repaint alone do not confirm. Slash commands
are `delivered` after body plus Enter (`confirmed_by=write`), without requiring a
model turn; their monitor remains armed.

Delivery restores the existing typing-quiet wait. Recognized terminal query
replies, mouse/focus and navigation do not count as typing; drafts and submits
still do. Confirmation gets 90 seconds **from the body write**, independently of
startup delays, with one Enter retry after ten seconds using the original
encoding. It never re-pastes the body. A human draft after the paste suppresses
the retry but does not prevent later evidence from confirming. Unrelated or
unreadable transcript activity and restart pause defer recovery while observation
continues; process replacement/exit ends it. Same-process transcript rebindings
(including repeated detector/hook corrections) defer for an observation, then
follow the new path while retaining the original write-time evidence cutoff.

`submitted=false` means confirmation failed, not proof the engine received
nothing. Inspect state/transcript before re-sending. The session and worktree
remain available; no reply wait or completed-work claim follows failure.
Successful launches use a level completion watch, retaining a first turn or done
report that finished during confirmation. Receipts are ephemeral: a daemon
restart can lose the receipt, producing an unconfirmed result explicitly labeled
unknown, not failed. Old daemons omit
these fields. Promptless and bash launches retain existing behavior. Deployment
needs the brain and complete MCP payload; callers reconnect MCP for the new
background confirmation handling.

## Producer RPC (S1)

`session.agent_report` accepts a Session caller only for its own `session_uid`;
control permission over descendants does not allow reporting their engine state.
Operator callers may report for diagnostics. Hooks must target a `claude-code`
session and snapshots a `codex` session. Reports larger than 256 KiB, unknown
kinds/events/statuses, invalid timestamps and mismatched engines are rejected.
During a brain restart the RPC returns conflict; retry the latest report.

```text
{session_uid, kind: "hook", event: UserPromptSubmit | Stop | StopFailure |
                                  PermissionRequest | Notification,
 payload: {observed_at?, prompt_id?, continuing?, transcript_path?,
           waiting_for?, error_kind?, resumes_at?, tool_name?, notification_type?,
           source?, stop_hook_active?, last_assistant_message?, error_details?,
           background?}}

{session_uid, kind: "snapshot", epoch: UUID, seq: u64,
 snapshot: {backend_connected: bool, foreground: idle | active | systemError,
            turn_seq: u64, turn_started_at?: unix_s,
            last_turn: {ended_at: unix_s | null, status: completed | interrupted | failed | null},
            engine_version?, transcript_path?, active_flags?: [string], child_active?: bool,
            pending_requests?: [{kind: approval | user_input | elicitation |
                                       tool_call | auth_refresh, since: unix_s}],
            retrying?: bool, error_kind?, background?}}
```

Hook payloads use the normalized fields above; hook adapters translate
engine-specific fields. `observed_at` defaults to receipt time; adapters should
capture it at the event so delayed hooks retain their order. `prompt_id` suppresses
duplicate prompt submissions. Notification edges apply only to
`permission_prompt`, `elicitation_dialog` and `idle_prompt`; unrelated notices
are ignored. A Stop with `continuing=true` starts another turn instead of
advertising semantic idle. Transcript rebinding retains the existing containment
check against the session's workspace.

Snapshots are complete latest values. Missing optional flags/lists mean empty;
missing background means enumeration unavailable. `backend_connected`,
`foreground`, `turn_seq` and `last_turn` are required. The relay's `turn_seq` counts
foreground starts within its epoch, even when publication coalesces start/end
notifications; it must not regress. The daemon's session counter survives relay
epochs and counts CM input plus its corresponding engine start once. A fresh UUID
starts an epoch; within it only increasing `seq` values apply. Retired epochs and
repeated/older sequence numbers are ignored. Freshness uses daemon receipt time,
not a producer-supplied heartbeat timestamp.

The response is `{ok: true, applied: bool, agent_state: ...}`. Ignored duplicates
return `applied=false` without stamping legacy activity or cancelling a done
report. Accepted turn edges also update the legacy semantic-idle clocks. Changes
to state, detail, background, turn counters, last turn or stall markers publish
manifest diffs; observation-only heartbeats do not. Read surfaces and reconnect
snapshots still return current observation times.

State publication is coalesced to the latest value per session and drained at
most eight updates per one-second tick, leaving buffer space for lifecycle
changes. The first observation after brain adoption is seeded silently; each
subscriber's initial snapshot supplies those states. Large simultaneous changes
may take several ticks to reach every viewer; direct reads are current.

The sidecar stores restart facts rather than PTY activity clocks or derived-state
caches. Periodic serialization/fsync runs outside the daemon lock, at most once
per five seconds, and unchanged records do not rewrite the file. A checked
restart flush bypasses that debounce and is ordered after any periodic write.

Producer limits: 256 recent prompt IDs (oldest evicted; event timestamps still
reject late prompts), 64 retired relay epochs, 256 bytes per ID/version/flag,
4,096 UTF-8 bytes per free-text field, 256 jobs/crons/pending requests, 16 active
flags and ten ended jobs. Oversized reports are rejected without applying them.
Retired epoch replay protection is never evicted: after 64 epoch retirements,
a further fresh epoch is refused until the session restarts; reports from the
current epoch continue to work.

## Claude hooks (S3)

The synchronous Stop hook retains its inbox drain and block/reason behavior.
It reports normalized background tasks and session crons, bounded assistant
text, `stop_hook_active` and `continuing` through `session.agent_report`. A
method-not-found, conflict (including brain restart), or invalid-params reply
enables the legacy `session.turn_ended` fallback; transport and authorization
failures do not. A partial payload missing the shared helper sends the legacy
report inline. Accepted continuation reports refresh legacy activity even when
a newer engine start already opened the turn. The report is best
effort and fails open. New UserPromptSubmit, StopFailure, PermissionRequest and
Notification hooks run asynchronously and double-fork before daemon IPC. They
print nothing and make no permission decisions. Events carrying a subagent
`agent_id` are ignored because those hooks inherit the main session's CM identity.

The adapter captures observation time before reading input and translates
StopFailure `error` to `error_kind`. `source`, `stop_hook_active`, assistant text
and error details are accepted metadata; they are not retained or published in
`agent_state`. Text is bounded to 4,096 UTF-8 bytes (source/prompt ID: 256).
Prompts, tool arguments, cron prompts and notification bodies are omitted.
Background tasks become shell/subagent/monitor/workflow jobs with
`wakes_agent=true`; their stable IDs preserve `first_seen_at` across consecutive
reports. Crons carry only ID, schedule and recurrence. Missing lists, unknown
job kinds, malformed entries or truncation mark enumeration incomplete.
Lists are capped at 256 entries and reduced further if necessary to fit the
RPC size limit. Cron schedules alone do not imply running background work.

Deployment needs the brain and complete MCP payload on each session host.
The launcher loads the updated Stop script for existing sessions. The other
events are frozen in Claude's launch settings and require a new session or
A-R; reconnecting MCP alone does not install them. Presence and transcript
observation continue to support sessions with the older launch settings.

## MCP consumers (S6)

Session listings, reads, waits and monitor results expose `agent_state` and
`pty_idle` when supplied by the daemon. Status maps engine `working` to
`working`, `idle` and `working-background` to `awaiting_input`,
`waiting-on-human` to `needs_human`, and `errored`/`unknown` to their own words.
`starting`, `reported` and `exited` retain their meanings. Background results
include the engine detail and a notification caveat; `needs_human` is not done.

A turn wait returns on engine idle, error, human wait, or background work after
a recorded foreground turn end. Working, starting and unknown keep waiting
even when the compatibility `idle` bit is true. Explicit `source=pty` or
`transcript` observations and old daemons retain the existing transcript/PTY
path. Transcript content is still read to return messages; it cannot override
an engine working/unknown state to complete a wait.

Edge monitors anchor on `turn_seq`. Arming during `working` includes the current
turn; arming at a boundary requires a later turn. A new engine `last_turn.ended_at`
also passes an unchanged counter for older Claude sessions that lack a prompt
hook on machine-injected starts. Receipt/heartbeat timestamps never count. A source upgrade re-anchors
old transcript baselines to engine counters. Send-and-wait captures the counter
before sending, so an old reply cannot satisfy the new request. Errors or human
waits can return without assistant text. Schema correction does not send another
prompt while the engine needs attention. `until=final` still requires an explicit
new `report_done` or exit, re-arming past interim turn boundaries.

Deploy the MCP payload and reconnect existing MCP callers for these consumer
changes. The continuation follow-up also needs the updated daemon brain.

## Codex relay (S4/S5)

The native app-server relay owns a pure thread-tree model and one ordered
snapshot publisher. It sends the latest value on changes and every 30 seconds;
daemon failures retry the newest value with bounded backoff. Disconnects report
`backend_connected=false`. Approval/input/elicitation requests stay pending until
the backend resolves them; tool calls and auth refresh do not imply human waits.
Status notifications take precedence over the older turn-event fallback.
State-model or publisher failures are contained and logged; snapshots stop while
native wake delivery and frontend forwarding continue. An old daemon without
`session.agent_report` receives ordered legacy turn edges; capability retries
back off from one to ten minutes. Optional `transcript_path` triggers the existing
ownership scan on path changes; heartbeats alone do not repeat that scan.

Background terminals are polled after turn completion and every 30 seconds with
pagination. A method-not-found response disables polling; failures, incomplete
pages or unavailable support set `complete=false`. Command labels are bounded;
PID/CPU fields are included only when the backend supplies them. Disappearances
between complete enumerations enter the ten-job history. No terminal completion
wake is enabled. The thread/request model is bounded at 256 entries; exhaustion
reports unknown rather than silently dropping live evidence.
That overflow state clears when closed threads or resolved requests free capacity.

Drain completion prefers relay end evidence. Scheduler probes also consult it,
while keeping transcript account/pool errors and unclassified records as holds.
An active, stale, disconnected, waiting or background relay cannot establish
completion. Drain alone returns to the bounded transcript check after 90 seconds
of stale/disconnected relay evidence; the UI remains unknown and scheduler
recovery stays conservative. Legacy relays continue through the rollout parser, which ignores
only known bookkeeping. Unknown records still block old completion evidence.

Deployment requires the complete MCP payload and daemon code. Existing Codex
sessions need A-R to load the new relay; new sessions use it at launch.

## Claude presence reader (S2)

The daemon checks `$CLAUDE_CONFIG_DIR/sessions/<pid>.json`, or
`~/.claude/sessions/<pid>.json`, once per state tick. It requires `kind=interactive`,
the expected PID, and `procStart` matching a live process. A wrapper with no PID
file may use one unambiguous interactive descendant. Dead processes, PID reuse,
malformed files, unknown statuses and a lost previously observed source produce
`unknown`. A partial JSON write is tolerated for one tick, retaining the last
valid observation; two consecutive parse failures yield unknown. Identity
failures are immediate. Claude versions with no presence file retain the existing fallback.
Optional fields and unknown metadata such as `tempo` do not override `status`.
`statusUpdatedAt` is converted from milliseconds to wire-format Unix seconds.

Unchanged files reuse parsed data; process identity is still checked every tick.
Transcript reads are bounded and cached by path and file metadata. Busy sessions
without prompt hooks use the tail to distinguish foreground from background
work; an unreadable tail cannot prove the main turn closed. Error verdicts apply
only when presence says idle, and clear when a newer prompt replaces the error
tail. The reader recognizes synthetic API-error tags plus the existing auth and
usage-limit banners. All file/proc reads run outside the daemon registry lock.

Presence files are limited to 64 KiB; wrapper scans to 4,096 directory entries
and 64 ancestry links. An incomplete or ambiguous scan returns unknown. Existing
Claude sessions gain this source on the next brain restart; no session restart
or MCP reconnect is needed.

After input newer than `statusUpdatedAt`, an unchanged valid presence is
treated as working for up to ten seconds while async prompt delivery settles.
Repeated file observations do not end that guard. An unchanged waiting status
returns to waiting-on-human after the guard, until the engine changes status.

`presence_idle_enabled = true` is the default in daemon.toml. For a quick
compatibility rollback, set it to false and call `daemon.reload_config` (or send
SIGHUP to the brain). Presence sessions then return raw `pty_idle` as their
legacy `idle` boolean; `agent_state` continues to report the engine state.
The MCP workflow stuck check uses explicit `agent_state.state == "idle"` when
available, so background work, waiting and unknown are never declared stuck on
the strength of the compatibility boolean alone.
