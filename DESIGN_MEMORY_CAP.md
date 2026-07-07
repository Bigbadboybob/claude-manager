# Session memory cap — design proposal

Branch: `cm/set-cap-on-session-memory`

## Goal

Stop runaway commands inside a Claude/Codex session from OOM-ing the host. When a session and its descendants collectively cross a soft threshold, kill the largest non-agent process in the session, preserve the agent itself, and surface what happened to both the user and the agent.

Out of scope: CPU caps, disk I/O caps, per-tool-call quotas, cloud workers (those already run on isolated VMs).

## Constraints

- **Linux + systemd only** for the cap mechanism. macOS dev users (if any) and any environment without a running user systemd instance must transparently fall back to "no cap, no watcher". No regressions on those paths.
- **Opt-in.** Off by default — the same `Session::new` path is used by tests (`Session::new("/bin/true", ...)` in `agent/tests.rs`, `workflow/controller.rs`, `control/methods.rs`) and a hard requirement on cgroup tooling would break them. Tests stay uncapped.
- **No new daemons.** Watcher must live in the TUI process so it dies with the TUI (no orphan supervisors).
- **The cap covers descendants for free.** That is the entire reason for using a cgroup — a `Bash`-tool subprocess that forks its own children is contained by the same cgroup as the agent, so one limit does the whole tree.

## Approach

Three pieces:

1. **Wrap the spawn** in a transient systemd user scope so the agent and every process it ever forks land in one cgroup with a memory limit attached.
2. **Watch the cgroup** from a TUI-side thread. When `memory.current` crosses the soft threshold, walk `cgroup.procs`, pick the highest-RSS PID that is *not* the agent root, and SIGTERM → SIGKILL it.
3. **Notify both audiences.** Push a line into the TUI activity feed (`A-,`) for the user, and append a sanitized JSON record to a host-side kill log under `~/.cm/` for the agent to consult. No PTY writes — see Component 4 for the rejected-channels analysis.

### Why a soft threshold instead of `MemoryMax`

`MemoryMax` triggers the kernel OOM killer, which picks by `oom_score`. That's usually the runaway child but not guaranteed — a bloated agent process (huge context window, leak in the CLI) would land on its own oom_score and we'd lose the agent and the runaway both. We want userspace control over *which* process dies.

So we use two limits per session:

- **`MemoryHigh` = soft threshold** (e.g. 6 GiB). The kernel throttles allocations and reclaims aggressively, but does not kill. This is our trigger: the watcher reacts to `memory.events` `high` counter increments.
- **`MemoryMax` = hard backstop** (e.g. 10 GiB). If the watcher is wedged or the agent itself is the offender and userspace can't recover, the kernel still stops the bleeding. This is the "the system is more important than this session" line.

Setting both means: under normal runaway conditions, we get clean userspace kills with notification; under pathological conditions, the kernel still prevents host OOM.

### Why systemd-run instead of raw cgroup writes

We could create our own cgroup via `/sys/fs/cgroup/...` writes, but:

- Requires the controller to be delegated to the user, which means depending on `systemd-run --user` having already done the `Delegate=yes` setup at login time anyway.
- `systemd-run --user --scope` gives us cleanup-on-exit for free (the scope unit dies when the last process exits, including on TUI crash).
- The path to the cgroup is well-known once we know the unit name: `/sys/fs/cgroup/user.slice/user-<UID>.slice/user@<UID>.service/app.slice/<unit>.scope`.

So the spawn becomes:

```
systemd-run --user --scope --quiet \
  --unit=cm-sess-<session-uid> \
  -p MemoryHigh=6G -p MemoryMax=10G -p MemorySwapMax=0 \
  -- <original shell> <original args>
```

## Components

### 0. Preflight (TUI startup, once)

Before any session is wrapped, the TUI must know whether `systemd-run --user` will actually *work* on this host — not just whether the binary is on PATH. There are several ways the wrapper command can spawn successfully but then the scope setup fails a moment later:

- No user systemd manager running (older WSL, some container images, minimal distros).
- `XDG_RUNTIME_DIR` unset or pointing at a stale path.
- cgroup-v2 unavailable, or the unified hierarchy not mounted.
- Scope unit name collision from a previous TUI run that didn't clean up.

In all of those, `systemd-run --user --scope -- <cmd>` exits non-zero *after* forking the PTY, which means by the time the TUI sees the failure the user is staring at a session that vanished a moment after opening. There is no in-band signal to `Session::new` to retry uncapped — `tty::new` returned a valid PTY, the child just exited.

So we run a **preflight probe** at TUI startup, exactly once. The probe must verify *all four* prerequisites that an actual capped session needs, not just that `systemd-run` exits 0:

1. **Env sanity.** `XDG_RUNTIME_DIR` is set and points to a directory that contains `systemd/`. Catches the most common WSL case without spawning a process.
2. **Scope creation, asynchronously, with the full real-session property set.** *Important:* `systemd-run --user --scope -- <cmd>` is **synchronous** — it does not return until `<cmd>` exits. If we run it as a one-shot and inspect the cgroup afterward, the scope is already gone and `cgroup.procs` reads as ENOENT. The probe *must* keep the scope alive concurrently with the inspection. Equally important: **the probe must pass exactly the same `-p` property set that real sessions use.** If preflight uses a weaker set, it can report "caps available" while a property the real wrapper relies on (most importantly `MemorySwapMax=0`) silently fails to apply on the production session — and the failure surfaces only as "OOM behavior diverges from what was promised", with no diagnostic. Concrete shape:
   - Spawn a child process (`Command::spawn`, not `output()` / `status()`) running: `systemd-run --user --scope --quiet --unit=cm-preflight-<random> -p MemoryHigh=64M -p MemoryMax=128M -p MemorySwapMax=0 -- /bin/sleep 30` — same `-p` set as the real wrapper at the top of this design (see the spawn-line block in *Why systemd-run instead of raw cgroup writes*), only with smaller memory numbers. If that block ever changes (e.g. add `IOAccounting=yes`), this command must change in lockstep.
   - The TUI now holds the child handle; the `systemd-run` process is still alive, the scope is active, and `sleep` is the live process inside it.
   - Wait up to ~500 ms for the unit to become active (poll `systemctl --user is-active cm-preflight-<random>.scope` or just retry step 3 a few times — `cgroup.procs` becomes readable as soon as `sleep` is in it).
3. **Cgroup-path resolution AND property-readback (while the probe is alive).** Two reads, both required:
   - **Path.** Read the *predicted* path `/sys/fs/cgroup/user.slice/user-<UID>.slice/user@<UID>.service/app.slice/cm-preflight-<random>.scope/cgroup.procs` and confirm it exists and contains the `sleep` PID. This is the same read the real watcher does (Component 2), so a host whose layout differs (non-default `app.slice` placement, different delegation, cgroup-v1 fallback) is detected here, not at first real spawn.
   - **Property readback.** Read `memory.high`, `memory.max`, and `memory.swap.max` from the same cgroup directory. Confirm that each was *actually applied* by the kernel — i.e. `memory.high` equals the requested 64M (in bytes: `67108864`), `memory.max` the requested 128M, and `memory.swap.max` is `0`. A systemd or kernel that silently dropped any of these (e.g. swap accounting not compiled in, so `memory.swap.max` reads as `max` instead of `0`) fails preflight here. Reading the cgroup files directly is one syscall each and matches what the watcher will do at runtime. `systemctl --user show <unit> -p MemoryHigh -p MemoryMax -p MemorySwapMax` is an acceptable alternative if the cgroup-file read is awkward, but the cgroup-file path is preferred — it tests that the same surface the watcher reads is also working.
4. **Cleanup.** Stop the unit explicitly: `systemctl --user stop cm-preflight-<random>.scope` (sends SIGTERM to processes in the scope, then waits). Then `child.wait()` on the `systemd-run` handle to reap it. Belt-and-suspenders: if cleanup fails, also try `kill(child_pid, SIGTERM)` directly so we don't leak a wedged unit.

Cache the result (`MemoryCapAvailable | MemoryCapUnavailable { reason }`) in shared TUI state. Every subsequent `Session::new` consults the cached result synchronously — no per-spawn probing, no failure-on-first-real-session surprise.

If preflight fails, log one line to the activity feed (`memory cap disabled: <reason>`) and mark all sessions uncapped for the rest of the TUI run. The user can rerun the TUI after fixing their environment.

The remaining failure surface after preflight is intentionally narrow: genuinely transient races (e.g. systemd briefly busy, scope unit name collision with a previous run that crashed mid-cleanup). Those are handled by the documented degraded-mode entry in the failure-modes table — *not* by pretending an in-progress wrapped spawn can fall back to raw.

### 1. Cgroup wrapping at spawn time (`tui/src/session.rs`)

Add a new optional parameter `MemoryCap` to `Session::new`:

```rust
pub struct MemoryCap {
    pub soft_bytes: u64,    // MemoryHigh
    pub hard_bytes: u64,    // MemoryMax
    pub session_uid: String, // becomes part of the unit name
}

pub fn new(
    shell: &str,
    args: &[String],
    cols: u16,
    rows: u16,
    working_dir: Option<PathBuf>,
    env: HashMap<String, String>,
    memory_cap: Option<MemoryCap>,  // NEW
) -> anyhow::Result<Self>
```

When `memory_cap` is `Some`, the function rewrites `(shell, args)` into `("systemd-run", [<wrapper args>, "--", shell, args...])` before passing to `tty::Shell::new`. When `None`, behavior is identical to today — tests stay uncapped, cloud `gcloud ssh` sessions stay uncapped.

The function also stores `MemoryCap` + the resolved cgroup path on the returned `Session` so the watcher can find it later.

### 2. Resolving the cgroup path (`tui/src/session.rs`)

After `tty::new(...)` returns, we need the cgroup path so the watcher can read it. Two options:

- **Probe.** After spawn, poll `/sys/fs/cgroup/user.slice/user-$(id -u).slice/user@$(id -u).service/app.slice/cm-sess-<uid>.scope/cgroup.procs` until it exists or a 2-second timeout fires.
- **Ask systemd.** `systemctl --user show cm-sess-<uid>.scope -p ControlGroup` returns the path. One subprocess per session spawn, ~5ms — fine.

I'd start with the probe. It's one syscall in the happy path and has no external dependency.

### 3. Watcher thread (new file: `tui/src/session_watch.rs`)

One thread per capped session. Owns:

- The cgroup path.
- The protected-PID set (see *Identifying the agent* below). Initially empty, populated during the stabilization phase, then frozen.
- A handle for emitting notices (channel back to the app event loop).

#### Identifying the agent

The original draft assumed `systemd-run --scope` exec's into the shell directly so the cgroup root PID *is* the agent. That holds for raw `/bin/bash`, but **not** for Claude Code or Codex: both ship as Node-based launchers that fork the actual agent binary as a child. The launcher exits or stays as a thin parent; the heavy process is a non-root PID in the cgroup. A naive "kill the largest non-root PID" rule would cheerfully pick the real agent and SIGTERM it. Same risk for any wrapper-style launcher.

Three candidate mechanisms:

- **(a) Post-stabilization snapshot + follow-up child window.** Watcher takes a snapshot of `cgroup.procs` at `T+STABILIZE_MS` (recommend 750 ms) after spawn — every PID present joins the protected set. Then for an additional `T+STABILIZE_MS` to `T+FOLLOWUP_MS` (recommend 2000 ms total since spawn), watch for new PIDs joining the cgroup; if a new PID's `ppid` is already in the protected set, add it. After `T+FOLLOWUP_MS`, the protected set is frozen and any new PID is killable. Name-independent, handles wrapper-style launchers (the wrapper's lazily-forked real worker is caught by the follow-up window).
- **(b) comm/exe allowlist.** Read `/proc/<pid>/comm` and protect by name match (e.g. `node`). **Rejected.** A `node` allowlist makes every `npm`/`jest`/`ts-node`/`webpack`/`vite` invocation an agent ran via Bash unkillable — those are exactly the tools the design needs to be able to kill. Defeats the goal.
- **(c) Agent self-marks via MCP.** Add `mcp__claude-manager__protect_pid`. Agent calls it for each long-lived helper. Most flexible, but requires per-agent buy-in and has a bootstrap race.

**Recommendation: (a).** Snapshot + follow-up window only, no comm allowlist. Concretely:

```
protected = cgroup.procs at T+STABILIZE_MS
            ∪ { pid : pid joined cgroup during (T+STABILIZE_MS, T+FOLLOWUP_MS]
                       AND ppid(pid) ∈ protected_at_join_time }
```

After `T+FOLLOWUP_MS` the set is frozen.

`STABILIZE_MS = 750`, `FOLLOWUP_MS = 2000` (so the late-fork window is the 1.25 s after stabilization). Implementation: a one-shot inotify watch on `cgroup.procs` for the duration of the follow-up window; each new PID is checked against the current protected set's ppid map and either admitted or marked killable.

**Residual risk: a wrapper that lazily forks its real worker more than 2 s after launch.** That worker enters the cgroup unprotected. If we see this in practice (e.g. a future agent runtime with very slow warmup), the additive fix is option (c) MCP self-marking — the agent can call `protect_pid(pid)` from its startup code and bypass the window's timing assumptions entirely.

**Trade-off accepted: a tool call that fires within the first 2 s after spawn is protected by mistake** — the soft cap can't kill it. This is a narrow window in practice (the agent isn't running significant tools before the user has even seen the prompt). If it happens, the hard `MemoryMax` backstop catches it.

#### Watch loop

1. Block on `inotify` for `memory.events` modifications, with a 2 s polling fallback in case inotify misses an event.
2. On wakeup, read `memory.events` and check whether the `high` counter increased since last iteration.
3. If yes: read `cgroup.procs`, then for each PID read `/proc/<pid>/status` (`VmRSS`). Pick the highest-RSS PID *not* in the protected set.
4. **Bind the kill to the original process, not the PID number.** Naively sending SIGTERM/SIGKILL to a numeric PID after the 500 ms grace is a well-known Linux footgun: if the target exits during the grace window and the PID gets recycled by another process on the host, the watcher will SIGKILL an unrelated process *outside* the session — and the bug bites worst exactly during churn (which is when the watcher is most likely to fire). Two paths, in order of preference:
   - **Preferred (Linux ≥ 5.3): `pidfd`.** At PID-selection time, call `pidfd_open(pid, 0)` and keep the returned fd. Send SIGTERM via `pidfd_send_signal(pidfd, SIGTERM, NULL, 0)`; after the 500 ms grace, send SIGKILL the same way. The pidfd is bound to the original process: if it exited and its PID number was recycled, `pidfd_send_signal` returns `ESRCH` and the watcher logs a no-op ("target already exited") instead of attacking a stranger. Close the fd after the kill (or after `ESRCH`).
   - **Fallback (kernels without `pidfd_open`).** Capture `/proc/<pid>/stat` field 22 (`starttime`, in clock ticks since boot) at PID-selection time. Immediately before *each* signal call, re-read `/proc/<pid>/stat` and confirm: (1) the file still exists, (2) `starttime` matches what was captured, and (3) the PID is still listed in the session's `cgroup.procs`. If any check fails, abort the signal — the original target is gone. This is best-effort (there is a vanishingly small TOCTOU window between the readback and the `kill(2)` syscall), but combined with the cgroup-membership check it's the right tradeoff for this fallback path.

   On systems where `pidfd_open` exists but is gated by seccomp / a container policy, the fallback path also kicks in. The watcher detects this on first use (one `pidfd_open` per session at most) and caches the result.
5. Append a sanitized record to the kill log (Notice channel A) and emit a `MemoryKill { session_uid, pid, comm, rss_kb }` event on the channel.
6. If every PID in the cgroup is in the protected set and we're still over the soft cap: emit `MemoryKillFailed { reason: "all candidates are agent processes" }`. Don't kill anything. The hard `MemoryMax` will fire if pressure continues.

Thread exits when the cgroup goes away (last process exited).

### 4. Notice channels

The agent already gets the **primary signal in-band**: the killed child's SIGKILL exit and partial output flow into the agent's tool-call stdout via the normal pipe. From the model's perspective, "Bash command exited with signal 9" is what it sees and reasons about. Everything else in this section is supplementary *context* — explaining the *why* (memory cap, not a crash) — and is optional for correct agent behavior.

#### What we will not do: PTY stdin injection

A previous draft of this design proposed writing `[claude-manager] killed PID ... — exceeded 6 GiB cap` directly to the PTY master. Dropping that approach for three reasons:

1. **Wrong recipient.** Bytes written to the PTY master go to whatever foreground process is reading it — could be the agent, could be a `less` pager the agent invoked, could be a child shell. The notice can land in anything. There is no way to address "the agent" specifically through this channel.
2. **Indistinguishable from user input.** The agent (or pager, or shell) has no way to tell a manager-generated notice apart from the user typing the same string. That's a trust boundary violation by construction.
3. **Injection vector if interpolated naively.** The kill notice would include the offending command's argv. Without strict sanitization (control bytes, escape sequences, embedded newlines), a hostile or even just unlucky argv lets us inject arbitrary input into whatever's reading the PTY. We'd have to spec the sanitizer, audit it, and accept that it's still hitting the wrong recipient half the time.

Even with sanitization the recipient problem is unfixable, so this channel is out.

#### Out-of-band options for richer agent context

Three real options, in increasing order of complexity:

- **(A) File drop under `~/.cm/`.** Watcher appends a JSON line to `~/.cm/memory_kills/<session_uid>.jsonl`, mode `0600`, with the parent directory created on first use also at `0700`. **Path lives outside the repo on purpose**: a worktree path puts the file inside the user's source tree, where it can be picked up by an unsuspecting `git add -A` or by a fresh worktree whose `.gitignore` doesn't yet cover `.cm/`. Argv strings can contain tokens, paths, and other secret-shaped text; the source of truth must be host-local, not repo-local. (If a user wants per-worktree convenience, a symlink from `<worktree>/.cm/` to the host-side file is fine — but the canonical writer targets `~/.cm/`.)

  **Logged fields** (sanitized — defense in depth, since the file is read by an agent that pulls it into model context):
  - `ts` — ISO timestamp
  - `session_uid` — for cross-correlation with the session manifest
  - `pid` — the killed PID
  - `comm` — `/proc/<pid>/comm`, **passed through the sanitizer below**. A process can rename itself via `prctl(PR_SET_NAME)`, so even though the kernel caps the field at 15 bytes + NUL, the *contents* are fully attacker-controlled and may contain arbitrary bytes including control codes. Treat as untrusted.
  - `argc` — number of argv entries (count, not values)
  - `argv_sha256_prefix` — first 8 hex chars of SHA-256 over the NUL-joined argv. Lets the user reproduce the kill via local audit correlation without piping argv into the agent's context.
  - `rss_kb` — measured RSS at kill time
  - `soft_cap_bytes` / `hard_cap_bytes` — the configured limits

  **Not logged**: raw argv, env vars, cwd, parent process info. If a user needs raw argv for forensic debugging, they can correlate by `pid` + `ts` against `journalctl --user -u cm-sess-<uid>.scope` or the kernel audit log — neither of which the agent reads.

  Document in `CLAUDE.md` that on seeing `signal 9` from a tool, the agent should `cat ~/.cm/memory_kills/$CM_TUI_SESSION_ID.jsonl`. Caveat: relies on the agent remembering the convention, which CLAUDE.md is the right place to durably encode.

  **Required code change: export `CM_TUI_SESSION_ID` into the agent's process env at PTY spawn.** Today, `tui/src/mcp_config.rs:41-58` injects `CM_TUI_SESSION_ID` only into the MCP server child's env (because Claude Code doesn't propagate parent env to MCP children), *not* into the agent's own process env. That means the agent — and any Bash tool it inherits env from — sees nothing. The CLAUDE.md instruction above would resolve to `~/.cm/memory_kills/.jsonl`, which is wrong. Fix: every agent-spawning path must populate the `env: HashMap` parameter on `Session::new` with `CM_TUI_SESSION_ID = <session_uid>` so the value reaches the PTY child and its descendants. The plumbing already exists — `Session::new` accepts an `env` map (session.rs:50); today it's almost always passed as `Default::default()`. The set of sites that need this is the same ~13+ surface that needs the cap wrapper (see the Code-changes table) — both concerns are best handled by routing every agent spawn through a single `spawn_agent_session(...)` helper that owns the env population and the cap lookup together, so neither can be forgotten at a new call site.
- **(B) MCP tool the agent polls.** Add `mcp__claude-manager__list_recent_kills` to `mcp_server/server.py`. Agent calls it after a SIGKILL. Same data as (A), delivered through the existing MCP control plane. Slightly more plumbing but matches the surrounding architecture (the MCP server already exposes `list_sessions`, `read_session_output`, etc.). Caveat: still relies on the agent knowing to ask, which still means a CLAUDE.md convention.
- **(C) Hook-injected system message.** Claude Code's hook system can inject text into the model's next turn out-of-band, addressed to the model rather than the PTY. This is the only option that actually pushes context to the agent without polling. Costs: a hook config the user has to install, behavior tied to Claude Code specifically (Codex would still need (A) or (B)).

**Recommendation:** start with (A). It is free, agent-agnostic, and non-invasive. If experience shows agents miss the file too often, layer (C) on top for Claude Code. Skip (B) unless we end up wanting kill events to flow through the MCP control plane for other reasons (e.g. surfacing them in `read_session_output`).

#### User-facing channel

- **Activity feed** (`A-,`). Already plumbs agent-initiated mutations like `start_session` / `kill_session`. Add a `MemoryKill` variant. User sees a line built from sanitized fields only — no raw argv, no unsanitized `comm`: `[14:22:01] cm-sess-abc123 — killed PID 41892 comm=rg argc=4 sha=a1b2c3d4 — 5.8 GiB RSS, soft cap 6 GiB`. The `comm` value is run through the sanitizer below before insertion; the activity-feed renderer additionally re-escapes for its own rendering surface (defense in depth — never trust that an upstream did the work).

This is the *only* channel the design relies on for user awareness. It is strictly internal to the TUI process — no PTY writes, no shell injection surface.

#### Sanitizer (applied at every write site)

`comm` and any other byte string sourced from `/proc/<pid>/...` or argv is **untrusted**. `comm` in particular can be set by the process itself via `prctl(PR_SET_NAME)`, so a malicious or buggy program can put any byte sequence (including escape sequences targeting the user's terminal) into that field. argv is fully attacker-controlled.

Apply the same sanitizer at every place these strings are written — JSONL log, activity-feed event, anywhere else that ever surfaces them:

1. **Strip C0 control bytes.** Any byte `< 0x20` or `== 0x7f` is replaced with `?`. No exceptions: not tab, not newline, not CR. Embedded newlines in particular would let an attacker forge log lines or activity-feed entries.
2. **Strip non-UTF-8.** Any byte sequence that doesn't decode as valid UTF-8 is replaced byte-by-byte with `?`. (Don't try to round-trip raw bytes through the agent's context.)
3. **Strip hostile valid-UTF-8 codepoints.** Stripping C0+DEL on raw bytes leaves several classes of valid-UTF-8 codepoints that survive step 2 and remain dangerous in both the agent-facing JSONL and the activity feed. Drop or escape (replace with `?` or `\uXXXX`) every scalar value in:
   - **C1 controls**: `U+0080`–`U+009F`. Some terminals interpret these as 8-bit equivalents of the ESC-prefixed C0 escape sequences.
   - **Bidi overrides**: `U+202A`–`U+202E` (LRE, RLE, PDF, LRO, RLO), `U+2066`–`U+2069` (LRI, RLI, FSI, PDI). The Trojan Source class (Boucher & Anandakumar, 2021) — these reorder visual text without changing the underlying bytes, so a hostile `comm` could make the activity-feed line *display* a different command name than the one the kill log records.
   - **Zero-width / formatting**: `U+200B` (ZWSP), `U+200C` (ZWNJ), `U+200D` (ZWJ), `U+FEFF` (BOM/ZWNBSP). Enables visual spoofing ("rg" vs "r​g") between the activity feed and what the agent reads from the JSONL.
   - **Line/paragraph separators**: `U+2028` (LS), `U+2029` (PS). Some parsers treat these as line breaks even when JSON does not — same log-forgery hazard as embedded `\n`.

   This is one allowlist-by-rejection check after UTF-8 decoding, not a per-class regex pass: filter on scalar value during the same iter that's already validating UTF-8.
4. **Cap length.** `comm` is hard-capped at 16 chars after sanitization (kernel limit anyway). Any other surfaced string is capped at 64 chars; truncations are marked with a trailing `…`.
5. **Render-time re-escape.** The activity-feed renderer treats the sanitized string as plain text and escapes any character its rendering layer treats specially (ratatui style markup, terminal escape codes if the renderer ever emits them). Belt-and-suspenders: the writer already stripped control bytes and hostile codepoints; this catches anything the sanitizer missed.

JSONL writes: serde's default JSON encoder already escapes control bytes inside string values, so the sanitizer is technically redundant for the on-disk file *if* an unsanitized string never reaches a non-JSON surface. We sanitize at the source anyway because the same fields are emitted into the activity-feed event channel, which is *not* JSON-encoded.

## Configuration

**Caps are off by default for every session type.** A cap turns on only when *both* `CM_SESSION_MEM_SOFT_<TYPE>` and `CM_SESSION_MEM_HARD_<TYPE>` are set to non-empty values. Unset, empty, or partially-set (only soft without hard, or vice versa) → uncapped, no wrapping, no watcher. There are no compiled-in default values.

To opt in, the user sets the relevant env vars in `~/.config/claude-manager/.env`. Example values that *enable* the cap:

```
# Example — these enable the cap. Without them, no cap is applied.
CM_SESSION_MEM_SOFT_CLAUDE=6G
CM_SESSION_MEM_HARD_CLAUDE=10G
CM_SESSION_MEM_SOFT_CODEX=6G
CM_SESSION_MEM_HARD_CODEX=10G
# CM_SESSION_MEM_SOFT_BASH / _HARD_BASH unset → bash sessions uncapped
```

The numbers above are illustrative, not defaults. The TUI ships with no opinion about what the threshold should be; the user picks values that match their machine's RAM and tolerance for false-positive kills. (See open question 1 — we may want to instrument first to recommend a starting value.)

`gcloud` and other "infra" sessions stay uncapped regardless of env vars (the cap is about protecting the local machine from agent-spawned commands, not about ssh shells).

Even when env vars are set, the cap is also gated on the **preflight result** (Component 0). If preflight failed, all sessions run uncapped for the lifetime of the TUI run, and the user sees a startup activity-feed line explaining why.

## Code changes (concrete)

| File | Change |
| --- | --- |
| `tui/src/config.rs` | Read four env vars per session-type; expose `Config::memory_cap_for(session_type)` returning `Some((soft, hard))` only when *both* are set, else `None`. |
| `tui/src/main.rs` (or `app.rs` startup) | Run the preflight probe once at TUI startup, store the result in shared state. Emit one activity-feed line on failure. |
| `tui/src/session.rs` | New `MemoryCap` struct. New `memory_cap: Option<MemoryCap>` parameter on `Session::new`. Rewrite `(shell, args)` when `Some` *and* preflight succeeded. When `None` or preflight failed, behavior is identical to today. |
| `tui/src/session.rs` | Store the resolved cgroup path on `Session`. Spawn `session_watch::watch(...)` thread when capped. |
| `tui/src/session_watch.rs` (new) | Watcher loop: inotify on `memory.events`, post-stabilization snapshot + follow-up child window for protected-PID set (no comm allowlist), RSS scan, SIGTERM/SIGKILL, channel emit. Sanitizer for `comm` strings (control-byte strip, UTF-8 strip, length cap). Append a sanitized JSON line to `~/.cm/memory_kills/<session_uid>.jsonl` (mode `0600`, fields per Notice channel A — `comm`/`argc`/`argv_sha256_prefix`, *not* raw argv). ~250 LOC. |
| `tui/src/session.rs` (recommended structural change) | Add a `spawn_agent_session(session_type, session_uid, ...)` helper that wraps `Session::new`, owns the cap lookup, owns the `CM_TUI_SESSION_ID` env-population, and is the *single* function every agent-spawning call site goes through. Without this, the cap+env wrapping has to be repeated at every call site, and future call sites silently drop both. With this, every agent-spawning path becomes a one-liner that can't forget either piece. Strongly recommended over the per-call-site change below. |
| **All agent-spawning call sites** | Route through `spawn_agent_session(...)` instead of calling `Session::new` directly. The set is broader than the original draft listed — this repo currently has agent-spawning `Session::new` calls at (at minimum) `tui/src/app.rs:676` (replace-in-place after fresh-context transition), `app.rs:2317` and `app.rs:2347` (restore path: with-MCP and fallback-without-MCP), `app.rs:3553` (MCP `start_session` tool handler), `app.rs:4928` (workflow new-slot fill), `app.rs:5034` (`A-n` / `A-s` new-session path), `app.rs:5130`, `app.rs:5143`, `app.rs:5204`, `app.rs:5625` (planning `A-l` linear-mode launch), `app.rs:5760`, `tui/src/planning.rs:2200`, `tui/src/workflow/controller.rs:493` (workflow participant spawn). **Implementer note:** don't trust this list to be complete or stable — `grep -n 'Session::new' tui/src/**/*.rs`, filter out `/bin/true` test calls and `gcloud`/`/bin/bash` non-agent paths (`session_type` check is the source of truth), and route every remaining hit through the helper. Sites where `session_type ∈ {"claude", "codex"}` (or, going forward, anything else `engine_for_session_type` maps to `claude-code` or `codex`) get the cap; bash/gcloud/infra sessions stay raw. Test sites that pass `/bin/true` keep calling `Session::new` directly with no cap. |
| `tui/src/app.rs` | New event variant `SessionMemoryKill` flowing into the activity feed renderer. **No PTY write.** Renderer re-escapes the `comm` string before display. |
| `CLAUDE.md` | Add a short paragraph telling agents that on a `signal 9` from a Bash tool call, they should read `~/.cm/memory_kills/$CM_TUI_SESSION_ID.jsonl` for context. |

**Estimate.** Roughly ~500 LOC across all changes: ~250 in the new `session_watch.rs`, ~80 for preflight, ~50 for the `spawn_agent_session(...)` helper + `MemoryCap` struct, ~30 for `Config::memory_cap_for`, plus the call-site rewrites (which collapse to one-liners once the helper exists). One new Rust file, one new section in `CLAUDE.md`, ~13 `Session::new` call sites rerouted through the helper.

## Testing

The cap mechanism touches the kernel, so full coverage requires a real Linux + systemd host. Split the work:

- **Unit tests (no cgroup required, run in CI like every other test).**
  - *Sanitizer:* one test per hostile codepoint class — C0 controls, DEL, invalid UTF-8 byte sequences, C1 controls, every bidi override, every zero-width / formatting char, `U+2028`/`U+2029`. Plus a positive test that ASCII passes through untouched and length-cap truncates at the right boundary with `…`.
  - *Protected-set computation:* feed a synthetic timeline of `cgroup.procs` reads (snapshot at `T+750ms`, additions during `(T+750ms, T+2000ms]` with controlled `ppid` values, additions after `T+2000ms`) and assert the final set matches expected. Cover the wrapper-style case (snapshot is the wrapper PID, real worker arrives at `T+1200ms` with `ppid=wrapper`) explicitly.
  - *Config parsing:* every combination of soft/hard set/unset/empty/invalid-suffix produces the right `Option<(soft, hard)>`.
  - *PID-reuse fallback:* mock `/proc/<pid>/stat` reads — verify the watcher aborts the signal when `starttime` differs from what was captured at selection.

- **End-to-end smoke (manual or scripted, on a real Linux + systemd host).**
  - *Soft-cap kill:* launch a real `claude` session with `CM_SESSION_MEM_SOFT_CLAUDE=512M`, `CM_SESSION_MEM_HARD_CLAUDE=1G`. Have the agent run `stress-ng --vm 1 --vm-bytes 800M`. Assert: stress-ng is SIGKILLed, activity-feed line appears with sanitized `comm=stress-ng`, a JSONL record lands in `~/.cm/memory_kills/<session_uid>.jsonl`, the agent itself stays alive.
  - *Hard-cap fallback:* same setup but `--vm-bytes 1500M`. The soft watcher can't keep up; assert the kernel `MemoryMax` fires and the activity-feed line records `MemoryKillFailed` followed by the kernel kill.
  - *Preflight failure:* temporarily unset `XDG_RUNTIME_DIR`, restart the TUI, confirm "memory cap disabled" appears once in the activity feed and subsequent sessions spawn raw.
  - *Helper coverage:* `grep -n 'Session::new' tui/src/**/*.rs | grep -v /bin/true | grep -v gcloud | grep -v /bin/bash` returns only sites inside `spawn_agent_session(...)` itself. Treat any other hit as a bug.

- **Out of scope for automated testing.** PID recycling under the pidfd path (requires artificial PID-space pressure that doesn't fit in CI) and the prctl-renamed `comm` sanitization with hostile codepoints (awkward to set up without a custom binary). Both are covered by code review against the sanitizer spec and the kernel guarantees pidfd provides.

## Failure modes & graceful degradation

| Scenario | Behavior |
| --- | --- |
| `systemd-run` not on PATH | Preflight catches it. Activity-feed line at startup. Cap disabled for the TUI run. Sessions spawn raw. |
| User systemd not running (e.g. older WSL), no `XDG_RUNTIME_DIR`, cgroup-v2 not mounted | Preflight catches it (the probe `systemd-run --user --scope -- /bin/true` exits non-zero). Activity-feed line at startup with the captured stderr. Cap disabled for the TUI run. No per-session retry surprise. |
| Preflight succeeded at startup but a single later spawn's cgroup-path probe times out at 2 s | **Degraded mode: scope active, watcher disabled.** Once `Session::new` has invoked `tty::new(...)` with `systemd-run --user --scope` as the program, the PTY is wrapped — there's no in-process way to "unwrap" mid-spawn into a raw uncapped session, and no such fallback is defined. The kernel-enforced `MemoryHigh`/`MemoryMax` properties on the scope unit are still in effect (the scope was created successfully, only the *path-resolution read* failed), so the host is still protected from runaway allocation. What's lost is the userspace soft-kill of children: when `MemoryHigh` is breached there's no watcher to pick which PID dies, and the kernel will eventually fire `MemoryMax` if pressure continues. Logged to activity feed as `memory cap degraded: watcher disabled (<reason>)`. Subsequent sessions still try the full cap+watcher path. |
| Watcher thread panics | Trapped, logged to activity feed. Agent keeps running uncapped (better than killing the session). |
| Agent itself is the largest process | Watcher sees only one PID in cgroup, refuses to kill, emits `MemoryKillFailed`. Hard `MemoryMax` eventually fires and kernel kills the agent — same failure mode as today, but only when the agent is the genuine offender. |
| `cgroup.procs` is empty by the time watcher reads it | The runaway already exited. No-op. |
| RSS read race (PID gone before /proc/PID/status read) | Skip and continue to the next PID. |

## Daemon-side (headless) capping — the system-service trap

Everything above assumes the spawner is the **TUI**, which runs inside the operator's login session and therefore inherits a working user systemd manager (`XDG_RUNTIME_DIR`, a live `user@<UID>.service`, the private socket). `cm-daemon` does **not** get that for free.

On `cm-manager`, `cm-daemon.service` is a **system** unit (`/etc/systemd/system/cm-daemon.service`, `WantedBy=multi-user.target`) running as `User=lucas`. A system service started as another user gets **no user-session environment**: no `XDG_RUNTIME_DIR`, no `DBUS_SESSION_BUS_ADDRESS`, and (absent linger) no running `user@<UID>.service` at all. So the daemon's `systemd-run --user --scope` can't reach the user manager bus — it exits non-zero *before* creating the scope, the child stays in the daemon's own `system.slice/cm-daemon.service` cgroup, and `start_session`'s cgroup verification rejects + SIGKILLs it. The whole continuous fire fails.

The trap is worse than a clean "no user manager": the predicted `app.slice` path can **exist** while the daemon still can't reach the bus. `user@<UID>.service` (and its `app.slice`) come up whenever `lucas` has *any* live login session (e.g. an operator SSH), then vanish when that session ends. So the old `app.slice`-`is_dir()` gate the cap resolvers used passed intermittently — the cap was applied, then `systemd-run --user` failed at spawn. Two identical fires at 21:50 and 21:52 UTC on 2026-07-06 failed this way while `systemd-run --user --scope true` run *interactively* as `lucas` completed in 26 ms.

### Fix 1 (code): authoritative probe + degrade-to-uncapped

`is_dir()` is necessary but not sufficient. `mcp_config::user_scope_capable()` runs the **real** operation once per process — `systemd-run --user --scope -p MemoryMax=… -- /bin/true`, capturing systemd-run's stderr/exit — and caches the result. Both cap resolvers (`resolve_continuous_cap`, `resolve_configured_participant_cap`) consult it after the `is_dir()` fast-path and, when it fails, run the spawn **UNCAPPED** with the captured reason logged (never a hard fire failure). This surfaces the true cause (`Failed to connect to bus…`) that the downstream cgroup-discovery timeout otherwise hid, and self-heals across hosts: the cap applies iff it actually works. The daemon's `[scheduler] default_cap` also defaults to `0` (uncapped) — a non-zero default only trapped new tasks; caps are opt-in per task.

### Fix 2 (ops): make the capped path actually work on `cm-manager`

To let an opt-in `mem_cap_bytes` task boot inside a genuine `cm-sess-*.scope`, give the daemon a reachable user manager. Two steps (uid is host-specific — `lucas` is `1001` on cm-manager; the drop-in uses the `%U` specifier so it's uid-agnostic):

1. **Persist the user manager** so `/run/user/<UID>` and the private socket survive with no login session:
   ```bash
   sudo loginctl enable-linger lucas
   ```
2. **Point the daemon at it** via a unit drop-in (checked in at `deploy/cm-daemon.service.d/user-scope-cap.conf`):
   ```ini
   [Service]
   Environment=XDG_RUNTIME_DIR=/run/user/%U
   ```
   Install + reload + restart:
   ```bash
   sudo mkdir -p /etc/systemd/system/cm-daemon.service.d
   sudo cp deploy/cm-daemon.service.d/user-scope-cap.conf /etc/systemd/system/cm-daemon.service.d/
   sudo systemctl daemon-reload && sudo systemctl restart cm-daemon
   ```

`systemd-run --user` talks to the manager over `$XDG_RUNTIME_DIR/systemd/private`, so `XDG_RUNTIME_DIR` + linger suffice; `DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/%U/bus` is only needed if a future path uses the session bus directly.

**Verify** (after deploy): create a throwaway continuous task with an explicit `mem_cap_bytes` (or set `[scheduler] default_cap`), `continuous.run_now`, then confirm the spawned session's `/proc/<pid>/cgroup` basename matches `cm-sess-*.scope` and `journalctl -u cm-daemon` shows no degrade line. Before the ops fix (or on any host without a user manager), the same task boots **uncapped** with a single `systemd-run --user … not usable: <reason> — running UNCAPPED` log line — the intended graceful degrade.

## Open questions

1. **Soft threshold default.** 6 GiB is a guess based on "Claude Code itself sits at ~1–2 GiB during normal use, plus some headroom for tool output." The right default is whatever value (a) you've never legitimately needed to exceed, and (b) is well below total RAM minus other workloads. Want me to instrument current sessions for a few days first to pick this from data instead of guessing?
[[6 and 10 are good for soft and max]]
2. **Should `bash` sessions be capped?** Default off feels right (you might intentionally run a build that needs lots of RAM in one), but a flag would be cheap.
[[default off]]
3. **Workflow participants.** A workflow has multiple sibling sessions on one task. Should each get its own cap, or should they share one cgroup so the *workflow* total is bounded? Independent caps are simpler; shared would need a parent cgroup and is significantly more code. I'd start independent.
[[each get their own cap]]
4. **Phase 7 permission convention.** This system kills processes — that's destructive. But the kill is a *response* to a configured cap, not an agent-initiated action, so I don't think the "ask before killing" convention applies (it's a safety mechanism, not an agent's tool call). Worth confirming before building.
[[Don't ask before killing]]
