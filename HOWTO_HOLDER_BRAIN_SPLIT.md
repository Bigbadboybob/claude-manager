# HOWTO: operating the holder/brain split

The operator's guide to the daemon architecture that has run **both hosts since
2026-08-24** (local since 08-20). Design rationale lives in
`DESIGN_HOLDER_BRAIN_SPLIT.md`; this document is what you do day to day, what
to look at, and what to do when something is wrong. Every command below is
runnable as written (`scripts/cm-op` is the operator-RPC helper).

## 1. What is running

```
cm-holder  (pid stays forever; supervisor; ~1.5k LOC; near-frozen)
 ├─ cm-daemon  "the brain" — ALL the logic (RPCs, MCP, workflows, persistence)
 ├─ claude … (session child)      ← children of the HOLDER, not the brain
 ├─ codex …  (session child)
 └─ bash …   (session child)
```

- The **holder** owns every session child, its PTY master, and its pidfd.
  It forks/execs sessions on the brain's behalf, reaps them, and hands the
  brain fd *dups* over an inherited socketpair (never a named socket).
- The **brain** is the holder's child. It crashes, deploys, and restarts
  freely; sessions never notice. On every brain start it re-adopts all
  sessions from the holder and keeps serving on the same control socket
  (the listener is *custodied* by the holder across brain generations).
- The holder persists nothing; all durable state stays brain-side
  (`~/.cm/daemon-sessions.json`, tombstones, etc.).

| | local | cm-manager |
|---|---|---|
| Supervisor | the TUI (auto-launch) | systemd `cm-daemon.service` |
| Launch topology switch (C6) | `~/.cm/holder-binary` (one line: the cm-holder path). Present = split; **fails closed** on empty/unreadable | `ExecStart=/opt/cm-daemon/cm-holder --brain /opt/cm-daemon/cm-daemon` |
| Binaries | `~/.cm/shared-target/release/{cm-daemon,cm-holder}` (the shared cargo target — see §7) | `/opt/cm-daemon/{cm-daemon,cm-holder}` (root-owned; previous monolith kept as `cm-daemon.pre-split-a4376f1`) |
| Operator token | `~/.cm/operator-token` (TUI-minted) | `~/.cm/operator-token` + `operator-token.env` drop-in on the unit |
| Holder/brain logs | stderr → `/dev/null` (TUI spawn) — **known gap**, see §8; brain persistence errors surface in `daemon.health` | `journalctl -u cm-daemon` (holder AND brain lines) |
| Config | `~/.cm/daemon.toml` (none locally = defaults) | `/home/lucas/.cm/daemon.toml` |

## 2. The health surface (what to look at)

```
scripts/cm-op daemon.health                # local
scripts/cm-op --ssh cm-manager daemon.health
```

Key fields in split mode:

| field | meaning | healthy |
|---|---|---|
| `split` | running the split | `true` |
| `holder_epoch` | brain-generation counter (1 = first brain since the holder started) | +1 per deploy, **stable otherwise** |
| `breaker_state` | holder's supervision state | `running` (`held_down` = no workable brain pin) |
| `brain_restarts` | generations beyond the first (deploys count) | matches your deploy count |
| `brain_pid` | the brain (child of the holder) | changes only on deploy/crash |
| `holder_build_id` | the holder image (crate version) | changes only on `upgrade_holder` |
| `build_id` | the brain's compile-time identity — **unreliable across worktrees sharing a cargo target** (§7); trust the counters | |
| `sessions` / `holder_sessions` | brain registry vs holder-held count | equal |
| `holder_pending_exit_events` | exits the brain hasn't durably acked yet | 0 at rest |
| `holder_status_error` | the holder didn't answer `status` | absent |
| `mcp_ok` | MCP preflight at brain boot | `true` |

**The all-clear after any deploy:** `holder_epoch` went up by exactly one,
sessions unchanged, `breaker_state == "running"` and `holder_epoch` still the
same 10 minutes later (the breaker's stability horizon — a slow crash loop
inside that window auto-rolls back to the previous pin and bumps the epoch
again, which is how you'd notice).

## 3. Routine deploys (brain code — the weekly case)

Sessions are never signaled. Attach streams blip and auto-reattach.

**Local** — unchanged habit:
```
scripts/cm-redeploy --yes
```
Builds the workspace release, calls `daemon.restart`; in split mode that arms
`restart_brain` (brain quiesces → checked persist → exits → holder execs the
pinned new binary). The script verifies `holder_epoch` +1 exactly, soaks 90s,
and prints the 10-minute note.

**cm-manager** — seamless (do NOT use `cm-redeploy --manager`: that is the
legacy stop/cp/start path and kills every session there):
```
cargo build --release -p cm-daemon
scp ~/.cm/shared-target/release/cm-daemon cm-manager:/tmp/cm-daemon-new
ssh cm-manager 'sudo install -m0755 -o root -g root /tmp/cm-daemon-new /opt/cm-daemon/cm-daemon \
                && /opt/cm-daemon/cm-daemon --daemon-preflight'
scripts/cm-op --ssh cm-manager daemon.restart '{"binary_path": "/opt/cm-daemon/cm-daemon"}'
# connection dies at the brain exit = success shape; then:
scripts/cm-op --ssh cm-manager daemon.health     # holder_epoch +1, sessions same
```
`install`, not `cp`: the running brain holds the old inode open (`cp` fails
with "Text file busy"; `install` creates a new inode). The holder pins the
binary by fd at exec time, so an in-flight `cp` can never half-apply.

**MCP server / workflows on cm-manager:** scp as before, then the same
`daemon.restart` — the brain restart re-runs the MCP preflight; no
`systemctl restart` needed anymore.

**Config value** (`mcp_server_path`, `api_*`, `notify_command`, …):
`scripts/cm-op daemon.reload_config` or `kill -HUP <brain_pid>` (the holder
ignores HUP).

## 4. When a deploy goes wrong

| Symptom | What happens | You do |
|---|---|---|
| New brain crash-loops | holder breaker trips after 3 consecutive failures → execs the **previous pin** (`holder_epoch` bumps again); no previous pin → `breaker_state: held_down`, retries the on-disk path every 60s | fix the binary on disk (self-heals on the next retry) or `kill -USR2 <holder_pid>` to retry now. Sessions untouched throughout |
| New brain boots but is wrong | nothing automatic | `scripts/cm-op daemon.rollback_brain` — execs the previous pin |
| Brain hangs (RPCs time out, `daemon.health` hangs) but pongs flow | the watchdog can't see it (S9) | `kill -9 <brain_pid>` (from `ps`; it's the holder's `cm-daemon` child) — the holder counts a strike and respawns; sessions untouched |
| Brain stops answering pings (3 misses, ~90s) or stops draining the channel | holder SIGKILLs + respawns (a strike) | nothing; watch `holder_epoch` |
| Holder itself dies | **everything dies** (the brain via PDEATHSIG, sessions via PTY teardown) — the one crash class the split does not survive | the supervisor relaunches the split (flip file / ExecStart); sessions resume from their spawn-pinned stamps via startup-restore |

Status dump on demand: `kill -USR1 <holder_pid>` prints epoch / held sessions /
pending events to the holder's stderr (journal on cm-manager).

## 5. Rare operations

**Upgrade the holder binary** (never in the weekly path):
```
scp …/cm-holder <host>:/tmp/cm-holder-new && sudo install … /opt/cm-daemon/cm-holder   # new inode
scripts/cm-op [--ssh cm-manager] daemon.upgrade_holder '{"holder_path": "/opt/cm-daemon/cm-holder"}'
```
The holder re-execs itself with a sealed state manifest; the brain stays up
and answers the new image's `rehello`. Verify: `holder_epoch` and `sessions`
unchanged. `holder_build_id` is the **crate version** — bump
`holder/Cargo.toml` when shipping holder changes or the id won't move (the
journal's `HOLDER UPGRADE` + `post-upgrade rehello answered` lines are the
other proof). Gated (S15): refused if the new holder's proto range doesn't
cover the running one's.

**Leave the split** (reverse migration): flip the supervisor FIRST, then
roll back — sessions ride back into a single-process daemon at the same PID.
```
rm ~/.cm/holder-binary                                          # local
# cm-manager: set ExecStart back to /opt/cm-daemon/cm-daemon; systemctl daemon-reload (NO restart)
scripts/cm-op [--ssh cm-manager] daemon.split_rollback '{"monolith_path": "/opt/cm-daemon/cm-daemon"}'
```
Refused `not_drained` if an exit is mid-pipeline — retry in a moment.

**Enter the split** (a fresh host, or after a reverse migration): stage +
preflight both binaries, flip the supervisor (write the flip file /
`ExecStart` + `daemon-reload`, no restart), then
`daemon.migrate_split {holder_path, brain_path}`. Full ordered runbook with
the crash-safety reasoning: `DESIGN_HOLDER_BRAIN_SPLIT.md` § Rollout runbook.

**Stop everything**: `systemctl stop cm-daemon` / SIGTERM to the holder.
Sessions are killed *deliberately* (on cm-manager systemd's default
`KillMode=control-group` signals the whole tree itself; same outcome) and
resume later from pinned stamps.

**Hard restart** (the `--hard` row — holder/manifest machinery changes or a
manifest-schema bump): `scripts/cm-redeploy --hard` locally; on cm-manager
`systemctl restart cm-daemon`. Sessions die and startup-restore them.

## 6. Reboots

Sessions die at a reboot (unchanged). The supervisor relaunches the **split**:
locally the TUI reads `~/.cm/holder-binary`; on cm-manager the unit's
`ExecStart` names the holder. Both are the "C6 flip" — the split's durability
is exactly the durability of that flip *plus the binary honoring it* (§7).

## 7. The shared-target rule (how we lost the split once)

All worktrees build into `~/.cm/shared-target`, and the served local binaries
ARE that directory's `release/` output. On 2026-08-22 a reboot relaunched a
**monolith** despite an intact flip file: release builds from main-based
worktrees had overwritten `cm-daemon` and the TUI with pre-merge images that
had no split code — the TUI didn't know the flip file existed. Resolved by
merging the split to main (`5190083`) so every worktree build carries it.

Rules that follow:
- **Main must stay split-capable.** A branch that changes the flip/launch
  semantics is a `--hard`-class change; build it with a private
  `CARGO_TARGET_DIR` until it's on main.
- `build_id` lies across worktrees sharing a target (the build script's git
  hash is cached) — verify deploys on `reexec_generation` / `holder_epoch`.
- After any TUI rebuild, restart the TUI (`A-q`, relaunch) so the running
  image is the flip-aware one; the old image relaunching a monolith over
  split state is the exact hazard above.
- Better still (open item): serve from a dedicated `~/.cm/bin/` that
  `cm-redeploy` installs into, instead of the build output directory.

## 8. Known gaps / residuals

- **Local holder logs go to /dev/null** (the TUI spawns the daemon with stdio
  detached). Breaker trips, HELD_DOWN, and migration lines are invisible
  locally except through `daemon.health`. Fix: have the TUI redirect daemon
  stdio to `~/.cm/daemon-stdio.log`.
- `cm-redeploy --manager` is still the legacy kill path — use §3's seamless
  sequence for cm-manager until it's rewritten.
- Brain **crash** (not deploy) residuals, by design: ≤ 8 KiB of display bytes
  per session can be lost from the fanout ring (transcripts unaffected); a
  prompt mid-`write_all` can land half-typed; a memory-cap kill during the
  brain gap may tombstone as a plain exit.
- `daemon.restart`/`migrate_split` refuse on `[tls]`-configured daemons
  (listener handoff unimplemented; neither host uses TLS).
- Skew matrix baseline is pinned at `924ff6a`; advancing it is a deprecation
  event recorded in the design doc.

## 9. Tests

```
cargo test -p cm-holder -p cm-holder-proto                     # holder loop + protocol + manifests
cargo test -p cm-daemon --test holder_mode_e2e                 # crash/deploy/held-down/stop e2e (7)
cargo test -p cm-daemon --test holder_split_migration_e2e      # migrate / failure-rollback / reverse / upgrade (4)
scripts/holder-skew-matrix                                     # {holder,brain} × {baseline,HEAD} + the V9 reverse cell
```
All e2es run real binaries in a throwaway HOME/socket sandbox — never the
real daemon. Run the matrix after ANY holder or protocol change.

## 10. Rollout history

| date | event |
|---|---|
| 2026-08-18 | seamless re-exec (`daemon.restart`) deployed locally — the prerequisite |
| 2026-08-19 | phases 1–6 built; phase-7 code + codex review round (6 blockers + 9 should-fix, all folded) |
| 2026-08-20 | local migrated live at the daemon's original PID (30/30 sessions); first `restart_brain` deploy verified (epoch 1→2) |
| 2026-08-22 | machine reboot relaunched a monolith (§7) — undetected until 08-24 |
| 2026-08-24 | merged to main `5190083`; local re-deployed + re-migrated (27/27); **cm-manager migrated** at its systemd MainPID (42/42); TUI restarted on the flip-aware build |
