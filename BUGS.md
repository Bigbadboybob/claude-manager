# Known bugs

Tracking unsolved bugs that recur across sessions so we don't keep rediscovering them. Open bugs go at the top; once a bug is fixed, remove it (the commit is the record).

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
