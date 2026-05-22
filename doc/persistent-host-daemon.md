# Persistent host daemon

## Summary

Split the TUI in two: a headless `cm-daemon` that owns sessions, PTYs, worktrees, workflow runs, and the MCP socket; and a thin TUI client that talks to it over a JSON-RPC connection. The daemon runs on whatever host the work lives on — initially `localhost` (so today's local-mode keeps working with no UX change) and a remote GCE VM (replacing the ephemeral-worker / warm-pool / GCS-push-pull architecture), eventually a Mac mini on the LAN. Treat "host" as a first-class abstraction in the TUI so swapping infra later is a config change, not a rewrite.

## Problem

CM started cloud-first with ephemeral GCP worker VMs claimed by a dispatch daemon, warm pools maintained per repo, and session state shuttled between worker and operator through GCS push/pull (`A-p` / `A-l`). In practice the local + worktrees flow turned out to be much smoother and is now the default, but the cloud path is still wanted for long-running or always-on jobs (periodic workflows, automation, work that should keep going while the laptop is closed). The current cloud architecture is heavy for that use case: every job pays VM cold-start, state has to round-trip GCS, and warm-pool sizing is an ongoing tuning knob. A single persistent host running everything would be cheaper, simpler, and would generalize cleanly to "the host is sometimes my laptop and sometimes a box on my desk" without two code paths.

The other half of the problem is structural. Today the TUI spawns PTYs in-process (`tui/src/session.rs:57`), the MCP socket lives on the same machine (`tui/src/control/server.rs:37`), workflow events are tailed off the local filesystem (`tui/src/workflow/events.rs:67`), and sessions are persisted to a local JSON manifest (`tui/src/app.rs:276`). All of those assume "the machine running the TUI is the machine running the work." To put work on another host you have to either ship the TUI there (loses the local UI) or rebuild a parallel cloud control plane (what we did, and what we're now backing away from).

## Goals

- One persistent host can run all cloud workloads — sessions, workflows, MCP — addressed by the local TUI as if it were local.
- Local mode is unchanged from the user's perspective: `A-n` still spawns a session, `A-a` still attaches, the same keybindings apply. The TUI just happens to be talking to a daemon process now.
- Adding a host (GCE VM today, Mac mini tomorrow, second laptop, whatever) is a config edit, not a code change.
- The MCP control plane (`~/.cm/tui.sock` + descendant-only `CM_TUI_SESSION_ID` scoping) keeps its current semantics. Agents calling MCP tools see no behavior change.
- Cloud-mode legacy (dispatch daemon, warm pool, GCS push/pull, worker startup scripts) is deprecated in a follow-up phase rather than cut in this work.

## Non-goals

- Multi-client attach to a single session. Daemon-side terminal grid + render-cell streaming is the right shape for that, but it's a much bigger refactor and not needed for the user's actual workflow (one operator, one TUI). For now: one attached client per session, daemon buffers a tail of PTY output for reconnect.
- Folding the FastAPI planning server into the daemon. They stay separate processes. The daemon owns session/PTY/workflow concerns; the API keeps owning the planning DB. They may end up on the same VM, but as two systemd units.
- Remote code editing inside the TUI. Worktrees live on the daemon's filesystem; editing them remotely is the user's choice of `ssh`, `sshfs`, VS Code Remote, etc. The doc flags this but doesn't solve it.
- Replacing the planning database with anything new. The planning view's interaction with `api/` is unchanged.

## Current state

PTYs are spawned in-process inside the TUI via alacritty's `Term` plus `portable-pty`, optionally wrapped in `systemd-run` for memory capping, with the master fd held directly in `Session::pty_writer` (`tui/src/session.rs:57-120`). Session metadata persists to `~/.cm/tui-sessions.json` as a `Manifest` of `ManifestEntry` records keyed by a stable per-session `uid` and including `transcript_id`, `generation`, `task_id`, optional `workflow_run_id` / `role`, and `managed_by_uid` for agent-spawned children (`tui/src/app.rs:276-390`, load/save at `tui/src/app.rs:447-480`). Worktrees are created locally via `git worktree add` under `~/.cm/worktrees/<slug>` (`tui/src/worktree.rs:5-151`).

The MCP control plane is already a Unix socket speaking length-prefixed JSON-RPC: server bound at `~/.cm/tui.sock` with an accept loop spawning a thread per connection (`tui/src/control/server.rs:37-100`), wire format defined in `tui/src/control/protocol.rs:1-95` (`Request{id, caller: {session_uid}, method, params}` → `Response{id, ok, result|error}`), and per-call authorization via `find_live_session` / `caller_ctx_or_tombstone` that validates the calling `CM_TUI_SESSION_ID` against the live session tree (`tui/src/control/methods.rs:43-144`). The MCP server (`mcp_server/server.py`) is a FastMCP instance that connects to that socket on behalf of each agent. Workflow runs persist to `~/.cm/workflow-runs/<run-id>/state.json` with the run's role bindings and history; the TUI watches `events.jsonl` in the same directory by byte offset (`tui/src/workflow/run.rs:1-120+`, `tui/src/workflow/events.rs:67-100`), and MCP tools `workflow_transition` / `workflow_done` append events to that file as an O_APPEND atomic write from the agent side.

The cloud path: `api/dispatch_daemon.py` runs a `dispatch_loop` (10s) that claims `backlog` tasks via `db.claim_next_task` and either reserves a warm VM or launches a new one through `dispatch.vm.launch_worker` (`api/dispatch_daemon.py:27-68`, `dispatch/vm.py:9-81`). A `warm_pool_loop` (30s) tops up per-repo VM pools (`api/dispatch_daemon.py:49-68`). Push/pull (`A-p` / `A-l`) is implemented in a backend thread that wraps gsutil and the API in `BackendCommand::Push` / `Pull` messages over an async channel (`tui/src/backend.rs:171-310`, bucket `gs://cm-sessions`). Workers run a Claude session inside tmux fronted by ttyd on port 8080; the operator-side TUI never attaches to those PTYs directly, it ships state through GCS.

## Proposed design

### Process model

```
┌──────────────────────┐                  ┌──────────────────────────────┐
│   cm TUI (laptop)    │                  │  cm-daemon (host: any)       │
│                      │                  │                              │
│  ┌────────────────┐  │   length-prefix  │  ┌────────────────────────┐  │
│  │ thin client    │◄─┤   JSON-RPC, plus │  │ session manifest       │  │
│  │ (RPC + render) │  │   per-session    │  │ (~/.cm/tui-sessions)   │  │
│  └────────────────┘  │   PTY byte       │  └────────────────────────┘  │
│  ┌────────────────┐  │   stream         │  ┌────────────────────────┐  │
│  │ alacritty Term │◄─┼──────────────────┤  │ PTY processes          │  │
│  │ (client-side)  │  │                  │  │ + memory cap wrap      │  │
│  └────────────────┘  │                  │  └────────────────────────┘  │
│                      │                  │  ┌────────────────────────┐  │
│  ┌────────────────┐  │                  │  │ worktrees              │  │
│  │ planning view  │──┼─► api (FastAPI)  │  │ (~/.cm/worktrees)      │  │
│  └────────────────┘  │                  │  └────────────────────────┘  │
└──────────────────────┘                  │  ┌────────────────────────┐  │
                                          │  │ workflow runs +        │  │
                                          │  │ events.jsonl tail      │  │
                                          │  └────────────────────────┘  │
                                          │  ┌────────────────────────┐  │
                                          │  │ MCP socket             │  │
                                          │  │ (~/.cm/daemon.sock)    │──┼──◄ agent PTYs in daemon
                                          │  └────────────────────────┘  │
                                          └──────────────────────────────┘
```

The daemon is a Rust binary that pulls out the "session + workflow + MCP-socket" side of today's TUI. Most modules (`tui/src/worktree.rs`, `tui/src/workflow/`, `tui/src/control/`, `tui/src/mcp_config.rs`) relocate wholesale. `tui/src/session.rs` is the one exception — it does *not* move wholesale because today's `Session` struct (`session.rs:31-48`) tightly couples three things that need to live on different sides of the wire: alacritty's `Term`, alacritty's `EventLoop`, and the OS PTY fd (`pty_writer: File`). See "Session struct split" below. By the end of Phase 2, the TUI has lost all direct filesystem access to `~/.cm/`; Phase 1 is the bulk of that move and Phase 2 wraps up the last leftover (workflow files).

### Host abstraction

A new `~/.cm/hosts.toml` enumerates daemons the TUI can talk to:

```toml
[[host]]
name = "local"
transport = "unix"
socket   = "~/.cm/daemon.sock"
default  = true

[[host]]
name = "manager"
transport = "tcp"
addr     = "34.11.80.141:8443"
tls      = "system"
auth     = { kind = "token", env = "CM_DAEMON_TOKEN" }
```

`HostId` becomes a first-class field on every session in the TUI's in-memory state and on every operation. The TUI keeps a small connection pool keyed by `HostId`. A new keybind (`A-H`) cycles the "active host" in the Sessions view for new-session creation (`A-n` / `A-s` / `A-f`); existing sessions stay pinned to whichever host they were created on, so a list view across all hosts is the natural default and individual sessions just route their RPCs to their owning host.

The planning view stays host-independent — it speaks to the existing FastAPI planning server through whatever URL/token the TUI already uses. Tasks aren't tied to hosts; only sessions are.

### Wire protocol

Reuse and extend the existing length-prefixed JSON-RPC frame from `tui/src/control/protocol.rs:1-95`. Today every connection carries a flat `caller: {session_uid: "..."}` field that authenticates the *agent* on the other end (`Caller` is a struct at `protocol.rs:28-31`; the Python client sends exactly that shape — `mcp_server/control_client.py:72`). Migrate `Caller` to a `serde(untagged)` enum over two newtype-wrapped structs, each with `deny_unknown_fields`. The wrapping is needed because `deny_unknown_fields` on inline struct variants of an `untagged` enum is silently ignored by serde — only struct-level `deny_unknown_fields` is honored, and untagged enum dispatch uses each variant's own `Deserialize`. The wrapper structs give us that struct-level annotation:

```rust
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallerSession { pub session_uid: String }

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallerOperator { pub token_id: String }

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum Caller {
    Session(CallerSession),
    Operator(CallerOperator),
}
```

Verified wire-compat (and the cases the doc has to be honest about):
- Existing payload `{"session_uid":"uid-x"}` deserializes as `Caller::Session(CallerSession { session_uid: "uid-x" })`. `CallerOperator` fails because it requires `token_id`.
- New operator payload `{"token_id":"tk-y"}` deserializes as `Caller::Operator(CallerOperator { token_id: "tk-y" })`.
- A *mixed* payload `{"session_uid":"...", "token_id":"..."}` fails both variants thanks to per-variant `deny_unknown_fields`, and `untagged` propagates a deserialization error — which is the behavior we want (the caller has to be one or the other, never both).
- A payload with no recognized fields (e.g. `{}`) also errors. The existing flat `Caller` had no such protection — this is a strict tightening rather than a regression; the previous deserializer would have accepted `{}` and produced an empty `session_uid: String::new()`. The migration PR adds a regression test for `{}` to confirm rejection.
- Either variant serializes back to the same field-only shape it parses — no extra `kind` tag is emitted (untagged + newtype + struct-level fields all collapse to a flat object), so `mcp_server/control_client.py` keeps sending the exact bytes it sends today.

The constraint this places on future variants is "no field-name collision with existing variants" — acceptable given the small caller taxonomy. Enforced by a round-trip test that includes a payload-with-both-fields case asserting an error. `Operator` callers can list sessions across the host, spawn / attach / kill, drive workflows, edit the manifest. `Session` callers stay descendant-scoped exactly as today. The existing methods in `tui/src/control/methods.rs` keep their semantics for `Session` callers; new operator-only methods land alongside.

For the TUI ⇄ daemon channel specifically, we need three new method families:

| Method | Connection | Direction | Shape |
|---|---|---|---|
| `session.attach(uid)` | control | unary request → unary response | Allocates an `AttachTicket` for `uid` and returns `{ attach_ticket, attach_addr }`. The TUI uses these to bootstrap a dedicated connection — see "Connection model" below. Stays on the control connection; does not itself stream. |
| `attach.open(ticket)` | dedicated | request → stream | Sent as the first JSON-RPC frame on a *freshly dialed* connection. Consumes the ticket, binds the connection to the matching `DaemonSession`'s fanout, and transitions the connection into a bidirectional byte stream: server frames are PTY output, client frames are keystrokes. Daemon-side ring buffer (configurable, default 1 MiB) replays on reconnect. |
| `events.subscribe(filter)` | control | request → stream | Daemon pushes workflow + session-lifecycle events (`session_started`, `session_exited`, `workflow_transition`, `workflow_done`, manifest changes). Replaces today's `events.jsonl` tail. Phase 2. |
| `manifest.watch()` | control | request → stream | Daemon pushes manifest diffs (session added/updated/exited/tombstoned, including the new `last_exit` field). Replaces today's read of `~/.cm/tui-sessions.json` on disk. |

Streams cannot extend the existing `Response` envelope: it carries `#[serde(deny_unknown_fields)]` (`protocol.rs:36`), so a `stream_id` field would break strict clients. Streams travel on a separate frame `StreamFrame { id, kind: data|end|error, payload }` distinguished from `Response` by the presence of `kind` (which `Response` doesn't have and won't accept). Framing is the same 4-byte length prefix as today.

Connection model:

- **Control connection (per operator).** One per TUI process, multiplexing every non-`session.attach` RPC and stream — `manifest.watch`, `events.subscribe`, `workflow.get_state`, the spawn/kill/send-input calls, etc. Stream `id` disambiguates concurrent streams.
- **One dedicated connection per `session.attach`.** Each attach opens a fresh connection (Unix or TCP) used exclusively for that session's PTY byte stream. This is required because the TUI-side `EventedPty` shim registers the connection's fd with alacritty's `Poller` (see "PTY streaming"); if two sessions multiplexed on one fd, alacritty would race them and consume each other's frames.

Bootstrap of a dedicated attach connection (so the daemon knows which session this new connection belongs to — the listening address is shared across all attaches and the control connection, so the connection has to identify itself):

1. **Control connection** sends `session.attach { uid: "ts-..." }`. The daemon validates `uid` against the caller's scope, allocates a single-use `AttachTicket` (UUID, 30 s TTL, bound to the session uid and the caller's identity), and returns it:
   ```json
   { "ok": true, "result": { "attach_ticket": "at-...", "attach_addr": "<same socket path / TCP host:port>" } }
   ```
   The address is intentionally not unique per attach — it's just whichever listener the daemon is already serving. Uniqueness lives in the ticket, not the address.

2. **TUI dials a fresh connection** to `attach_addr`. On a TCP host, it completes TLS and sends `auth.hello` as today (operator token). On a Unix host, this step is skipped (filesystem permissions are the trust boundary).

3. **First post-auth JSON-RPC frame on the new connection is `attach.open`:**
   ```json
   { "id": "<uuid>",
     "caller": { "token_id": "..." },
     "method": "attach.open",
     "params": { "ticket": "at-..." } }
   ```
   The daemon looks up the ticket, verifies it's unconsumed and unexpired and that the connection's authenticated identity matches the identity it was issued to, then binds this connection to that `DaemonSession`'s fanout. Response is an empty `ok`. From this point the connection carries `StreamFrame`s only — no further `Request`/`Response` traffic. Any frame other than a valid `attach.open` as the first non-auth message on a fresh connection (including a stale ticket, wrong caller, or a different method) earns `Unauthorized` and a close.

The ticket is single-use and short-TTL specifically so a stale `attach_ticket` value cannot be replayed by a different process, and a ticket issued for session A cannot redirect traffic into session B's fanout. Tickets that expire without being consumed (TUI crash between steps 1 and 3) are reclaimed by the daemon's allocator; they don't accumulate.

Cost: each `session.attach` adds two RTTs over a fresh connection — one for the control RPC, one for `attach.open`. Bounded by how many sessions an operator looks at concurrently (typically 1–5), not how many exist on the host. Control-plane traffic stays on the multiplexed connection.

### PTY streaming

The daemon owns the OS PTY (the concrete type returned by `alacritty_terminal::tty::new`, called at `tui/src/session.rs:106`). The TUI keeps its alacritty `Term` instance per session, attached to its own `EventLoop` whose underlying "PTY" is a network-backed shim. Keystrokes flow the other direction. This preserves today's behavior (alacritty handles ANSI, scrollback, resize) almost unchanged on the TUI side.

The shim exists because alacritty's `EventLoop<T: tty::EventedPty, U: EventListener>` (`alacritty_terminal-0.25.1/src/event_loop.rs:46`) is generic over its PTY type but the on-host PTY produced by `tty::new` can't itself be split across the network. Each attached session gets its own dedicated transport connection (see "Connection model" above) so the shim has an exclusive fd to register with the `Poller`. The shim is a small struct on the TUI side that:

- Implements `EventedReadWrite` (`alacritty_terminal-0.25.1/src/tty/mod.rs:64-77`) with `Reader = StreamReader` and `Writer = StreamWriter`. The `Poller` registration uses the dedicated connection's fd. Because no other session shares this fd, there's no risk of one shim consuming another session's bytes.
- The dedicated connection carries one stream of length-prefixed `StreamFrame`s in each direction; the shim's `StreamReader` peels the framing and yields raw PTY bytes to alacritty, and `StreamWriter` re-frames keystrokes before they hit the wire. Framing happens inside the shim, not at the alacritty layer.
- Implements `EventedPty` (`tty/mod.rs:91-96`) so `next_child_event()` returns `Some(ChildEvent::Exited(code))` when the daemon sends a `kind = "exited"` frame on the stream. The frame additionally carries `memory_cap_kill: bool` for cap-triggered exits so the TUI can surface them in Phase 1 without depending on `events.subscribe` (see "Memory-cap kill notification in Phase 1" below). Daemon-side child-exit detection is unchanged (alacritty's PTY child reaper on the daemon); the event is forwarded over the wire.
- Implements `event::OnResize` by sending a `winsize` frame back to the daemon, which calls `set_winsize` on the real PTY.

The shim replaces `tty::new` only in the TUI's `Session` constructor; everything downstream of it (the `Term`, `EventLoop`, EventProxy) is unchanged. The daemon's `Session` constructor is essentially today's code with the consumer of PTY bytes swapped from "feed the local Term" to "broadcast to attached client streams + an internal write-through to its own Term if we keep a server-side grid for future multi-client (we don't, yet)."

Reconnect: the daemon retains a ring buffer of the last N bytes per session (default 1 MiB, configurable per session type). On reattach, it replays that buffer, then resumes live. This is enough for the user-visible "I closed the TUI, reopened it, my session is still going" case. Scrollback that exceeds the ring is lost on reconnect — same as today after a TUI crash, so no regression.

Resize: handled by the shim's `OnResize` impl above. No new RPC method needed.

### Session struct split

Today's `Session` in `tui/src/session.rs:31-48` couples `Arc<FairMutex<Term<EventProxy>>>`, the `EventLoopSender`, the `pty_writer: File`, the EventProxy channel, exit state, wakeup-burst tracking, the `MemoryCap`, and the cgroup path in one struct. That single coupling needs to become two structs in two processes:

```text
// Daemon-side: no Term, no EventLoop. Owns the OS-level PTY child
// and a fan-out broadcaster to attached clients.
struct DaemonSession {
    pty: alacritty_terminal::tty::Pty,    // returned by tty::new(); owns master fd + child
    reader_handle: thread::JoinHandle<()>, // pulls bytes off master, pushes to fan-out
    fanout: PtyByteFanout,                 // ring buffer + 0..N attached client streams
    memory_cap: Option<MemoryCap>,         // unchanged
    cgroup_path: Option<PathBuf>,          // unchanged
    exited: AtomicBool,                    // set by the reader thread on EOF
    wakeup_times: Mutex<Vec<Instant>>,     // moved here; burst detection is a server-side signal
    title: String,
    uid: String,                           // identity for manifest + auth scoping
}

// Client-side: Term + EventLoop, fed by the network shim — no PTY,
// no child process, no cgroup. EventProxy is unchanged.
struct ClientSession {
    term: Arc<FairMutex<Term<EventProxy>>>,
    sender: EventLoopSender,
    event_rx: mpsc::Receiver<TermEvent>,
    shim_writer: StreamWriter,             // keystrokes → daemon (dedicated attach connection)
    title: String,
    uid: String,
}
```

The shim (`StreamReader` / `StreamWriter` — the same types referenced in "PTY streaming" above) bridges the two: client-side `EventLoop` reads from `StreamReader` (which peels framing off the dedicated attach connection — the one transitioned into streaming by `attach.open` — and yields PTY bytes), and the client writes keystrokes back through `StreamWriter` (which re-frames keystrokes; the daemon writes them to the real PTY master). The mpsc EventProxy channel still exists in the client; it carries terminal-level events (title changes, bell, etc.) from the EventLoop to the TUI's main loop just like today.

The memory-cap mechanism stays daemon-side because it wraps the *child process*. Today's `wrap_with_systemd_run` (`session.rs:87`) runs in the same code path that calls `tty::new` — both move to `DaemonSession::new`. Memory-kill records continue to land at `~/.cm/memory_kills/<uid>.jsonl` on the daemon's host.

#### Memory-cap kill notification in Phase 1

`events.subscribe` is a Phase 2 deliverable, so Phase 1 can't depend on it for memory-kill UX. Two Phase-1 signals cover it:

1. **Attach-stream exit frame** (described above) carries `memory_cap_kill: bool`. If the TUI is attached to the session when the kill happens, it sees it inline with the exit event on the dedicated stream (the one bootstrapped by `attach.open`) — same UX as today's `signal 9` toast, just sourced from a stream frame instead of a process exit code on the local PTY.
2. **`manifest.watch` exit metadata.** When the daemon's reaper detects a memory-cap kill, it updates the session's `ManifestEntry` to mark it exited with `last_exit: { code: ..., memory_cap_kill: true, kills_file_offset: <bytes> }`. The TUI subscribes to manifest diffs in Phase 1 anyway (it's how the Sessions view stays current), so detached sessions surface the kill the moment the manifest diff arrives. The TUI continues to read `~/.cm/memory_kills/<uid>.jsonl` for the detailed record body, exactly as today — Phase 1 is single-machine so direct file read is allowed.

Phase 2 doesn't change the producer side; it adds `events.subscribe` as a third subscriber on the same daemon-side broadcast, useful for cross-host scenarios in Phase 3 where the TUI can no longer read the kills file directly.

### MCP socket placement

The MCP socket moves with the daemon. New path: `~/.cm/daemon.sock` on the daemon's host. Agents spawned by the daemon get `CM_TUI_SOCKET=~/.cm/daemon.sock` in their environment, exactly as `tui/src/mcp_config.rs:45-92` injects it today; `mcp_server/control_client.py:45-51` already resolves the path via that env var with a fallback to `~/.cm/tui.sock`. No symlink is needed for in-tree usage — agents inherit the right `CM_TUI_SOCKET` value from the daemon's spawn env. The fallback default in `control_client.py` is updated to `~/.cm/daemon.sock` in the same change; the `tui.sock` literal is retained as a secondary fallback for one release for any external scripts that bypass the env.

`CM_TUI_SESSION_ID` is the env name today for descendant-scope auth. It stays named that way through Phase 3 to avoid wire churn, and is renamed to `CM_SESSION_ID` (with `CM_TUI_SESSION_ID` accepted as a deprecated alias) in Phase 4. Both `mcp_server/server.py` and `predictionTrading/scripts/mcp/claude_manager_server.py` need the alias accepted; the second copy is easy to forget — track it in the Phase 1 checklist.

Crucially, agents do NOT reach back to the operator's laptop. Their MCP socket is local to whichever daemon they live in. This sidesteps the entire "how does a cloud agent call the TUI's MCP" problem that the current ephemeral-worker design has.

### Workflow event flow

Today the TUI tails `~/.cm/workflow-runs/<id>/events.jsonl` by byte offset (`tui/src/workflow/events.rs:67-100`). MCP-side writers (`mcp_server/server.py:39-56`, `_append_event`) write to the same file directly via O_APPEND — that's the existing producer path and stays unchanged. In the new model, the *daemon* runs the file-tail loop (moved out of the TUI) and broadcasts each appended event to `events.subscribe` subscribers; the TUI subscribes and stops touching the filesystem. The event file still exists for durability and post-mortem.

Concretely: agent MCP calls land in the file → daemon's internal tail loop reads new bytes → daemon dispatches to its workflow controller and to TUI-side subscribers in one pass. No new daemon RPC is added for MCP writers; the file remains the producer/consumer rendezvous, just with the consumer relocated.

The workflow controller (`tui/src/workflow/controller.rs` and friends) moves into the daemon wholesale. Static `on_idle` transitions and dynamic `workflow_transition` / `workflow_done` MCP calls all resolve daemon-side. The TUI observes the resulting state via `events.subscribe` plus a `workflow.get_state` RPC for cold reads on attach.

### Auth and transport

Local Unix socket: filesystem permissions are the trust boundary, same as today. No token required.

Remote TCP: TLS terminates at the daemon. This is *not* HTTP, so `Authorization: Bearer` semantics don't apply. Instead, the first JSON-RPC frame the client sends after the TLS handshake completes is a literal `auth.hello`:

```json
{ "id": "<uuid>",
  "caller": { "token_id": "manager-default" },
  "method": "auth.hello",
  "params": { "token": "<value from CM_DAEMON_TOKEN>" } }
```

The daemon compares the token against its configured value in constant time. On success it replies with an ok `Response` and the connection is "authenticated" — subsequent frames are processed normally with `Caller::Operator` identity. On failure it sends an `Unauthorized` error and closes the connection. Any non-`auth.hello` frame sent before this handshake also closes the connection. The token comes from the env var named in `hosts.toml`, matching the existing `CM_API_TOKEN` pattern in `api/auth.py` so users have one mental model — just delivered as an in-protocol frame rather than an HTTP header.

For users who don't want to manage TLS certs on a single GCE VM, the daemon can also listen on a Unix socket only and the TUI reaches it via `ssh -L /local/path:/remote/path`. This is an explicit option in `hosts.toml`: `transport = "ssh-unix"`. It's lower-friction for the first cm-manager rollout; native TCP+TLS can land in the same phase or follow shortly. The SSH transport doesn't need `auth.hello` because the SSH session itself is the trust boundary.

### Daemon supervision

- On the GCE VM: systemd unit, `Restart=always`, mirrors the existing `claude-manager.service` pattern. Logs to `/var/log/cm-daemon.log`.
- On the local laptop: TUI auto-launches the local daemon if it isn't already running (checks the Unix socket; if absent, forks `cm-daemon --user` and waits up to 2 seconds for the socket to appear). Same pattern as `tmux`.
- On the Mac mini: launchd plist with the equivalent shape.

### Configuration and on-disk layout

Daemon-owned (on the daemon's host):

```
~/.cm/daemon.sock              # MCP + TUI client socket
~/.cm/daemon.toml              # daemon config (port, TLS paths, buffer sizes)
~/.cm/tui-sessions.json        # session manifest (unchanged schema)
~/.cm/worktrees/...            # worktrees (unchanged)
~/.cm/workflow-runs/.../...    # workflow state + events.jsonl (unchanged)
~/.cm/memory_kills/...         # SIGKILL records (unchanged)
~/.claude/projects/...         # claude transcripts (unchanged; daemon spawns claude)
~/.codex/sessions/...          # codex transcripts (unchanged; daemon spawns codex)
```

TUI-owned (on the operator's laptop):

```
~/.cm/hosts.toml               # daemon endpoints
~/.cm/tui-state.json           # UI-only state (focus, active host, etc.)
```

Existing data on disk on someone's laptop is read by the local daemon at first launch — no migration step beyond the additive `#[serde(default)]` fields described in Rollout / migration.

### Alternatives considered

| Alternative | Why rejected |
|---|---|
| Keep ephemeral workers, just make them longer-lived (idle for hours) | Doesn't address the warm-pool / GCS-push-pull complexity, and "one host I keep my work on" is the actual mental model the user wants. |
| TUI drives remote PTYs over `ssh host claude ...` directly | Trivial for spawning but the MCP socket has nowhere coherent to live — either on the laptop (agents can't reach it without reverse-tunnels) or on the host (then half the existing design lives in two places). |
| Server-side terminal grid + render-cell stream (like mosh) | Right shape for multi-client attach and proper scrollback persistence, but a substantial render-pipeline refactor — the chosen byte-streaming path needs only an `EventedPty` shim on the TUI side because alacritty's `EventLoop` is already generic over its PTY type. Revisit when multi-client matters. |
| Fold the FastAPI planning server into the daemon | Mixes two concerns (planning DB vs session control) and forces a planning DB to follow you to a Mac mini, which we don't want. |
| gRPC instead of extending JSON-RPC | The existing protocol already works, agents speak it through `mcp_server/server.py`, and adding bidirectional streams to length-prefixed JSON is straightforward. gRPC pulls in tonic + protobuf machinery for negligible gain. |

## Risks and open questions

- **PTY ring buffer sizing.** 1 MiB is a guess. Heavily-output sessions (long compiles, fuzz runs) blow past that quickly. Should we make it per-session-type, or expose it as a `daemon.toml` knob with a sane default? Probably both, defaults wired in Phase 1, override-per-session deferred.
- **`EventedPty` shim correctness.** The shim's child-exit semantics need to match what alacritty expects (one `Some(ChildEvent::Exited(_))` then steady `None`). Daemon-side wiring is straightforward; the open question is what to surface when the network stream itself dies before the daemon reports child exit — probably a synthesized `ChildEvent::Exited(None)` plus a TUI-side toast. Decide and write the test during Phase 1.
- **Operator auth on the local Unix socket.** Filesystem permissions today are the trust boundary for the MCP socket. Adding an `Operator` caller kind that bypasses descendant-scoping changes the threat model slightly (any process under the user's UID can drive the daemon). Acceptable for the user's setup but worth documenting in `mcp_server/server.py`.
- **Codex transcript paths.** `~/.codex/sessions/` is daemon-local. If the user wants to inspect transcripts from the laptop, they need SSH access (or a future `transcript.read` RPC). Not blocking; flag in Phase 4 retrospective.
- **Network partitions.** TUI loses its connection mid-session: the daemon keeps the PTY alive, the buffer fills past its cap, on reconnect the user sees a "you missed N KiB of output" notice and the buffer tail. Spec the behavior; don't try to be clever.
- **GCS push/pull during transition.** While Phase 4 hasn't shipped, the `A-p` / `A-l` keybindings still work against the old API. Either leave them wired (lowest risk) or stub them with a "deprecated, use host=manager" hint. Decide in Phase 4 itself.
- **Mac mini transition.** The doc claims this is "just a config change." That's true for `hosts.toml`, but in practice it also requires the user to install `cm-daemon` on the Mac, set up launchd, copy any data they want to preserve, and update the planning API endpoint if they're moving it too. Worth a short follow-up doc when the time comes.

## Implementation plan

### Phase 1: Extract `cm-daemon`, local-only

- **Goal:** A single new `cm-daemon` binary owns sessions, worktrees, MCP socket, and workflow state. The TUI becomes a thin client over a Unix socket; everything works the same as today on a single local machine. No multi-host yet.
- **Scope:**
  - New crate `daemon/` (or `tui/cm-daemon` sub-binary — sort during scaffolding) containing what is today in `tui/src/worktree.rs`, `tui/src/control/`, `tui/src/mcp_config.rs`, plus the daemon half of the split `tui/src/session.rs` (see "Session struct split" above). The TUI keeps the client half.
  - `tui/src/workflow/` *also* moves to the daemon as part of this phase (so the workflow controller runs daemon-side), but the *TUI-side workflow event consumer* still file-tails `~/.cm/workflow-runs/<id>/events.jsonl` from the same machine. This is intentional staging: Phase 1 is single-machine so cross-process file-tail still works; Phase 2 cuts the file dependency.
  - Extend `tui/src/control/protocol.rs`: migrate `Caller` to a `serde(untagged)` enum with `Session`/`Operator` variants (the existing flat shape parses unchanged, see Wire protocol above), add `StreamFrame`, add `session.attach` (control-side, issues `AttachTicket`), `attach.open` (dedicated-connection bootstrap, consumes ticket), and `manifest.watch`. Daemon implements an `AttachTicket` allocator with single-use semantics and 30s TTL.
  - Implement the network-backed `EventedPty` shim on the TUI side and the daemon-side PTY-byte stream owner. Configurable per-session ring buffer; default 1 MiB.
  - TUI: replace direct PTY spawning, manifest file I/O, and worktree calls with RPC calls against `~/.cm/daemon.sock`. Workflow file-tail stays in the TUI until Phase 2.
  - TUI auto-launches the daemon on startup if the socket is absent.
  - Default socket path: `~/.cm/daemon.sock`. Update `mcp_server/control_client.py:45-51` to default there (keep `~/.cm/tui.sock` as a secondary fallback for one release). No symlink needed — agents inherit `CM_TUI_SOCKET` from the daemon's spawn env.
  - Mirror any MCP-side changes into `predictionTrading/scripts/mcp/claude_manager_server.py` (per [[project_mcp_two_servers]]).
- **Out of scope for this phase:** multi-host config, remote transport, `events.subscribe` / `workflow.get_state` RPC (Phase 2), removal of TUI-side workflow file-tail (Phase 2).
- **Acceptance criteria:**
  - `cm-daemon` builds and starts; binds `~/.cm/daemon.sock`; running it twice fails fast on the stale-socket probe.
  - TUI launched against a clean home spawns the daemon, creates a session via `A-n`, attaches via `A-a`, types into it, kills it via `A-w`, all with no behavior change visible to the operator.
  - Workflow lifecycle (`A-f` to launch feedback mode, transitions to reviewer, manager calls `workflow_done`) completes end-to-end with the daemon owning the workflow controller.
  - MCP agent inside a daemon-spawned session can call `propose_task`, `list_sessions`, `start_session`, `send_input`, `read_session_output`, `kill_session`, `workflow_transition`, `workflow_done`. `CM_TUI_SESSION_ID` scoping behaves identically to today (descendant-only).
  - Memory cap kills still write to `~/.cm/memory_kills/<uid>.jsonl`. The TUI surfaces them via both Phase 1 paths: attached sessions get the `memory_cap_kill: true` flag on the attach-stream exit frame, and detached sessions get the same flag on their `manifest.watch` exit diff. Behavior is indistinguishable from today's `signal 9` toast.
  - All existing TUI integration tests pass; new test covers daemon reconnect (kill TUI mid-session, restart, observe the ring-buffer replay).
- **Dependencies:** none.

### Phase 2: Workflow events over RPC

- **Goal:** Drop the file-tail of `events.jsonl` in the TUI. Workflow state and events flow exclusively through `events.subscribe` and a `workflow.get_state` RPC. Once this lands, the TUI no longer touches the daemon's filesystem for any runtime data, which is the precondition for putting the daemon on another host.
- **Scope:**
  - Add the `events.subscribe` streaming RPC: on subscribe, the daemon's existing `events::read_new` loop (already daemon-side after Phase 1) fans out each `Event` to all current subscribers in addition to its existing in-daemon dispatch.
  - Add `workflow.get_state(run_id)` returning the daemon's current `WorkflowRun` snapshot for cold reads (TUI attach / reconnect).
  - TUI's workflow view consumes the subscribe stream + snapshot RPC instead of reading files; delete the TUI-side file-tail code path.
  - The MCP `workflow_transition` / `workflow_done` tools keep writing to `events.jsonl` on the daemon's filesystem (durability), which is still the producer; the daemon's tail loop is the broadcast point — single write, two consumers (file durability + RPC broadcast).
- **Out of scope for this phase:** remote / multi-host. Still single Unix socket.
- **Acceptance criteria:**
  - The TUI no longer references `tui/src/workflow/events.rs::read_new` or any path under `~/.cm/workflow-runs/` for reads. (Grep proves it.)
  - Existing feedback-mode workflow runs work identically — manual smoke: worker → reviewer → manager → done, with `A-y` history showing every transition.
  - Killing the TUI mid-workflow and reattaching shows the current active role and recent transitions (via `workflow.get_state` + last N events from the daemon's broadcast buffer).
  - `~/.cm/workflow-runs/<id>/events.jsonl` still contains the same records it does today (durability unchanged).
- **Dependencies:** Phase 1.

### Phase 3: Multi-host + remote transport

- **Goal:** TUI can attach to a remote daemon. The cm-manager VM runs `cm-daemon` and the operator drives sessions on it from their laptop. End of phase, the user has a working "persistent host" exactly as the doc proposes, with everything but cloud-mode cleanup in place.
- **Scope:**
  - `~/.cm/hosts.toml` schema + loader. `HostId` plumbed through every session-bearing piece of TUI state. Session manifest gains an optional `host_id` field on each entry (see Rollout / migration for the backfill rule).
  - Per-host RPC connection pool in the TUI.
  - Daemon TCP listener with TLS (rustls). `daemon.toml` declares cert paths and listen address. Operator auth via the `auth.hello` JSON-RPC frame (see "Auth and transport" above); the token comes from the env var named in `hosts.toml`.
  - `transport = "ssh-unix"` option in `hosts.toml` for users who prefer SSH-tunneled Unix sockets to managing TLS certs. The TUI invokes `ssh -L` itself and binds the local end.
  - `A-H` keybind cycles active host in Sessions view. Sidebar groups sessions by host when more than one is configured.
  - **Remote-host packaging.** The cm-manager VM currently has no `mcp_server/` directory (per `CLAUDE.md` line 127). For agents spawned by a remote daemon to use MCP tools and workflows, the deploy must ship:
    - The `cm-daemon` binary (Linux build for cm-manager).
    - The `mcp_server/` Python package, alongside a Python interpreter that satisfies its `pyproject.toml` deps. Install to `/opt/cm-daemon/mcp_server/` and set `CM_MCP_SERVER=/opt/cm-daemon/mcp_server/server.py` (the resolver in `tui/src/workflow/spawn.rs:18` checks `CM_MCP_SERVER` first before falling back to workflows-dir-relative paths).
    - The `workflows/` directory at a known path; daemon config records the location so `crate::workflow::toml_schema::workflows_dir()` resolves correctly daemon-side.
  - **Daemon-side env injection.** The daemon's `spawn` path injects the following into every agent process env: `CM_TUI_SOCKET` (daemon socket), `CM_TUI_SESSION_ID` (session uid), `CM_MCP_SERVER` (server.py path), `CM_API_URL` and `CM_API_TOKEN` (so `propose_task` etc. can reach the planning API — these come from `daemon.toml` on the daemon's host, NOT from the TUI), `CM_WORKFLOW_RUN_ID` and `CM_ROLE` for workflow participants.
  - **Systemd unit.** `/etc/systemd/system/cm-daemon.service`, `Restart=always`, `Environment=` block carrying `CM_API_URL` / `CM_API_TOKEN` / `CM_MCP_SERVER`. `EnvironmentFile=/etc/cm-daemon/token` for the `CM_DAEMON_TOKEN` value. Mirrors the existing `claude-manager.service` recipe (`CLAUDE.md` "Deploying API changes").
  - **VM preparation on cm-manager.** Today the VM is sized to run only the FastAPI; it needs real prep before it can carry interactive workloads:
    - **Instance sizing.** Today `cm-manager` is a small instance running uvicorn. Bump to at least `e2-standard-4` (4 vCPU, 16 GB) before Phase 3 lands, with headroom to grow. Sizing is workload-driven; the doc doesn't fix a target — `gcloud compute instances set-machine-type cm-manager` is reversible.
    - **Disk sizing.** Today's boot disk is ~30 GB. Worktrees, transcripts (`~/.claude/projects/`, `~/.codex/sessions/`), and `~/.cm/` state will accumulate. Resize the boot disk to ~200 GB (`gcloud compute disks resize`), or mount a separate persistent disk at `~/.cm/` so an instance recreate doesn't lose state. Disk choice is a one-line decision in Phase 3 itself; default to bumping the boot disk for simplicity.
    - **Daemon user account.** Today the API runs under whatever uid was set when `claude-manager.service` was installed. The daemon needs a real user account with a home directory that owns `~/.cm/`, `~/.claude/`, `~/.codex/`, and any worktree paths — sessions write into all of those. Either reuse the operator's account (`lucas`) on the VM, or provision a dedicated `cm` user. Document which.
    - **Firewall.** Today only `tcp:8000` (FastAPI) is open. Open `tcp:8443` (daemon TLS) to the operator's IP range, *not* `0.0.0.0/0`. `gcloud compute firewall-rules create cm-daemon-tls --allow=tcp:8443 --source-ranges=<operator-ip>/32 --target-tags=cm-manager`. The operator-IP-pinning is the second line of defense behind `auth.hello`; if your IP changes (laptop on a different network), update the firewall rule. SSH-unix transport doesn't need this rule.
    - **TLS cert.** Use a self-signed cert generated on the VM (`openssl req -x509 -newkey ed25519 -days 3650 ...`), stored at `/etc/cm-daemon/tls.{crt,key}`, mode 0600 for the key, owned by the daemon user. The TUI side pins the cert's SHA-256 fingerprint in `hosts.toml` (`tls_fingerprint = "sha256:..."`). This avoids the DNS + Let's Encrypt machinery for a single-operator deployment; the tradeoff is rotation is manual (regenerate + update fingerprint, ~5 min). If we ever want a real CA path later, swap `tls_fingerprint` for `tls_ca_cert` or `tls = "system"` in `hosts.toml` — the schema accommodates both.
    - **`CM_DAEMON_TOKEN` storage.** Generate once during install (`openssl rand -hex 32 > /etc/cm-daemon/token`, mode 0600, owned by daemon user). The systemd unit reads it via `EnvironmentFile=`. The operator's laptop sources the same value from a local secret store (1Password CLI, `pass`, `gcloud secrets versions access`, etc.) into the env var named in `hosts.toml`. Rotation: regenerate on the VM, push the new value to the operator's secret store, restart `cm-daemon`.
    - **Operator SSH access.** Confirm the operator's SSH key is in `~/.ssh/authorized_keys` for the daemon user. Needed both for the SSH-unix transport fallback and for in-place code editing on the host (VS Code Remote, etc.).
  - Document the operator-side flow in `CLAUDE.md`: update the "Cloud mode" section to describe the host-daemon model, and update the "the MCP server runs locally on user machines (cm-manager has no `mcp_server/` directory)" claim to reflect the new deployment.
- **Out of scope for this phase:** removing the dispatch daemon, warm pool, or GCS push/pull code. They keep running on cm-manager alongside the new daemon during Phase 3.
- **Acceptance criteria:**
  - Operator with `[[host]] name = "manager"` in `~/.cm/hosts.toml` can run the TUI on their laptop, switch to that host via `A-H`, create a session with `A-n`, attach with `A-a`, drive a feedback-mode workflow end-to-end, and kill the session.
  - Closing the TUI and reopening it shows the remote session still running and reattaches without state loss (beyond ring-buffer overflow).
  - An agent inside a remote-host session can call `propose_task` and the task appears in the planning view, proving MCP-on-daemon works and proving planning is host-independent.
  - TLS handshake failure (bad cert, bad token) is surfaced to the operator as a clear error, not a hang.
  - SSH-unix transport works as a fallback against the same daemon (with TCP listener off).
  - VM prep complete: instance type and disk size right-sized, firewall rule for the daemon port scoped to the operator IP (not `0.0.0.0/0`), TLS cert + fingerprint and `CM_DAEMON_TOKEN` generated and stored under `/etc/cm-daemon/` with 0600 perms. Phase 3 PR description includes the exact `gcloud` commands used so the recipe is reproducible.
- **Dependencies:** Phase 2.

### Phase 4: Cloud-mode deprecation

- **Goal:** Remove the dispatch daemon, warm-pool maintenance, GCS push/pull, and ephemeral worker startup. The persistent host is the only cloud path.
- **Scope:**
  - Delete `api/dispatch_daemon.py`, `dispatch/vm.py`, `dispatch/db.py`'s warm-pool methods, and the lifespan hooks in `api/main.py` that start them.
  - Delete `worker/startup.sh` and any worker base-image rebuild scripts.
  - Delete the `BackendCommand::Push` / `BackendCommand::Pull` arms in `tui/src/backend.rs` and the `A-p` / `A-l` keybinds. (`A-p` and `A-l` become free for future use or stay unbound — pick during the phase.)
  - Remove the `is_cloud` task field plumbing if it has no other consumers post-removal.
  - Remove the `cm-worker-base` image family and any warm VMs (`gcloud compute instances delete`).
  - Update `CLAUDE.md`, `README.md`, and any docs that reference `A-p` / `A-l` or the dispatch loop.
- **Out of scope for this phase:** deleting the `cm-manager` VM itself or the FastAPI server — both keep running, the FastAPI server keeps owning planning, the VM just gains the daemon as its second tenant.
- **Acceptance criteria:**
  - `git grep -i 'dispatch\|warm.pool\|gs://cm-sessions\|ttyd\|cm-worker-base' -- ':!doc/'` returns no live references.
  - The `cm-manager` `claude-manager.service` systemd unit still starts cleanly with the deleted lifespan hooks removed; planning CRUD works.
  - The TUI no longer offers `A-p` / `A-l` (or offers them with an "obsolete" toast — chosen during the phase).
  - All TUI integration tests + API integration tests pass.
- **Dependencies:** Phase 3, and a deliberate decision-point: the user runs on the daemon for some period (a week or so) before this phase lands, to validate the persistent-host model in real use before deleting the fallback.

## Testing strategy

- **Phase 1:** Existing TUI integration tests run against the daemon (the daemon being auto-launched preserves the test harness's `~/.cm/` fixture pattern). One new test for ring-buffer replay on TUI restart. Manual smoke: `A-n` → `A-a` → `A-f` (feedback workflow) → `A-w`.
- **Phase 2:** Unit tests for the daemon's event broadcaster (subscriber join/leave, replay buffer for late subscribers). Integration test that asserts a feedback workflow's transitions are observable via `events.subscribe` without anyone tailing `events.jsonl`.
- **Phase 3:** Integration test against a daemon running in a local container reachable via TCP+TLS (self-signed cert). Auth failure cases. Manual end-to-end run against the real `cm-manager` VM before declaring done.
- **Phase 4:** Mostly negative — `git grep` checks above, plus running the API + daemon + TUI together on the VM for a few days of real workloads before deleting the old worker base image.

The TUI's render layer is the part that can't be unit-tested cleanly today. The Phase 1 acceptance criteria explicitly call out a manual smoke pass; expand it to a written checklist in the PR description, not a new test framework.

## Rollout / migration

The migration is incremental and reversible at every phase boundary.

- After Phase 1, every existing local workflow uses the daemon. The home-directory layout is identical apart from the new `~/.cm/daemon.sock` and the additive `last_exit` field on `ManifestEntry` (older binaries simply ignore it). `mcp_server/control_client.py` still falls back to `~/.cm/tui.sock` for external scripts that haven't picked up the new `CM_TUI_SOCKET`. Rolling back means reverting the binary; user data is untouched.
- After Phase 2, the TUI no longer reads workflow files directly. Still fully local; rollback path is the same.
- After Phase 3, the user's `hosts.toml` may define a remote host but it's opt-in; not configuring it leaves the laptop running exactly as Phase 2 did. Cloud-mode (`A-f` to a GCP worker) still works in parallel via the existing dispatch daemon.
- Phase 4 is the only destructive phase. It should land only after the user has been on the persistent-host model for long enough to be confident (≥1 week of regular use). The PR description should explicitly note "cloud rollback path is now `git revert`."

No database migration is required at any phase.

Two on-disk schema changes total, both additive and both keyed off `#[serde(default)]` so manifests written by an older binary load cleanly. Each Phase's PR includes a smoke check that loads a manifest produced by the previous phase's binary.

**Phase 1: `last_exit` on `ManifestEntry`.** `tui/src/app.rs:276-390` gains:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub last_exit: Option<LastExit>,

struct LastExit {
    code: Option<i32>,                       // PTY child exit code, when known
    memory_cap_kill: bool,                   // true iff the cgroup OOM-killer fired
    kills_file_offset: Option<u64>,          // byte offset into ~/.cm/memory_kills/<uid>.jsonl
                                             // for the matching record, when one exists
    exited_at: f64,                          // unix seconds; matches SessionTombstone.exited_at
}
```

Backfill rule for existing entries: missing or null `last_exit` means "no recorded exit metadata" — the TUI falls back to scanning `~/.cm/memory_kills/<uid>.jsonl` at startup, exactly as today, so behavior on first run with the new binary against an old manifest is unchanged. Going forward, the daemon writes `last_exit` whenever a session transitions to exited (whether by memory cap kill, normal exit, or signal) and includes it in `manifest.watch` diffs.

**Phase 3: `host_id` on `ManifestEntry`.** Same struct, additional field:

```rust
#[serde(default = "default_host_id_local")]
pub host_id: String,

fn default_host_id_local() -> String { "local".into() }
```

A *missing* `host_id` field is interpreted as `"local"` — which covers the only case that arises in practice, since older binaries never write the field. Explicit `null` would fail to deserialize as `String`, which is acceptable: we never emit `null` ourselves, and a hand-edited manifest with `"host_id": null` is operator error. (If we ever need to accept `null` too, switch to `Option<String>` with a custom `deserialize_with` that maps both `None` and `Some(null)` to `"local"` — but that's not warranted now.) The TUI writes the explicit value going forward. The Phase 3 PR includes a smoke check that confirms a Phase-2-era manifest (which has `last_exit` but no `host_id` field at all) still loads.

All other on-disk files (worktrees, `~/.cm/workflow-runs/<id>/state.json`, `events.jsonl`, `~/.cm/memory_kills/*.jsonl`, `~/.claude/projects/*`, `~/.codex/sessions/*`) are unchanged across all phases. The daemon reads them in place.
