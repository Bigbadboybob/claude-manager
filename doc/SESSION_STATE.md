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
`idle` boolean is `state` outside `{working, starting}`; fallback retains the
existing PTY boolean. `semantic_idle` remains available. Consequently `idle=true`
is a compatibility signal, not proof of true idle: new consumers must inspect
`agent_state.state`, especially for waiting, error, background work and unknown.
The board's holder-idle clock counts only `state=idle`.
