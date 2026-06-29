---
name: bug-hunt
description: Multi-tier adversarial bug hunt over a scoped diff or subsystem — deterministic scans, triage, diverse-lens finder fanout, refute-by-default verification, a user gate, then file-disjoint fix waves. Tuned for this repo's Rust TUI + daemon (primary) and Python api/dispatch/cli/mcp_server (secondary). Triggers when the user asks for a "bug hunt", a "sweep for bugs", pre-merge hardening of a branch, or runs /bug-hunt [target].
---

# Bug Hunt

A structured hunt that escalates from cheap deterministic scans to multi-agent fanout, with adversarial verification before any fix effort. Tuned for **claude-manager**: a Rust workspace (`tui/`, `daemon/`) that is the primary surface, plus secondary Python (`api/`, `dispatch/`, `cli/`, `mcp_server/`). This skill instructs multi-agent orchestration (Workflow/Agent fanout) — that is its core mechanism, not an optional add-on. If you instead run the hunt as cm sessions/workflows via the claude-manager MCP, the Phase-7 agent convention applies (state intent + get user confirm before any spawn/kill/send).

## Phase 0 — Target & staleness check (NEVER skip)

Hunting stale code finds already-fixed bugs and produces revert-risk fixes. Before any finder runs:

1. Resolve the hunt surface with the user if not given: usually (a) the diff of the current branch vs `main` (this repo works in worktrees off short-lived `cm/*` branches — `git merge-base HEAD main`), (b) a feature branch pre-merge, or (c) a named subsystem (e.g. `daemon/src/control/`, the attach/reattach path, the workflow controller).
2. Staleness check: `git merge-base HEAD main` + `git log --oneline HEAD..main -- <surface paths>`. If main has commits touching the surface that the target branch lacks, STOP and surface it — get explicit confirmation before proceeding on a stale target. (Worktrees drift from `main` silently; this is the common case here, not the rare one.)
3. Record the baseline commit and the changed-file list: `git diff --name-only <baseline>..HEAD -- '*.rs' '*.py'`. Keep the in-file `#[cfg(test)] mod tests` / `tests/*.rs` readable as context, but the surface to harden is the non-test code. This list scopes every later phase.
4. Skim `git log --oneline <baseline>..HEAD` and group the surface into subsystems — these become verifier groupings and fix-wave boundaries. Natural seams here: daemon control/dispatch/auth, session/PTY lifecycle, manifest + manifest.watch, workflow controller/transitions, attach + remote reattach/tunnels, TUI render/draw, planning/api client.

## Tier 0 — Deterministic scans (cheap signal first)

Run over the surface only. Collect output into one scratch file for triage; don't fix anything yet.

- **Rust (primary):**
  - `cargo clippy --workspace --all-targets -- -W clippy::all` — then read only the warnings whose `file:line` falls in the surface. Pay attention to `clippy::await_holding_lock`, `clippy::let_underscore_future` (dropped futures), `clippy::unwrap_used`/`expect_used` on paths that handle untrusted input or I/O.
  - `cargo build --workspace` must be clean; a surface that doesn't compile isn't huntable. `cargo fmt --check` to spot half-applied edits.
  - `cargo test --workspace` (or `cargo test -p claude-manager-tui` / `-p cm-daemon`) as a baseline — note pre-existing failures so a fix wave doesn't get blamed for them.
- **Python (secondary — cloud path):** no ruff/mypy is wired in this repo. Lean on greps + reading. If the surface is Python-heavy and the user wants it, install ad-hoc (`uvx ruff check --select ASYNC,B,DTZ,RET,SIM`) but treat results as advisory.
- **Project footgun greps** over the surface (each maps to a real failure mode in this codebase):
  - **Lock held across `.await` or across expensive work** — `rg -n "\.lock\(\)|\.read\(\)|\.write\(\)" <surface>` then check whether the guard lives across an `.await` or a draw/render call. The standing `BUGS.md` time bomb (TUI draw lag) is a guard-lifetime perturbation in `daemon/src/workflow/transcript/cache.rs`; treat any lock-on-the-UI/draw-path as suspect.
  - **Fire-and-forget tasks** — `rg -n "tokio::spawn|thread::spawn"` with no retained `JoinHandle`/thread handle. Background workers here (tunnel respawn in `host_pool`, `manifest_watch`, `attach_worker`, `session_watch`) must be owned or they die silently and the feature looks "wired" but isn't.
  - **Mirrored / paired paths drifting apart (the X7 pattern)** — a fix applied to one copy but not its twin:
    - daemon auth `daemon/src/control/auth.rs::check_session_caller` vs its TUI mirror `tui/src/control/methods.rs::caller_authorized_for`.
    - the session-state triplet `DaemonSession` / `TerminalSession` / `ManifestEntry` — a field (e.g. `global_perms`, `host_id`) added to one but not the others.
    - the two MCP server copies — `mcp_server/server.py` and predictionTrading's `scripts/mcp/claude_manager_server.py` (a planning tool added to one only).
    Grep both members of each pair and diff their handling of the changed field/flag.
  - **serde schema drift** — new fields on manifest (`~/.cm/tui-sessions.json`), `state.json`, or `daemon.toml` structs that lack `#[serde(default)]`: old files fail to deserialize and the session/workflow silently vanishes. `rg -n "struct .*Entry|struct .*State|#\[derive\(.*Deserialize" <surface>` then check every new field.
  - **Permission scope / escalation** — `global_perms` reads, descendant-only scope checks, and the `start_session(global_perms=true)` escalation guard (honored only if the caller is itself global). A scope check that trusts the caller-supplied id without `check_session_caller` is the bug.
  - **Transport EOF vs `End` frame** — code in the attach/reattach path that treats a socket EOF as a child exit (or vice-versa). The invariant: a real daemon-side child exit always sends an `End` frame; a bare EOF is tunnel death and must requeue for reattach, not tear down.
  - **`.unwrap()` / `.expect()` / `panic!` on the daemon accept loop or PTY I/O** — a panic in a per-connection task can take down the host the TUI depends on.
  - **Cloud migrations** (if the surface includes `sql/`): row-level `UPDATE` in a migration re-runs on every API restart; migrations must be idempotent (`IF NOT EXISTS`).

## Tier 1 — Triage

One agent takes the Tier-0 scratch output + the surface file list and returns a deduplicated candidate list: `id, file:line, one-line claim, suspected severity`. Kill obvious clippy/style noise here; everything plausible survives to Tier 2 verification rather than being adjudicated by the triage agent alone.

## Tier 2 — Finder fanout (loop-until-dry)

Spawn diverse-lens finder agents via Workflow (or direct Agent calls). Each finder gets: the lens, the baseline commit, the surface file list, and the instruction to READ the code (and for refactored/moved code, diff against the pre-move original via git — moved code drops invariants). Default lens set (drop/add per subsystem):

concurrency & lock discipline (locks across await, deadlock order, draw-path contention) · async task lifecycle (dropped/un-owned spawns, cancellation, respawn-on-death) · daemon↔TUI mirror divergence (auth, state triplet, MCP copies) · session/PTY lifecycle & durability (survives daemon restart? resume binds the right pty?) · serde/persistence schema drift (forward/back compat of manifest, state.json, daemon.toml) · auth & permission scope (descendant-only, global_perms, escalation guard) · attach/reattach & tunnels (EOF-vs-End, backoff, in-place rebind) · workflow transitions (static on_idle gating on a *new* assistant message, fresh-context respawn, templating indices) · error handling & `.unwrap()`/panic on long-lived loops · config/host reachability (is the new code actually wired under the `local` default — not just the cloud path?)

- Finders return free-text findings with `file:line` citations and a concrete failure scenario each. If Workflow StructuredOutput schemas fail, fall back to free text — and the orchestrator MUST verify every fanout result actually landed (don't trust that N finders ⇒ N outputs; a silent drop reads as "nothing found").
- Finders/verifiers are instructed read-only, but instructions are not enforcement: run `git status` at every phase boundary and treat ANY unattributed working-tree change as a finding to review.
- Loop until dry: maintain a seen-set of ALL candidates (including killed ones — dedup against killed findings too, or they reappear every round). After each round, spawn a smaller round of fresh-angle finders on under-covered surface areas. Stop when 2 consecutive rounds yield nothing new, or the user's budget is reached.

## Verification — adversarial, three lenses

Group candidates by subsystem; one verifier agent per group, REFUTE-BY-DEFAULT, must re-read every cited line (no trusting the finder's paraphrase). Three explicit checks per finding:

1. **Real** — does the failure scenario actually execute? Trace the call path; check guards the finder may have missed (e.g. an early `if paused { continue; }`, a `check_session_caller` short-circuit).
2. **Reachable as actually constructed** — under the path the code is really driven by, not just in principle. The repo's analog of "prod construction": is it reachable under the **`local`-default** host and the daemon-default flip (mandatory since slice 10f), behind a keybinding that is actually dispatched, or only in a cloud/feature path that isn't the common case? A bug behind dead wiring is DOWNGRADED, not CONFIRMED.
3. **Already fixed on `main`** — re-run the Phase-0 port-gap check for the specific lines. Worktrees drift; a "bug" may be a fix `main` already shipped.

Verdicts: CONFIRMED (severity HIGH/MED/LOW + reachability note) / KILLED (why) / DOWNGRADED. Expect a real kill rate; if a verifier confirms everything, it isn't being adversarial.

## Findings doc + user gate (STOP here)

Write the findings doc before any fix: stable bug IDs, severity, `file:line`, one-paragraph mechanism, proposed fix, origin tag (NEW / PRE-EXISTING / regression-from-`<commit>`), plus the killed/downgraded list with reasons (so later rounds don't re-litigate). Location: a dated doc under `doc/` (e.g. `doc/bughunt-<surface>-2026-06-29.md`). `BUGS.md` at the repo root is the *separate* long-lived tracker for unsolved/recurring time-bomb bugs — only promote a finding there if it's understood-but-unfixed and likely to recur; do not dump the hunt's full list into it.

Then present the confirmed list ranked by severity × reachability and WAIT for the user's smell-check. Do not start fixing without it.

## Fix waves

- Group confirmed bugs into waves of FILE-DISJOINT fixes; parallel workers within a wave, waves sequential. Workers in this repo typically share one worktree, so brief them: stay strictly inside the assigned file set, no git mutations, no repo-wide `cargo fmt`, and NO-CLOBBER — if the assigned fix already exists (a retry/duplicate dispatch landed it), verify it against the bug entry instead of racing it.
- Direct fixes, NO feature flags for bug fixes. When you fix one side of a mirrored pair (auth, state triplet, MCP copies), fix BOTH in the same wave or the divergence just inverts.
- One regression test per bug, named so the hunt's tests are greppable: Rust `#[test] fn bughunt_<id>_...` in the relevant module's `mod tests`; Python `test_bughunt_*`. Run with `cargo test -p <crate>` (or the targeted module) — not the whole workspace each iteration.
- Orchestrator verifies each worker's edits actually landed (diff non-empty, test exists and passes) before counting the bug fixed.
- Workers can die mid-edit (session limits, API errors) AFTER landing partial changes. After every wave: `git status`, attribute every changed file to a worker, and for a dead worker's files VALIDATE the partial work by running its tests before deciding finish-vs-revert.
- **Do NOT auto-commit.** This repo's standing rule is to leave fix-wave changes UNSTAGED for the user to review and commit. Update the findings doc's fix-status table as you go and present the diff; let the user commit. (Only commit if the user explicitly asks, or you are explicitly acting as a design-doc-impl-loop-style orchestrator with that mandate.)

## Final gate

1. `cargo build --workspace` clean; `cargo clippy --workspace` clean on touched files.
2. Targeted `cargo test -p <crate>` per wave during iteration; ONE `cargo test --workspace` at the end (note pre-existing failures separately from anything the hunt touched). For daemon-side behavior that unit tests can't reach, a real-agent e2e via the operator socket (`scripts/e2e_bind.py`) is the ground truth.
3. `cargo fmt --check` clean (no stray reformatting outside the fixes).
4. Findings doc fix-status table complete; report killed-finding count alongside fixed count (the kill list is half the value). Working tree left unstaged for the user unless told otherwise.
