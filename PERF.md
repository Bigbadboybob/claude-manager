# TUI performance follow-ups

Survey performed 2026-05-24 against `cm/improve-perf`. Findings come from a code read across the Rust TUI — not from profiling. Symptoms motivating the survey: keypress→paint lag with many sessions open, occasional RAM spikes.

## Shipped on this branch

- `session.rs:73` — alacritty `scrolling_history` capped at 1500 (was 10000 default). Drops worst-case per-Term scrollback RAM ~6×.
- `app.rs` drain_terminal_events:
  - `needs_redraw` gated to focused-session Wakeups + structural events (status transition, Title, Exit/ChildExit, sid detection). Background-session PTY chatter no longer drives the redraw loop.
  - `wakeup_times` push coalesced to one entry per 50ms — bounds the in-memory window to ~40 entries even at kHz wakeup rates. Burst detection (≥5 in 2s) still works.
  - `bound_sids` HashSet only built on ticks where session-id detection is actually due, not every drain tick.
- `app.rs:5041` — `tick_workflows` throttled to 10Hz and skipped entirely when `workflow_runs` is empty. Each tick does whole-transcript reads per active role; was running at drain frequency.
- `workflow/controller.rs:756` — idle check derives `will_fire` from already-computed `count + is_idle` instead of calling `assistant_turn_completed_since` (which re-runs both, costing 2 extra transcript reads per role per tick).

## Open follow-ups, ordered by (impact desc, ease asc)

### High impact

1. **Cache codex session directory walk** — `app.rs:2881-2935`, `app.rs:4857-4895`.
   Every 5s while any codex session is unbound, walks `~/.codex/sessions/**/*.jsonl` and parses the first line of each. With hundreds of historical sessions this is the biggest disk-I/O hit on a routine basis.
   Fix: keep a cached cwd→sid index keyed by `(path, mtime)`; rescan only when a directory's mtime changes, or use notify/inotify.

2. **Workflow transcript caching with byte cursors** — `workflow/transcript.rs:289-424`, `workflow/controller.rs:754-769`.
   Even with the 10Hz throttle, every workflow tick re-reads the *entire* transcript file twice (once in `count_messages`, once in `role_turn_complete`). Transcripts grow to many MB.
   Fix: keep a parsed-line cursor per `(transcript_id, generation)` — re-stat the file, seek to the cached offset, parse only the appended bytes, update count/is-idle incrementally. Invalidate on generation bump.

3. **Selective top-level `Clear`** — `app.rs:8330`.
   `frame.render_widget(Clear, content_area)` runs every frame even when layout hasn't changed. I verified `Terminal::swap_buffers` already resets the back buffer in current ratatui, so this `Clear` is largely redundant — but the comment warns of layout-shrink artifacts in some past version, so leave it gated behind a `layout_changed` flag rather than just deleting it.
   Fix: track `last_view_mode`, `last_activity_visible`, `last_input_mode_kind` on App; render Clear only when one of them differs from this frame's value.

4. **Manifest save off the UI thread** — `app.rs:3020-3119`, save sites at 3784, 3947, 3975, 4103, 4141, 4152, 4961, 5092, 5362, 5526.
   `atomic_write_manifest()` calls `fsync` then rename, on the UI thread. Many session mutations trigger it. On a slow disk or NFS this stalls the loop.
   Fix: bump a `manifest_dirty` flag; have a single background writer thread debounce (e.g. 250ms) and write. The TUI's authoritative state lives in memory; on crash, the manifest is best-effort.

### Medium impact

5. **Cache `visual_items()`** — `app.rs:4386-4552`, called from `draw_session_list` every redraw.
   Rebuilds the entire flat or hierarchical session list every frame. With the redraw rate now lower (after the focus-gate fix above), the per-call cost matters less, but it's still ~O(W·S) every paint.
   Fix: cache `Vec<VisualItem>` + a content-hash of `(workspace_ids, session statuses, hidden flags, task_ids, workflow_run_ids)`; recompute only on hash change. Or invalidate explicitly at the small number of mutation points.

6. **HashMap-ify task reconciliation** — `app.rs:5537-5775`.
   Repeated O(N) `Vec::iter().find()` scans by `task_id` and `workspace_id` during reconcile, plus an O(N²) workspace-rank lookup at 5757-5775.
   Fix: prebuild `HashMap<task_id, idx>` and `HashMap<workspace_id, idx>` once per reconcile.

7. **Backend `list_tasks` called twice per poll** — `backend.rs:349-379, 800-819`.
   Every 5s the backend thread calls `list_tasks` twice — once for the regular task list, once for planning — and ships full Vecs back.
   Fix: single call, partition locally, emit only if changed (compare via a hash or row checksum).

8. **inotify on history + workflow events** — `workflow/history.rs:48-72`, `workflow/events.rs:67-100`.
   Each tail re-stats/seeks/reads its file every drain tick.
   Fix: notify/inotify watcher; otherwise stat with cached mtime and bail when unchanged.

9. **`apply_history_rotations` cost reduction** — `workflow/history.rs:153-207`.
   Rotation resolution scans every Claude transcript file and reads the whole thing to inspect the first ~50 lines.
   Fix: filter candidates by directory mtime first, then `BufReader::lines().take(50)`. Cache earliest-timestamp per transcript.

10. **Bounded control queue + worker pool** — `control/queue.rs:36-50`, `control/server.rs:82-91`.
    Queue is unbounded and the server spawns one OS thread per client; requests drain serially on the UI thread.
    Fix: bound the queue; drain a fixed budget per tick; small worker pool or async accept.

### Lower impact / nice to have

11. **Shared scheduler for memory watchers** — `session_watch.rs:356-463`.
    One polling thread per capped session. Move to a single tick scheduler shared across all sessions.

12. **Planning grid caching** — `planning.rs:1482-1630, 3023-3299`.
    Visible-rows projection rebuilt per column per frame in the Planning view. Cache per-(project, layout-generation, fold-state).

13. **Avoid the `xclip -o` subprocess on the UI thread** — `app.rs:4677-4686`.
    On OSC 52 clipboard load we shell out synchronously inside `drain_terminal_events`. Move to a worker thread; race condition risk is minimal since clipboard reads are user-initiated.

14. **`detect_session_id` Vec scan per JSONL file** — `app.rs:2845-2871`.
    `existing_files.contains(&stem.to_string())` per file. Convert `existing_files` to `HashSet<&str>` (and pass borrowed) before the walk.

15. **Control method workspace/session lookups** — `control/methods.rs:42-210, 493-614`.
    Every method scans workspaces+sessions linearly for auth + lookup. Maintain a `HashMap<session_uid, (wi, si)>` index updated on session add/remove.

## Notes for whoever picks this up

- The Clear (#3 above) is the easiest immediate next win and zero-risk if gated behind a layout-change flag.
- The codex-walk cache (#1) is the biggest disk-I/O reduction.
- For #2 and #8, the prerequisite is a transcript-position cache keyed on `(transcript_id, generation)` — the generation bump on `/clear` is already tracked and is the natural invalidation point.
- Slow ticks > 200ms get logged to `~/.cm/slow-ticks.log` (see `main.rs:160`); use that to confirm which phase actually hurts before tackling a finding.
