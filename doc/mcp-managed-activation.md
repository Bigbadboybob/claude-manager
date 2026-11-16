# CM-managed MCP activation at a safe boundary

Status: built (slices 1–4, 2026-10-24), msgfix lane, Swarm Focused Design;
not deployed, flag off by default. §10 lists what is proven and how.

## 1. Problem and recommendation

New MCP code reaches an agent only when someone types `/mcp` → reconnect in
the agent's pane. Agents can't do that themselves, so every MCP deploy ends
with "Owner, please reconnect X". On 2026-10-24, EP's server (pid 2740452,
started Oct 20) was still running code from before 3e45fd4, at about 70% CPU.

**Recommendation: a small, stable stdio proxy between the engine and
`server.py`.** The engine's MCP client talks to the proxy for the life of the
session. The real server runs as the proxy's child, and the proxy can replace
that child. The steps are:

1. Start the new child.
2. Replay the client's original `initialize` to it and check its tool list.
3. Stop the old child.
4. Resume forwarding.
5. If the tool list changed, send `notifications/tools/list_changed`.

The engine never sees a disconnect, so to the engine the "reconnect" is
invisible.

The same mechanism works for both engines. In Claude Code, the MCP client is
the `claude` process. In Codex, it is CM's owned `codex app-server`. Both
already start `~/.cm/mcp/launcher.sh server.py`, so the proxy goes in at the
launcher and needs no new engine integration.

The split of responsibilities follows the holder/brain split:

- **The proxy holds mechanism only.** It is stdlib only, about 250 lines, and
  is replaced only by a real reconnect.
- **The backend holds policy.** This includes when to activate, what to tell
  the agent, and how to re-arm monitors. Policy ships with every MCP deploy.

## 2. What lives in the MCP process today

| State | Where it lives | Survives a backend swap today? |
|---|---|---|
| Chat cursors | Held by the agent; the daemon validates them | yes, untouched |
| Chat read receipts / acks | Daemon messaging store | yes |
| Chat watches and follows (`chat_monitor`, `chat_follow`) | Daemon (`_chat_call`) | yes |
| Session UID, chat name, board-holder identity | `CM_TUI_SESSION_ID` env plus the daemon's participant UID | yes: the child inherits the env from the proxy |
| Native notification queue | `~/.cm/notifications/<sha256(uid)>/` on disk, with flock | yes. The consumer lock is released when the old child exits. A notice left `submitting` becomes `uncertain`, with no resend, so the boundary rule excludes that case (§4) |
| Claude channel / own-child socket consumer | The `server.py` child (`native_claude.run`) | it restarts in the new child. The channel's MCP session is bound on the replayed `tools/list` |
| Codex notification consumer | `native_codex.py`, the launcher of the app-server, **not** `server.py` | unaffected |
| Session monitors (`monitor_sessions`, auto monitors) | asyncio tasks in `server.py`, plus the journal `~/.cm/monitor-state/<uid>.json` | yes, with §5: the successor adopts them from the journal (before this work, shutdown marked them `interrupted` and nothing re-armed them) |
| Blocking tool calls (`wait_for_*`, `send_input_and_wait`, `get_backtest_result(wait)`) | The in-flight request | the proxy never swaps while any request is in flight |
| Agent guide (`instructions`) and server capabilities | The `initialize` reply, read once by the engine | **no**: the engine keeps the first reply. See §8 |

## 3. Mechanism

### 3.1 Insertion point

`launcher.sh server.py` execs `mcp_proxy.py` from the chosen candidate
directory. The proxy's backend argv is `launcher.sh server.py` with
`CM_MCP_PROXY_CHILD=1` set, and with that variable set the launcher execs
`server.py` directly. That gives each new backend the launcher's existing
drift-proof resolution (first working python/server pair) at every
activation.

The proxy's own code is loaded once, so payload updates on disk don't change
a running proxy. `launcher.sh` is regenerated at daemon startup
(`mcp_config::ensure_launcher`), so a launcher change reaches every session
at its next spawn or reconnect.

### 3.2 Forwarding

The proxy forwards line-delimited JSON-RPC in both directions. It records:

- the client's `initialize` request and whether `notifications/initialized`
  has been sent;
- in-flight client→server requests (id → method, start time);
- in-flight server→client requests (elicitation, roots, sampling).

It rewrites one field: the `initialize` reply advertises
`capabilities.tools.listChanged = true`, so that it can announce tool
changes later.

Backend messages with a `notifications/cm/` method prefix are intercepted
and never forwarded. That prefix is the channel from the backend to the
proxy (§4).

### 3.3 Activation sequence

The activation runs as its own task, so a client EOF or a backend crash is
handled while it waits. Every wait in it gives up when the proxy is closing
or the serving child has died.

1. **Quiet check.** "In flight" means requests actually forwarded to the
   serving child, in either direction; input the proxy is holding never
   counts. Wait at most `QUIET_WAIT_S` (10 s) for nothing to be in flight,
   or give up until the next boundary.
2. **Preflight.** Start a candidate child with `CM_MCP_PROXY_CHILD=1`,
   `CM_MCP_PROXY_VERSION`, `CM_MCP_PROXY_GENERATION=n`, and the old
   child's identity, `CM_MCP_PREDECESSOR_PID` plus `CM_MCP_PREDECESSOR_START`
   (its start time, so a reused pid can't be mistaken for it). Send it the
   saved `initialize` params with a proxy-owned id, then
   `notifications/initialized`, then `tools/list`. The proxy reads both
   replies and doesn't forward them. Meanwhile client traffic keeps flowing
   to the old child. If the candidate errors, exits, or doesn't answer
   within 30 s, it is killed and the old child keeps serving.
3. **Switch.** A call that reached the old child during the preflight is
   waited for, again for at most `QUIET_WAIT_S`, without holding anything.
   If it's still running (a long `wait_for_*`), the activation is abandoned
   and the candidate killed. Once the old child is quiet, routing moves to
   the candidate. No `await` separates that check from the switch, so no
   request can be split between two children. `list_changed` goes out if
   the tool names differ.
4. **Hold until ready.** New client input is buffered until the candidate
   sends `notifications/cm/ready`, or for at most `HOLD_MAX_S` (5 s). Held
   messages are not in flight anywhere. A client response to a backend's
   request always goes straight to the backend that asked.
5. **Retire the old child** in the background: stdin EOF (the normal MCP
   shutdown, so its lifespan `finally` runs), then SIGTERM, then SIGKILL,
   5 s each. Its exit releases `consumer.lock` and writes the monitor
   journal.

A candidate is passive until promoted. It answers the preflight, but
starts its notification consumer, activation policy, status write and
monitor adoption only once its predecessor (that pid with that start
time; a PermissionError means another user's process, so not ours) has
exited. It then adopts its monitors and sends `ready`. The proxy reads
each child's start time when it spawns it, because after a crash the dead
child is already reaped by the time its successor needs to name it. If
the predecessor is still alive after 45 s, past anything the proxy allows
an undecided candidate, the backend fails loudly. It logs and exits rather
than start a second consumer. The proxy then treats that exit as a crash
(a serving child) or a failed preflight (a candidate). So an abandoned
candidate never takes `consumer.lock` or sends a channel notice whose output
the proxy would drop. Monitor tools can't run on the new child before
adoption, because client input is held until `ready`.

Measured (§10): the preflight takes 2–8 s, mostly `server.py` import, while
the old child keeps serving. Client input is held for well under a second.

### 3.4 Crash of a serving child

If the serving child exits outside an activation:

- The proxy answers each in-flight client request with an error ("CM MCP
  backend restarted; retry the call"). It never replays requests.
- It restarts the backend with backoff (1 s, 2 s, 4 s). A client
  `initialize` still pending is answered with an error, never left waiting.
- A reply line over 64 MiB, or a failure in the proxy's reader, kills that
  child loudly and goes through the same restart, so a reply never vanishes
  silently. A broken pipe to the engine shuts the proxy down with every
  child.

After 3 crashes in 10 minutes the proxy is **held down**. It answers every
request with a clear error naming the version and the crash count, and it
retries the start every 60 s. A fixed deploy therefore recovers on its own,
the same way the holder's breaker does. Today a crashed `server.py` needs a
manual `/mcp`; with the proxy it heals itself.

### 3.5 Per engine

- **Claude Code.** No engine change. The MCP stdio pipe is the one
  connection that stays up. Channel events (`notifications/claude/channel`)
  from the new child travel the same pipe. The own-child messaging socket
  (`CLAUDE_CODE_MESSAGING_SOCKET`) carries user messages *into* Claude and
  cannot ask Claude to reconnect MCP, so it is not an activation channel. It
  remains a delivery path that the new child reopens from the inherited env.
  One code fix is needed: `native_claude.claude_parent_argv` walks at most
  two parents and accepts only `sh launcher.sh` between `server.py` and
  `claude`. It must also accept `mcp_proxy.py`, or `channels_blocked` and the
  argv session-id fallback stop working.
- **Codex.** The proxy is the `claude-manager` MCP command that the owned
  app-server spawns. That command is set by
  `-c mcp_servers.claude-manager.command=…/launcher.sh` (`mcp_config.rs`).
  The notification consumer lives in `native_codex.py` and is not affected
  by a swap. **Measured on Codex 0.164** (§10): a swap applies new code to
  existing tools at once, even mid-turn, and the app-server refreshes its
  own tool cache on `list_changed` (a new thread sees new tools). But a
  thread that is already live keeps the tool list it started with, through
  later turns and `thread/resume`. So in a running Codex session new tools
  stay invisible, description changes don't show, and a changed schema
  stays the old one. Two rules handle this: tool schemas stay compatible
  with what running threads hold (enforced by
  `tests/test_tool_schema_compat.py` against `tool_schemas.json`, including
  nested `items`/`properties`/`enum` and `$ref`s: removing a tool,
  parameter or nested property, adding a required parameter or nested
  property, dropping an enum value, or changing a type fails
  unless marked with `mcp_version.py snapshot --write --allow-breaking
  "tool: reason"`); and Codex sessions are told when their tool set changed
  (`tools_changed_unseen` in `status.json`, a note in `ping().mcp` and in
  the pending notice, "New tools appear after a restart"). `A-R` picks the
  new tools up.

### 3.6 Alternatives rejected

- **Typing `/mcp` into the pane** (CM sends keystrokes). It races with Owner
  typing. It breaks the rule that notifications never send keyboard bytes.
  It reconnects every server, and it cannot run while the agent is mid-turn.
- **Codex `config/mcpServer/reload` through CM's Relay.** The app-server
  exposes this method (it is in the 0.164 method table). It is Codex only,
  and it restarts every MCP server for the thread. Its behavior with a tool
  call in flight is unknown, and it has no preflight or rollback. It is a
  fallback only if the proxy fails on Codex.
- **`server.py` re-execs itself in place** (same pid, same stdio fds). This
  needs no extra process. But the new code would have to fake the handshake
  inside FastMCP's read stream, and a bad payload kills the connection with
  nothing left to fall back to, because the old code has already exec'd
  away. The proxy gives a preflight, crash restart, and a stable place for
  the mechanism.

## 4. Boundary policy

The policy lives in the backend, `server.py`'s `activation` task, so it can
change with ordinary deploys. The proxy only enforces "nothing in flight".

**Pending.** Each deploy runs `mcp_version.py stamp [--git SHA]` on the
target after copying the payload. It writes `mcp_server/VERSION` (payload
content hash, tool-schema hash, git SHA) with an atomic rename. Every 10 s
a backend stats that file in its own directory. An update is pending when
the stamp's payload hash differs from what the backend loaded **and**
matches the files on disk (rehashed only when the stamp changes). So a copy
still in progress, or files edited after stamping, never activate, and an
unstamped dev checkout never activates anything.

**Activate** when either of these holds:

1. **Explicit:** the agent calls `ack_reconnect()`. The tool returns
   `{"activating": true, "after": "this reply", "version": …,
   "monitors_carried": N}`. Then the backend emits
   `notifications/cm/activate`, and the proxy swaps once that reply has been
   written.
2. **Idle:** all of the following hold, checked every 10 s through
   `resolve_authorized_session` and local state:
   - `agent_state.state` is `idle` and has been for at least 60 s
     (`working`, `working-background`, `waiting-on-human`, `errored` and an
     unknown state all block);
   - the unread inbox (DMs + mentions) is empty;
   - the native queue has no notice in `pending` or `submitting`, other
     than this update's own notice;
   - no tool call is in flight (the proxy checks this).

   `status.json` records the current blocker (`working`,
   `idle_too_recently`, `unread_inbox`, `notice_pending`, `no_proxy`, …).

   When they hold, the backend emits `notifications/cm/activate`.

The proxy treats `activate` as a request, not a command. It swaps only
when it also sees no request in flight in either direction. After an
explicit ack it swaps right after the ack reply is written; for the idle
rule it also requires 60 s since the last client request. A request that
races the activation is covered by the hold in §3.3 step 3.

Both rules are EP's (round 5, `cm-initiative/shared/ep-interviews.md`).

## 5. Monitors: re-arm, not lose

Before this work, shutdown flipped `watching` and `fired` records to
`interrupted`, and nothing picked them up again. Now:

1. **Persist enough to resume.** The journal keeps each record's `caller`,
   original `baselines`, `report_anchors`, absolute `deadline_at`, launch
   receipt and `interim_turn_ends`, plus each producer's `pid`.
2. **Adopt after the predecessor exits.** A backend started with
   `CM_MCP_PREDECESSOR_PID`/`_START` waits until that exact process has
   exited (its shutdown writes its last journal state). Producers record
   their pid and start time. `monitor_state.adopt(pid, start)` then moves
   every unfinished record (`watching`, `fired`, `interrupted`) of the
   producer matching both
   in **one** atomic journal write: the predecessor's copy becomes
   `replaced` (settled, `adopted_by`) and this producer's copy becomes
   `watching` (`adopted_from`). Each watch restarts from its **original**
   baseline and deadline, so a turn that ended during the swap is still a
   new edge and fires. A watch whose deadline passed in the gap fires
   `timed_out` at once. Records written before baselines were persisted are
   not resumed, because without their original edge they could fire on old
   news.

**Exactly once.** A record can't be resumed twice, because the handover is
a single rename: before it, only the predecessor owns the record; after
it, only the successor does. A failed write changes nothing and is
retried. A successor killed after adopting is itself the next successor's
predecessor, so the record moves on again. The notice can't be sent twice,
because its queue ID is the monitor ID (`monitor:<id>`) and the queue
refuses a second publication under that ID. If the predecessor published
and died before recording the delivery, the successor re-detects the edge,
finds the ID taken, and verifies the existing notice instead of sending
another. The one residual case is the queue evicting that settled notice
(past 512 retained events) during the gap; that is outside any realistic
swap window.

A crash restart (§3.4) uses the same path, because the proxy passes the
dead backend's pid. Session exit and the continuous drain keep their
current semantics: an `interrupted` record stays an obligation in the
journal until someone adopts it.

## 6. How CM knows each session's MCP version

Two files per session, both host-local, under `~/.cm/mcp/<uid>/`:

- `status.json`, written by the backend at startup and on each policy tick.
  It holds `version` (the stamp's, or the payload hash when unstamped),
  `payload_hash`, `tools_hash`, `toolset_hash` (tool names plus required
  parameters, which is what a Codex thread can notice; additive optional
  parameters leave it alone), `session_toolset_hash` (the set the engine
  saw first), `tools_changed_unseen` (Codex only), `engine`,
  `proxy` (`{version, generation}` from the env), `pid`, `started_at`,
  `pending` (`{version, payload_hash, tools_hash, tools_change}`) and
  `blocked`.
- `proxy.json`, written by the proxy: `proxy_version`, `proxy_pid`,
  `state` (`running`, `activating`, `restarting`, `held_down`, `stopped`),
  `generation`, `backend_pid`, `activations`, `recent_crashes`,
  `last_error` and `last_activation` (`mode`, `ok`, `from_pid`/`to_pid`,
  `preflight_ms`, `hold_ms`, `held_messages`, or the preflight `error`).

`ping().mcp` is the agent-facing summary: `{version, proxy,
update_pending?, note?}`.

Hashing is cheap and off the common path. The walk skips dot-dirs,
`site-packages`, `venv` and tests: about 25 files and a few ms, against
1,194 files and 0.5–1.6 s when it included `.venv`. A venv rebuild
therefore never changes the version. Only a backend behind the proxy
hashes at import, to pin what it loaded; elsewhere, nothing is hashed until
`ping` asks.

No new RPC, and `control/methods.rs` isn't touched. The release-lead
script reads the files directly. A later slice can add them to
`list_sessions` and the A-i peek.

A session with no proxy (anything spawned before rollout, like EP today)
has no `proxy.json`. A `/proc` scan finds its `server.py` by
`CM_TUI_SESSION_ID` in the environ, and it is reported as needing one
manual `/mcp`.

## 7. What the agent and the release lead see

**Agent.** One native notice per pending version, and only while the agent
is working (`working` or `working-background`). An idle agent is never
woken; it is simply activated about a minute later:

> `[cm-mcp-update v4dd…] A CM MCP update (v4dd…) is pending. It activates
> on its own after you have been idle for a minute (ack_reconnect() applies
> it sooner); your 2 monitors and chat watches carry over and nothing is
> re-sent.`

Codex sessions whose tool set changes get " New tools appear after a
restart." added. The wording comes from the backend, so it can change with
deploys. `ack_reconnect()` is documented as an optional shortcut. After
activation the agent has nothing to do: Claude's tool list refreshes, and
`ping().mcp` shows the new version.

**Release lead.** `scripts/cm-mcp-versions [--ssh <host>] [--json]`:

```
host <host>: launcher payload /opt/cm-daemon/mcp_server version <stamp>
  3 current, 2 behind, 68 pre-status code
uid  name  engine  proxy  version  state  age_h  notes
```

Rows run stalest first: `pre-status code` (a server from before this
work), then `behind`, then `current`. The notes column flags:

- "no proxy: needs one manual /mcp reconnect";
- the pending version and its blocker;
- a failed last activation;
- a proxy that is restarting or held down;
- "tool set changed, not visible until restart".

On cm-sessions today it lists 68 live servers, all pre-status and all
proxyless. That is the expected starting point.

## 8. Limits

- **The agent guide and server `instructions`.** The engine reads these
  only from the first `initialize` reply. A proxy swap can't push a new
  guide. Tool descriptions and schemas do refresh through `list_changed`.
  Mitigation: deploys that change `AGENT_GUIDE.md` say so in the pending
  notice, and the guide stays readable through `chat_norms` and the docs. A
  full guide refresh still needs a real reconnect or a new session. That is
  rare and not urgent.
- **Capabilities fixed at `initialize`.** An example is the
  `experimental: {"claude/channel": {}}` opt-in. A backend that would answer
  differently (say, channels enabled where they weren't) keeps the original
  answer. The proxy compares the two and logs the difference, and the
  backend stays consistent with what the client was told: it reads
  `CM_CLAUDE_CHANNEL` from the frozen env, as now.
- **The proxy itself.** Changing the proxy needs one real reconnect per
  session. It is kept near-frozen, like `cm-holder`: stdlib only, no MCP
  SDK import, no policy. Its version is in `proxy.json`, and in
  `status.json` through the env, so the release lead can see stale proxies.
- **Codex threads keep their first tool list** (§3.5).

## 9. Failure and rollback

| Failure | Behavior |
|---|---|
| New payload fails preflight (import error, `initialize` error, no reply in 30 s) | The candidate is killed and the old child keeps serving. `proxy.json` records `last_activation: {ok: false, error}`. The old backend's policy asks again at most once per minute while the stamp stays pending; a fixed stamp clears it. |
| New payload passes preflight and then crashes | The §3.4 restart: errors for in-flight calls, backoff, then held down after 3 crashes in 10 minutes. |
| Old child won't exit | stdin EOF, then SIGTERM, then SIGKILL, 5 s each. Monitors are adopted from the journal (a SIGKILL'd child may not have written `interrupted`, so a dead predecessor's `watching` records are adopted too). A notice it left `submitting` becomes `uncertain`, with no resend. |
| Code rollback | Redeploy the previous payload and write a new stamp. It activates through the same boundary. A held-down proxy picks it up within 60 s. There is no special rollback path. |
| Proxy bug | The engine sees the MCP server fail, as it does today when `server.py` dies. Recovery is a manual `/mcp` reconnect. With `~/.cm/mcp/proxy.disabled` present on the host, that reconnect goes direct to `server.py`. New spawns stop getting the proxy once `mcp_proxy` is removed from `daemon.toml`. |
| Activation during an Owner-visible prompt | Can't happen. A permission prompt is either a request in flight (elicitation) or an agent that is working. |

## 10. What is proven

Evidence is in `~/.local/share/msgfix/mcp-activation/` (probe runs) and
`~/.local/share/msgfix/sb/e2e-slice{1,3,4}.log` (scratch `cm-daemon` from
this branch with `mcp_proxy = true`, isolated HOME, real Claude Code
2.1.310 sessions on Haiku 4.5, with real `server.py` behind the proxy).

**Probe backend.**

- **Claude (`claude -p`):** in one turn, the same tool returned v1, then v2
  from a new pid after the swap. A v2-only tool became visible and was
  called, without `/mcp`.
- **Codex 0.164 (`codex exec` and a multi-turn `app-server` client):**
  - existing tools return v2 mid-turn;
  - a new thread sees the new tool;
  - the live thread doesn't, even after `thread/resume` (§3.5).

**Real `server.py` behind the proxy** (slice 1):

- `chat_open`/`chat_send` before and after a swap send from the same
  participant ID and name, and `ping` shows the same UID.
- A `wait_for_session_idle` call on a 40 s busy session, with an activation
  requested 12 s in, returned `{idle: true}` to the agent first; the swap
  ran after it.
- A DM became a channel event into Claude, and the agent replied.
- **Consumer-lock handoff with a pending notice:**
  1. the serving backend was frozen (SIGSTOP) while holding
     `consumer.lock`;
  2. a DM queued as `pending`;
  3. the activation SIGKILLed the frozen backend after 10 s;
  4. the new backend took the lock and delivered the notice (`observed`),
     and the agent replied.

**Policy** (slice 3):

- **Idle path:** blocked as `idle_too_recently`, then activated 76 s after
  the stamp (mode `idle`, hold 237 ms).
- **Ack path:** the notice reached a working agent mid-turn. Its
  `ack_reconnect` returned `{activating: true, version, from_version}`, and
  the swap followed its reply (mode `ack`).
- The live run found one bug: the notice also woke a just-idle agent. Fixed:
  the notice now goes to working agents only.

**Monitors** (slice 4): a monitor was armed on a busy session. The serving
backend was frozen, and the watched turn ended during the gap. After the
swap:

- the journal shows the predecessor's copy `replaced` and the successor's
  copy `delivered`;
- exactly one `monitor:<id>` notice was published (`observed`);
- the agent replied once.

**Unit tests** (all stdlib or mocked):

- **Proxy:** never forwards `notifications/cm/*`; in-flight calls finish on
  the old backend; a failed preflight keeps the old one; backend-requested
  ack and idle activation; idle refused after recent traffic; crash
  restart; crash-loop hold-down and recovery.
- **Launcher branch** and **`daemon.toml` flag** (Rust).
- **`claude_parent_argv`** through the proxy.
- **Stamp and pending consistency; status.json** and the Codex
  tool-set flag.
- **Schema compatibility guard.**
- **Policy:** each blocker; no notice to idle agents; the Codex suffix;
  ack.
- **Monitor adoption:**
  - a turn ending in the gap fires once;
  - a predecessor that published and then died is not doubled;
  - a successor that died after adopting hands over again;
  - a failed or transient handover write;
  - settled and pre-baseline records are left alone;
  - a live predecessor is never adopted from;
  - the deadline carries over;
  - registration persists the resume fields.

**Review round (2026-10-24), proxy tests against a concurrent stub:**
- the hold-window hang repro (ask, slow, probe during a 1 s candidate
  start) now answers every call;
- a long call reaching the old child during the preflight abandons the
  activation and kills the candidate, with no input held (a concurrent call
  is answered in well under 2 s);
- EOF mid-activation leaves no process;
- an old-child crash mid-wait recovers;
- a pending `initialize` is answered on a backend death;
- a client's answer goes back to the backend that asked;
- an oversized line kills and restarts loudly;
- a broken engine pipe shuts the proxy down;
- `ready` ends the hold early.

Real `server.py` behind the proxy without a daemon
(`tests/test_proxy_real_server.py`): `ready` ends the hold, and status.json
moves to the new backend only after promotion. The monitor-adoption tests
now include a reused pid.

**Not proven:** real `server.py` behind the proxy under Codex (only the
probe backend ran there), and multi-day sessions.

## 11. Rollout

Slices 1–4 are built on `cm-sub/msgfix-mcp-activation`, with the flag off.
To deploy:

1. Ship the brain, so new spawns stamp `CM_MCP_PROXY_ENABLED`.
2. Ship the complete MCP payload, then run `mcp_version.py stamp` on each
   host (HOWTO §3).

**Slice 5, the canary:** set `mcp_proxy = true` in cm-sessions'
`daemon.toml`. It is read at each spawn, so no reload is needed. New
spawns there get the proxy. cm-manager stays off until the canary has run
for a day. **Existing sessions need one last manual `/mcp`** to get the
proxy, and only once their host has the flag. After that, MCP deploys
activate without Owner. `scripts/cm-mcp-versions --ssh <host>` tracks the
progress.

The daemon change is limited to `mcp_config.rs`: the flag and the launcher
rendering. There are no `control/methods.rs` changes and no new RPCs.
