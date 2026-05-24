# Known bugs

Tracking unsolved bugs that recur across sessions so we don't keep rediscovering them. Open bugs go at the top; once a bug is fixed, remove it (the commit is the record).

---

## TUI lag triggered by heavy background task — masked by an unexplained `drop(map)` perturbation

**Status.** Not understood. A two-line workaround (`drop(map)` in `tui/src/workflow/transcript/cache.rs`) reliably suppresses the symptom, but the mechanism makes no sense from a code-reading standpoint. We're shipping the workaround OR keeping the laggy state — see "Disposition" at the end — but the underlying cause is unknown and could re-surface at any time as a real bug. **Treat as a time bomb.**

**Symptom.** TUI keystrokes lag noticeably (hundreds of ms per keypress) while a Claude session is producing heavy PTY output (e.g. long `cat`, verbose bash, big file reads). The lagging session is the **focused** one; the heavy-output session is **in the background**. `~/.cm/slow-ticks.log` shows `phase=draw` 200–725 ms and `phase=drain_terminal_events` up to 1.5 s during the regression window. Sample lines:

```
1779655351 phase=drain_terminal_events elapsed_ms=1169
1779655367 phase=drain_terminal_events elapsed_ms=1537
1779655346 phase=draw elapsed_ms=501
1779655353 phase=draw elapsed_ms=725
```

**Conditions where reproduced.**
- Heavy-output session is a **standalone** Claude session (not a workflow participant).
- A paused-but-active workflow exists in `~/.cm/workflow-runs/` (status `Paused`, not `Done`). The single paused run in the repro was `wf_6a123c1a2bd019e4`.
- User is focused on a *different* session from the heavy one.
- HEAD = `359c2c0` (or anything containing `c115efc` "more perf" — the transcript cache commit).

**Bisect result.** Reverting `c115efc` (the transcript byte-cursor cache) eliminates the lag. Reverting only the later commits (`3ff5ced`, `359c2c0`) does not. So the regression is associated with `c115efc`. **But** — see "What we ruled out" — the cache code itself is provably *not being called* in the reproduction scenario.

**What we ruled out (with evidence).**

1. **The cache code path is dead in this scenario.** All callers of `workflow::transcript::count_messages` and `role_turn_complete` (which is what calls `cache::get`) go through `workflow::controller::tick`. That function early-exits paused runs at `controller.rs:737` (`if paused { continue; }`) *before* any cache lookup. With only a paused workflow and a non-workflow standalone session, the cache should never be touched. We confirmed this empirically by adding a file-logging trace to `cache::get` and running the heavy task — `~/.cm/cache-trace.log` was **never created**, meaning the function was never entered. (The instrumentation patch is reproducible from the conversation transcript leading up to this entry; see step-by-step below.)

2. **It's not single-threaded mutex contention.** `cache::get` holds a `Mutex<HashMap<PathBuf, Entry>>`. Only one caller, only the UI thread. We grep'd for other callers and other Agent impls — no surprises (`tui/src/agent/{claude_code,codex}.rs` are the only impls; tests don't run in production).

3. **It's not the user's two unrelated edits in `c115efc`** (engine-mismatch validation removal in `workflow/controller.rs::launch_workflow`, `worktree.rs::remove_worktree` idempotency). Neither runs during a heavy background task — one only runs at workflow launch, the other only at `A-x`.

4. **It's not `sync_role_session_ids`** (the only part of `tick` that runs for paused workflows). It does string compares and workspace lookups, no transcript I/O, no cache touches.

**What "fixes" the symptom.** Adding `drop(map);` before the two return statements in `cache::get`:

```rust
// Fast path
if entry.mtime == mtime && entry.len == len {
    let cached = entry.cached_state.clone();
    drop(map);                  // <-- this line
    return Some(cached);
}

// Slow path tail
entry.cached_state = cached.clone();
entry.mtime = mtime;
entry.len = len;
drop(map);                      // <-- this line
Some(cached)
```

That's literally the only change vs. the laggy version. **No other instrumentation, no PathBuf clone, no `Instant::now()`, no logging** — just the two explicit drops. NLL would release the guard at essentially the same logical point, so this should be a no-op functionally. But the perturbation is reliable: with the drops, no lag; without them, lag.

The minimal-perturbation patch (the only thing we trust to "fix" the symptom) is in the conversation history and is trivially small; reapply with:

```rust
// in tui/src/workflow/transcript/cache.rs, function `get`
//   add `let cached = entry.cached_state.clone(); drop(map); return Some(cached);`
//     on the fast path
//   add `drop(map);` immediately before the final `Some(cached)` on the slow path
```

We have NOT committed this. The codebase as of `359c2c0` does not contain the drop perturbation.

**Hypotheses that remain plausible.**

1. **Compiler/codegen perturbation.** The presence of explicit `drop(map)` changes the lifetime metadata LLVM sees, which can shift register allocation, inlining decisions, or function layout. Different I-cache / branch-prediction behavior could mask whatever the real bottleneck is. This would mean the bug isn't in `cache::get` at all — `cache::get` is just close enough to the real culprit that LLVM-level perturbation in it incidentally affects the hot path. If true, the fix is fragile to any rustc upgrade or unrelated change in `cache.rs`.

2. **Heisenbug / system-state coincidence.** The repro is not bulletproof — maybe the OS page cache, alacritty's internal state, or some other process happens to make the difference, and `c115efc` only correlates with the lag because of when it was tested. The "drop fixes it" observation would then be coincidence, not causation. Argues against this: the user reports it's reliably reproducible with the bad build and reliably not reproducible with the dropped build, across multiple runs.

3. **A subtler interaction with alacritty / draw code we haven't traced.** The slow-tick log shows *both* `draw` and `drain_terminal_events` slow. Cache code can only plausibly affect `drain_terminal_events` (where `tick_workflows` runs). Slow `draw` is unexplained by any change in `c115efc` we've examined. Worth checking whether the slow `draw` and slow `drain` are correlated in time, or whether they're two independent symptoms with a shared root cause.

**How to reproduce.** Tested on this exact setup; YMMV.

1. Check out a commit that includes `c115efc` and *not* the drop workaround. `359c2c0` (current main as of this writing) is fine.
2. Ensure at least one workflow run in `~/.cm/workflow-runs/` is `status: Paused` (not `Done`). The reproduction case had exactly one paused feedback workflow.
3. `cd tui && cargo build && ./target/debug/claude-manager-tui`
4. In a standalone Claude session (not a workflow participant), run a task that produces heavy PTY output — e.g. "cat a large file" or "find . -type f | head -10000".
5. Focus a *different* session.
6. Try typing in the focused session — keystrokes should lag visibly. `~/.cm/slow-ticks.log` should accumulate new `phase=draw` / `phase=drain_terminal_events` entries in the 200+ ms range.
7. To verify the workaround: apply the two-line `drop(map)` patch above, rebuild, re-run the same task. Lag should disappear.

**Suggested next investigation steps.**

1. **Compare disassembly of `cache::get` with and without the drops.** If LLVM produces meaningfully different code for callers (function-level inlining boundaries shifting), that points at codegen as the mediator. `cargo asm` or `objdump -d target/debug/claude-manager-tui` on the relevant symbols.
2. **Profile with `perf` or a flamegraph** during the laggy state. The slow-tick log says draw + drain are hurting, but it doesn't say *where in them*. A flamegraph would localize.
3. **Instrument `draw` and `drain` themselves** — break each phase into sub-phases (per-workspace, alacritty render, ratatui frame, the top-level `Clear`, etc.) and log per-phase elapsed. The cache-trace pattern in this conversation is reusable.
4. **strace the laggy binary** for syscall rate / latency. Compare with the non-laggy build. If syscall behavior diverges meaningfully (read sizes, fdatasync calls, anything alacritty's PTY thread does), that's a clue. `strace -c -p <pid>` for aggregate, `strace -ttT -e read,write,futex,...` for per-call timing.
5. **Check alacritty scrollback eviction.** PERF.md notes the `scrolling_history` cap was tightened to 1500 (in `tui/src/session.rs:73`, commit `604132c`). Heavy output rapidly fills 1500 lines; alacritty has to evict. Pre-cap (10000) gave more headroom. Possibility: the lag is alacritty cell-buffer churn during eviction, and `c115efc` is irrelevant to the root cause. Test: bump scrolling_history back to 10000 temporarily, see if lag changes.
6. **Try with no paused workflows at all.** If lag disappears, the cause is somehow related to the paused workflow path (despite our reading that says nothing transcript-related runs for paused runs). If lag remains, paused workflows are a red herring.
7. **Add a global atomic counter to `cache::get`** that increments every call, and an Atomic flag that gets set on first call. Read both from a debug keystroke handler. This is more bulletproof than file-logging because it has no I/O overhead.

**Files of interest.**
- `tui/src/workflow/transcript/cache.rs` — the cache module; site of the load-bearing drops.
- `tui/src/workflow/controller.rs:737` — the `if paused { continue; }` that should prevent the cache from ever being called for paused workflows.
- `tui/src/app.rs:5051` — where `tick_workflows` is called from inside `drain_terminal_events`. Slow workflow ticks land in the slow-tick log as slow drain ticks.
- `tui/src/app.rs:4687-4708` — the focus-gated `needs_redraw` logic for background-session Wakeups. Currently correct (background Wakeups don't trigger redraws), but worth re-verifying that status transitions on background sessions don't fire frequently during heavy output.
- `~/.cm/slow-ticks.log` — slow-tick log; the primary signal source.
- `~/.cm/workflow-runs/<run-id>/tick.log` — per-workflow tick debug log (rate-limited).

**Disposition.** As of writing: workaround is *not* committed. The laggy state is shipped. Decide whether to commit the workaround as a stopgap depending on tolerance for the lag during ongoing work.

---

## Codex swallows all workflow Enters

**Symptom.** When the feedback workflow activates a Codex role, the visible result in the codex TUI is:

```
› /clearCan you review unstaged changes.
```

`/clear` and the prompt body collapsed onto one line, and codex never advances. Reported as "Enter doesn't get processed for codex."

**Root cause (confirmed).** `deliver_pending_write` in `tui/src/app.rs` unconditionally sets `ts.pending_enter` after every body write. The drain loop (around line 1827) delivers `pending_clear` first, then `pending_prompt` is gated only on `pending_clear.is_none()` — so the moment the clear is delivered (and `pending_clear` becomes None), the prompt is delivered on the **same or next tick**, well before the queued `pending_enter` for the clear has had a chance to fire (`ENTER_GAP = 10s`). The prompt's `deliver_pending_write` then **overwrites** `ts.pending_enter`, and the clear's Enter is silently dropped.

Confirmed in every recent tick.log (e.g. `~/.cm/workflow-runs/wf_69f90f111763ba5/tick.log`):

```
1777930757 delivered pending_clear: 6 body bytes ... reviewer
1777930757 delivered pending_prompt: 227 body bytes ... reviewer
1777930767 enter_fired mode=raw ... reviewer
```

Two bodies delivered in the same second; **only one** `enter_fired` 10s later. So codex receives `"/clearCan you review unstaged changes...\r"` as a single input line, submits it as a slash command, and gets back "Unrecognized command '/clearCan'."

**Why it looks Codex-specific.** Claude Code happens to tolerate the same merged `/clear` + body better (for slash-command UX reasons, claude rotates its transcript even mid-line). Codex submits the merged garbage as a slash command and errors. The bug itself isn't codex-specific — it just shows up worst there.

**Reproduced empirically.** A standalone Python PTY test (`/tmp/codex-test/test2.py`) that writes `/clear` then 227 bytes of body then a single `\r` 10s later produces exactly:

```
• Unrecognized command '/clearCan'. Type "/" for a list of supported commands.
```

So the trailing single Enter does fire and codex does submit — it just submits the wrong thing. This rules out paste-burst suppression (`PASTE_ENTER_SUPPRESS_WINDOW = 120ms`, well below our 10s) and Enter-encoding mismatches as the cause.

**Secondary finding (benign for now).** `tui/src/session.rs` calls `TermConfig::default()` for the alacritty terminal — which sets `kitty_keyboard: false`. Alacritty therefore **ignores** codex's `\x1b[>7u` Kitty-mode push, so `enter_bytes_for_mode` always sees no `DISAMBIGUATE_ESC_CODES` and emits raw `\r`. This is harmless because codex's level-7 Kitty mode doesn't change Enter encoding (Enter is still `\r` at levels 1–7; it would only become `\x1b[13u` at level ≥ 15). But the comment at `tui/src/app.rs:6541` claiming we detect Kitty Enter for codex is misleading — alacritty currently has no chance to detect it.

**Fix.**

The right fix is to sequence pending_clear's Enter properly:
- After delivering pending_clear, don't deliver pending_prompt until **pending_enter has fired**, not just until pending_clear is None.
- That guarantees codex sees: `/clear\r` → process slash command → 10s settle → prompt body → `\r` → submit.

Concrete change: gate the prompt-delivery branch on both `ts.pending_clear.is_none()` **and** `ts.pending_enter.is_none()`. Or — cleaner — model "deliver clear then deliver prompt" as one state machine instead of two independent `Option`s racing.

While in there: also fix the alacritty `kitty_keyboard` config (set `kitty_keyboard: true` in `TermConfig` in `session.rs`) so the comment at `app.rs:6541` is actually true. Not the bug, but worth doing in the same cleanup.
