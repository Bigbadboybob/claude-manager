# Always-running agents with responsive interaction

Brainstorm, 2026-09-07. These are options and recommendations, not an approved implementation specification. No runtime changes were made during this investigation.

## Main recommendation

Give long-running agents a stable execution home and make the laptop an immediately responsive client. Start by fixing CM's stalls and reconnect behavior on the existing host. Then compare that experience with an always-on Linux machine at home before committing to hardware or a migration system.

The desired experience is: close the laptop, travel, open it somewhere else, and reconnect to the same running agents and worktrees. Moving the viewing connection should be routine; moving the running work should be exceptional.

Three issues need separate treatment:

1. **Execution continuity:** the computer running an agent needs power and connectivity to its dependencies, independently of the laptop.
2. **Interaction latency:** typing, navigating, and reading cached output should happen locally; remote execution and fresh results still take network time.
3. **Recovery and scale:** losing a connection must not strand a pane or make the selected session wait for dozens of background terminals.

An always-on server solves the first issue. It does not automatically solve the other two. During a complete laptop network outage, server-side agents can continue, and a local client can show cached state and accept drafts; live interaction resumes when connectivity returns.

## What the current system shows

These findings combine source inspection with a small live snapshot. They are not a controlled reproduction of the reported freezes, and the deployed executable was not matched to this checkout.

- The configured remote host uses `ssh-unix`. `HostPool` manages one forwarding tunnel per host **per client instance**, and session attachments use individual streams through it. The proposed move from one SSH login per agent to a shared tunnel is already substantially present. See `tui/src/host_pool.rs:340` and `:750`, and `tui/src/client_session.rs:372`.
- The live process snapshot contained **29 CM forwarding tunnels**. One belonged to the running CM client; **28 had parent PID 1**, with ages of roughly 1.7–29.7 hours. This demonstrates orphaned forwarding processes, but does not establish their traffic, whether any still have users, or their contribution to latency. Normal cleanup lives in `SshTunnel::drop`; abrupt client termination needs separate lifecycle handling.
- Five ICMP probes to the configured cloud host measured **40.4–44.5 ms round-trip, averaging 42.4 ms, with no loss in that sample**. This is a baseline for that route at that moment, not a measure of application latency or commute reliability.
- The local `~/.cm/slow-ticks.log` contained **107 logged slow phases in the preceding 24 hours**. Examples included `drain_control_events` at **8.61 seconds**, `input` at **6.82 seconds**, and `maybe_adopt_daemon_sessions` at **5.25 seconds**. These are time spent inside CM's main-loop phases, which can include waiting on other components; they are not necessarily CPU work. The log only records slow phases, so it cannot provide overall latency percentiles. Instrumentation is in `tui/src/main.rs:441`.
- Remote attaches use **one worker thread** consuming requests sequentially. Attach requires at least the `session.attach` and `attach.open` exchanges. The dispatcher walks the pending collection without an explicit selected-session priority. Reconnecting many sessions can therefore impose queueing on the pane the user wants, even though the UI thread is free. See `tui/src/attach_worker.rs:78`, `tui/src/app/remote.rs:227`, and `tui/src/client_session.rs:395`.
- Background terminals already avoid some unnecessary redraws, but their terminal grids are still processed. Avoiding a repaint is different from avoiding transmission and parsing of hidden terminal output. See `tui/src/app/events.rs:418`.
- The source already includes SSH keepalives, tunnel-generation checks, and reconnect handling. Their existence does not establish that the reported refresh-dependent freeze is solved. A follow-up should reproduce it and check the deployed version rather than proposing those mechanisms as entirely new.
- **The daemon and holder are Linux-only.** A Mac Mini would need a compatible Linux VM or a portability project. A Linux x86 mini PC is the more direct fit for today's deployment. See `daemon/src/lib.rs:3` and `holder/src/main.rs:38`.

The evidence supports investigating application stalls, tunnel lifecycle, and attach scheduling before attributing the experience to geographic distance or SSH connection count alone.

## Options

| Option | What it buys | Main tradeoff | Assessment |
|---|---|---|---|
| Existing cloud host, improved CM client | Continuity without hardware or routine transfers | Requires fixing software stalls; route and host capacity still matter | Best first experiment |
| Always-on Linux computer at home | Very low latency on the home LAN; stable workspace and potentially ample RAM/CPU | Home power, internet, and remote access become dependencies | Strong long-term candidate |
| Mac Mini with Linux VM | Quiet home hardware with the same stable-workspace model | VM lifecycle, Linux architecture compatibility, and toolchain validation | Reasonable if there is another reason to prefer a Mac |
| Nearby persistent VPS/dedicated host | Potentially better routes or more predictable resources, without home operations | Recurring cost; provider location alone does not prove a better route | Compare using real routes and workload measurements |
| Stable split: continuous work remote, selected interactive work local | Keeps local-only workflows convenient without frequent migration | Two execution locations and some coordination remain | Useful compromise |
| Laptop plus cellular connectivity | Smallest behavioral change for commuting | Laptop must stay awake; coverage and power limit continuity | Bridge or experiment, not dependable overnight infrastructure |
| Automated checkpoint-and-resume migration | Local execution when wanted, remote continuation when prepared | Cannot transparently move every live process or in-flight operation | Secondary capability |

### Home machine

The strongest argument is a stable place for repositories, worktrees, dependencies, agent processes, and background services. Proximity is a bonus: being physically nearby only helps when the network route is also short. At home the LAN should be excellent; elsewhere the route, home upload, and any VPN relay determine responsiveness.

Try an existing spare Linux computer before buying anything. Size RAM and CPU from concurrent agents **plus** compiles, tests, browsers, and other tools. Session count alone is a poor sizing metric. For unattended operation, use wired networking, disable sleep, configure boot recovery, and consider power backup for both the computer and networking equipment.

A private overlay network can simplify access. Check whether it connects directly or through a relay when evaluating latency. Keep active worktrees on the execution machine's own filesystem; putting them on a network mount can reintroduce latency and connectivity dependence into every file operation.

### Run the whole terminal UI beside the agents

An additional, inexpensive experiment is to run CM's terminal interface on the execution host and access that one interface through a persistent terminal session. This aggregates all agent terminals on the server and sends only the visible CM screen across the network.

`tmux` provides terminal-session persistence; Mosh can improve roaming and perceived typing responsiveness through prediction. They do not remove application stalls or make actual remote operations instantaneous. Clipboard, keyboard, mouse, and terminal-rendering behavior need checking. This can be both a usable configuration and a comparison that reveals whether multi-session client processing is the expensive part.

### Stable split without routine transfer

Choose execution location when a task begins. Continuous orchestrators and work likely to run overnight start remotely and stay there. Tasks needing the laptop's devices, browser state, or local environment can stay local. All appear in one interface.

This is less seamless than one execution home, but much simpler than making every commute a migration event. Routine collaboration should exchange commits, artifacts, and explicit task handoffs, rather than continuously synchronizing two live writable copies of one worktree.

## Software changes with the largest likely payoff

### 1. Make disconnect recovery reliable

- Distinguish an idle agent from an unhealthy connection using application health checks; silence from a terminal is not itself a failure.
- Preserve agent execution independently of client disconnects, refreshes, and reconnects.
- Reconnect the selected pane first, then a small recent set. Use bounded concurrency and discard obsolete queued requests.
- Resume output using sequence numbers and a valid checkpoint; rebuild from a snapshot when history has a gap.
- Show cached output immediately with an explicit connection state. Retain prompt drafts locally and distinguish queued, acknowledged, and uncertain submissions so reconnect never blindly duplicates a consequential command.
- Make the tunnel's owner and lifetime explicit. Candidate mechanisms include a shared local connection service or supervised subprocess lifetime; orphan cleanup should verify ownership and usage before terminating anything.

### 2. Observe many agents without attaching to every terminal

Keep a lightweight host-level feed of task state, last activity, completion, and recent message summaries. Stream full terminal output only for visible panes and a small warm cache of recent panes.

The daemon should continue recording output and provide a valid screen snapshot plus subsequent updates when a pane becomes visible. Arbitrarily dropping terminal bytes is unsafe because escape sequences and earlier state affect later rendering. Full transcript/history access should be separate from live-screen reconstruction.

This changes routine client cost from “all the output of all agents” toward “fleet status plus the few agents being inspected.” Continuous tasks remain running without requiring live terminal viewers.

### 3. Keep interactive work ahead of background work

Move blocking I/O, including local daemon RPCs, out of the UI loop. Bound queue draining and background processing by a time budget. Prevent a busy terminal or history replay from delaying input and control traffic.

An improved protocol could multiplex typed events and terminal streams over persistent connections, but its scheduling, backpressure, recovery, and subscription model matter more than replacing JSON or SSH by itself. One TCP connection also shares packet-loss stalls across its streams; QUIC or a small separate interactive/control connection may help if measurements show that issue. Avoid a transport rewrite without evidence.

### 4. Give prompts a local editor

A locally rendered prompt editor makes keystrokes immediate while the agent remains remote. On submit, send the complete prompt and track acknowledgement. Navigation and scrolling through cached content can also remain local during an outage.

Structured conversation events could eventually support a richer CM client that doesn't need to reproduce every agent's terminal UI. Keep terminal access for shells and tool interactions that need it. That is a larger product change than fixing the current attach path.

## Making migration less annoying

The existing push/pull design transfers a worktree and conversation state and resumes an agent; that is different from migrating a running process and all of its tools.

If migration remains useful, make it **prepare early, hand off at a safe boundary**:

1. While the source runs, provision the destination's repository, dependencies, paths, and credentials, and pre-copy compatible state.
2. Request a move after the current operation or turn, rather than requiring the user to manually pause everything.
3. Quiesce the source agent and its relevant worker processes, finish the checkpoint, and transfer the final changes.
4. Confirm the destination is ready, transfer execution ownership once, and resume with the same visible task identity.
5. If transfer fails, retain a recoverable source checkpoint. Do not let both copies perform work while ownership is uncertain.

Conversation history is only part of the state. Open shell jobs, browser sessions, untracked and ignored files, local databases, credentials, agent-specific metadata, child agents, and in-flight external side effects complicate the handoff. Transcript resume cannot promise seamless continuation of all of those.

Preparation can shorten a planned departure's pause. It cannot rescue a laptop that already lost connectivity before transferring its latest state. VM/process live migration is an even more constrained option across different OS/CPU/tool environments, and would be a poor first investment here.

## Suggested sequence

1. **Make the existing setup measurable.** Reproduce a refresh-dependent freeze. Correlate UI phase timings, daemon response time, tunnel health, attach queue depth, bytes processed, and host CPU/memory. Compare a plain remote terminal with CM on the same route, under quiet and busy fleets.
2. **Fix observed stalls and connection lifecycle.** Investigate the orphan tunnels, remove UI-thread waits, prioritize the selected session, and verify recovery after a controlled client network interruption. Do not restart agents to reconnect their viewers.
3. **Decouple fleet visibility from terminal attachment.** Keep all tasks visible while streaming only the panes that need live output. This improvement benefits home and cloud hosts equally.
4. **Compare execution homes.** Use a spare home Linux machine and the repaired cloud setup from home, work, and a hotspot. Judge real interaction, capacity, and recovery, not a geographic assumption or one ping sample.
5. **Choose the default location.** Prefer one stable home for long-running work. Build polished migration only if actual use still requires moving execution.

Useful acceptance criteria should describe the experience: typing and navigation remain responsive during a busy fleet; the selected pane is the first to recover; background agents keep working when the laptop disconnects; queued input is not duplicated; and repeated client restarts do not accumulate tunnels. Set numerical targets after establishing the baseline rather than treating numbers invented in this brainstorm as requirements.
