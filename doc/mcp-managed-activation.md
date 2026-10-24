# CM-managed MCP activation at a safe boundary

Status: design (2026-10-24), msgfix lane, Swarm Focused Design. Not built. A
prototype proxy (`mcp_server/mcp_proxy.py`) and its tests are on this branch;
§10 lists what they prove and what they don't.

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
| Session monitors (`monitor_sessions`, auto monitors) | asyncio tasks in `server.py`, plus the journal `~/.cm/monitor-state/<uid>.json` | **no**: shutdown marks them `interrupted`, and nothing re-arms them. Needs §5 |
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

1. **Quiet check.** Wait until no request is in flight in either direction.
2. **Preflight.** Start a candidate child with these env vars:
   `CM_MCP_PROXY=<proxy version>`, `CM_MCP_PROXY_GENERATION=n`, and
   `CM_MCP_PREDECESSOR_PID=<old child>`. Send it the saved `initialize`
   params with a proxy-owned id, then `notifications/initialized`, then
   `tools/list`. The proxy reads both replies and doesn't forward them. If
   the candidate errors, exits, or doesn't answer within 30 s, it is killed
   and the old child keeps serving.
3. **Hold.** Buffer client input. Wait for any request that arrived after
   step 1 to finish on the old child.
4. **Stop the old child.** Close its stdin, which is the normal MCP
   shutdown, so its lifespan `finally` runs. Escalate to SIGTERM and then
   SIGKILL, each after 5 s. Its exit releases `consumer.lock` and writes the
   monitor journal.
5. **Switch.** Point routing at the candidate, flush the buffered input, and
   send `notifications/tools/list_changed` if the tool names differ.

The pause is the old child's shutdown time (well under 1 s in the
prototype). Requests that arrive during the pause are delayed, not lost.

The candidate starts before the old child stops, so for a moment both
children exist. Two rules make that safe:

- The notification consumer already arbitrates this overlap: it was built
  for the reconnect overlap ("A reconnect can briefly overlap the old MCP
  child", `notifications.py::_wait_for_lock`). The candidate waits for the
  lock, and the old child's exit hands it over.
- Monitor adoption waits for the predecessor to exit (§5).

### 3.4 Crash of a serving child

If the serving child exits outside an activation:

- The proxy answers each in-flight client request with an error ("CM MCP
  backend restarted; retry the call"). It never replays requests.
- It restarts the backend with backoff (1 s, 2 s, 4 s).

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
  The Codex 0.164 binary contains `notifications/tools/list_changed`
  handling, but this is not yet proven end to end (§10). The notification
  consumer lives in `native_codex.py` and is not affected by a swap.

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

**Pending.** Each deploy writes a version stamp last:
`mcp_server/VERSION`, containing the git SHA and a content hash of the
payload, written with an atomic rename. Every 10 s a backend stats that file
in its own directory. When the stamp differs from the version it loaded,
an update is pending.

**Activate** when either of these holds:

1. **Explicit:** the agent calls `ack_reconnect()`. The tool returns
   `{"activating": true, "after": "this reply", "version": …,
   "monitors_carried": N}`. Then the backend emits
   `notifications/cm/activate`, and the proxy swaps once that reply has been
   written.
2. **Idle:** all of the following hold, checked every 10 s through
   `resolve_authorized_session` and local state:
   - the agent has been idle (`agent_state` not Working or
     WorkingBackground) for at least 60 s;
   - its unread inbox count (DMs + mentions) is 0;
   - the native queue has no notice in `pending` or `submitting`;
   - no tool call is in flight (the proxy also checks this).

   When they hold, the backend emits `notifications/cm/activate`.

The proxy treats `activate` as a request, not a command. It swaps only
when it also sees no request in flight in either direction. After an
explicit ack it swaps right after the ack reply is written; for the idle
rule it also requires 60 s since the last client request. A request that
races the activation is covered by the hold in §3.3 step 3.

Both rules are EP's (round 5, `cm-initiative/shared/ep-interviews.md`).

## 5. Monitors: re-arm, not lose

Today shutdown flips `watching` and `fired` records to `interrupted`, and
nothing picks them up again. Two changes fix this:

1. **Persist enough to resume.** Add `baseline`, `report_anchor`,
   `deadline` (absolute), `edge`, and per-session completion state to the
   journal `FIELDS`. All of these are small JSON values: `baseline_for`
   returns `{"kind": "turn"|"size", "value": …}`.
2. **Adopt on activation.** A backend started with
   `CM_MCP_PREDECESSOR_PID` waits until that pid has exited (via pidfd, or
   by polling `/proc`). It then takes every `interrupted` record of the
   predecessor's producer. Producers record their pid for this. For each
   record it:
   - restarts the watch with the **original** baseline and deadline;
   - marks the predecessor's copy `replaced` (already in `SETTLED`), in one
     `monitor_state.publish`.

   A turn that ended during the swap gap is then still a new edge against
   the original baseline, and it fires. A watch that would have timed out
   during the gap fires `timed_out`, as before.

A fired monitor whose notice is already queued needs no adoption: the queue
holds the notice durably, and `_refresh_native_receipts` already reconciles
receipts across producers. A crash restart (§3.4) uses the same adoption
path, because the predecessor pid is set for every proxy start after the
first.

Session exit and the continuous drain keep their current semantics. The
drain still sees unfinished obligations in the journal until they are
adopted.

## 6. How CM knows each session's MCP version

Each backend writes `~/.cm/mcp/<uid>/status.json` (0600, atomic) at
startup and whenever its state changes. It contains:

- `version`, the stamp it loaded;
- `proxy` (from `CM_MCP_PROXY`) and `generation`;
- `pid` and `started_at`;
- `pending`: the stamp on disk if newer, plus `pending_since` and the
  reason activation is blocked (`working`, `inbox:3`, `notice_pending`,
  `in_flight`);
- `last_activation`: when, from which version, monitors carried, and any
  preflight failure.

`ping` returns the same object as `mcp`.

The brain reads these files directly. Status is host-local, as sessions
are, so no new RPC is needed and `control/methods.rs` isn't touched. A
later slice can add the field to `list_sessions` and the A-i peek.

A session with no proxy (anything spawned before rollout, like EP today)
has no fresh `status.json`. A `/proc` scan finds its `server.py` by
`CM_TUI_SESSION_ID` in the environ, and the session is reported as
`proxyless`.

## 7. What the agent and the release lead see

**Agent.** One native notice per pending version, and only for an agent
that is working (an idle agent is simply activated within about 60 s;
waking it to say so would cost a turn):

> `[cm-mcp-update] CM MCP update abc123 is pending. It activates when you
> call ack_reconnect() or after 60 s idle. Your 3 monitors and all chat
> watches carry over; nothing is re-sent.`

The monitor count and the wording come from the backend, so the text can
change with deploys. After activation, the agent sees nothing it has to act
on. The tool list may refresh (`list_changed`), and `ping().mcp` shows the
new version and `last_activation`.

**Release lead.** `scripts/cm-mcp-versions [--ssh <host>]` prints one row
per live agent session:

```
uid  label  engine  proxy  version  pending  blocked  since  monitors
```

Rows are sorted by staleness. Proxyless sessions are listed separately as
"needs one manual /mcp reconnect". Activation failures, crash-loop
hold-downs and `blocked` reasons are flagged. This is the old-version view
from EP's requirements. A `daemon.health` count (`mcp_outdated`) and a TUI
marker can follow later; they are not needed to ship.

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
  session. It should be as near-frozen as `cm-holder`: no MCP SDK import,
  no policy, and a version string in `status.json` so the release lead can
  see stale proxies.

## 9. Failure and rollback

| Failure | Behavior |
|---|---|
| New payload fails preflight (import error, `initialize` error, no reply in 30 s) | The candidate is killed and the old child keeps serving. `status.json` records `preflight_failed: <version>`. There is no retry until the stamp changes again, so it doesn't loop. |
| New payload passes preflight and then crashes | The §3.4 restart: errors for in-flight calls, backoff, then held down after 3 crashes in 10 minutes. |
| Old child won't exit | stdin EOF, then SIGTERM, then SIGKILL, 5 s each. Monitors are adopted from the journal (a SIGKILL'd child may not have written `interrupted`, so a dead predecessor's `watching` records are adopted too). A notice it left `submitting` becomes `uncertain`, with no resend. |
| Code rollback | Redeploy the previous payload and write a new stamp. It activates through the same boundary. A held-down proxy picks it up within 60 s. There is no special rollback path. |
| Proxy bug | The engine sees the MCP server fail, as it does today when `server.py` dies. Recovery is a manual `/mcp` reconnect, and turning the launcher's proxy flag off (`CM_MCP_PROXY_DISABLE=1` in the launcher, written by `ensure_launcher`) makes that reconnect go direct to `server.py`. |
| Activation during an Owner-visible prompt | Can't happen. A permission prompt is either a request in flight (elicitation) or an agent that is working. |

## 10. Prototype: what is proven

Branch head: `mcp_server/mcp_proxy.py` (prototype) and
`mcp_server/tests/test_mcp_proxy.py`. The trigger is SIGUSR1 rather than
`notifications/cm/activate`.

**Proven on Claude Code 2.1.310** with `claude -p` and Haiku 4.5. Evidence
is in `~/.local/share/msgfix/mcp-activation/` (`run-claude.log` and the
transcript copy). In one turn the model called `probe_version` and got
`version=1 pid=769596 generation=1`. A Bash step then swapped the backend.
The next call to the same tool returned `version=2 pid=770670
generation=2` over the same connection, with no reconnect. The tool that
exists only in v2 (`probe_new_tool`) became visible and was called
successfully. So Claude honors `list_changed` mid-turn, and the swap is
invisible to it.

**Unit tests** (3, stdlib backend, passing) cover three cases:

- a swap keeps the connection and announces new tools;
- a slow call in flight finishes on the old backend before the swap, and the
  next call goes to the new one;
- a candidate that dies in preflight leaves the old backend serving.

**Not proven yet:**

- Codex end to end: list_changed handling is visible in the binary but has
  not been exercised.
- Running real `server.py` behind the proxy with channel events and the
  native consumer handoff.
- Monitor adoption, which is new code (§5).
- Long-session behavior.

These are the first build-phase tests.

## 11. Rollout and build slices

1. Proxy hardened (crash restart, hold-down, `notifications/cm/` intercept),
   `launcher.sh` integration behind a flag, and the `claude_parent_argv` fix.
2. The `VERSION` stamp in the MCP payload deploy (HOWTO §3 and the release
   script), `status.json`, `ping().mcp`, and `scripts/cm-mcp-versions`.
3. The backend activation policy: `ack_reconnect`, the idle rule, and the
   pending notice.
4. Monitor persistence and adoption (§5).
5. Turn the launcher flag on. **Every existing session then needs one last
   manual `/mcp` reconnect** to get the proxy; new spawns get it
   automatically. After that, MCP deploys activate without Owner.

Slices 1–2 alone already help the release lead (the version view and crash
self-heal). Slices 3–4 remove "Owner, please reconnect X".

Daemon changes are limited to `mcp_config.rs` (launcher rendering) and
optionally reading `status.json`. There are no `control/methods.rs` changes
and no RPCs.
