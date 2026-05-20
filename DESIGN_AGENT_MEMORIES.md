# Agent Memories: snapshot + clone for sessions

## Goal

Let the user save a named snapshot of a running session's full context (transcript + memory), then later launch a new session that resumes from that snapshot — diverging from it. Snapshots are immutable templates; to "save back" diverged state, the user creates a new snapshot.

Use case: build up a session that's been primed with project context, conventions, a persona, or a partially completed investigation — then fork it for similar future work without paying the priming cost again.

## Non-goals (v1)

- Mutable named agents that accumulate state across runs. Snapshots are immutable.
- Snapshotting workflows (multi-session). Only single sessions in v1.
- Snapshotting bash PTYs. No meaningful "resume" semantics; only `claude-code` and `codex` engines.
- Sharing snapshots between machines / users. Local-only, lives under `~/.cm/`.
- Snapshot diffing, merging, or hierarchical inheritance.

## Concepts

- **Snapshot** — a named, immutable on-disk capture of one session's transcript + (for Claude Code) per-cwd memory dir, plus a manifest with provenance.
- **Clone** — a new session created by copying a snapshot's files into the new session's expected paths, then spawning the agent with the appropriate resume flag.

## Storage layout

```
~/.cm/agent-memories/
  <name>/
    manifest.json
    transcript.jsonl
    memory/                # only present for claude-code snapshots
      MEMORY.md
      *.md
```

`<name>` is user-supplied. v1 disallows slashes (see "Name validation" below); names are flat single-component identifiers like `reviewer-strict` or `pre-onboarded-reviewer`.

### manifest.json

```json
{
  "version": 1,
  "description": "Reviewer primed with our codebase conventions",
  "engine": "claude-code",            // "claude-code" | "codex"
  "source_session_uid": "…",           // ManifestEntry.uid from ~/.cm/tui-sessions.json
  "source_transcript_id": "…",         // session-id; for codex this is payload.id
  "source_cwd": "/home/lucas/.cm/worktrees/…",  // original worktree (informational)
  "created_at": "2026-05-19T14:32:11Z",
  "transcript_bytes": 184320,
  "memory_files": 5                    // 0 for codex
}
```

**No `name` field in the manifest.** The snapshot name is canonically the directory name under `~/.cm/agent-memories/`. Storing it twice (on disk path *and* in JSON) invites them to drift after a rename; keeping path as the single source of truth means rename is a pure directory `rename(2)` and never has to touch the manifest.

### Name validation

- Allowed characters: `[A-Za-z0-9_.-]` only (no slashes in v1 — see below).
- No leading `.`, no `..` components.
- Max 128 chars.
- Reject if `~/.cm/agent-memories/<name>/` already exists.

**No slashes in v1.** Earlier draft allowed `reviewer/strict`-style nested names. Dropped because it creates ancestor/descendant ambiguity: deleting `reviewer` would clobber `reviewer/strict`; renaming a parent would silently move children; the catalog browse semantics get tangled (is `reviewer` a snapshot, a folder, or both?). Users can use `-` or `_` (e.g. `reviewer-strict`, `reviewer-lenient`) to group informally in v1. Real hierarchy is future work — when added, on-disk storage will stay flat (one dir per snapshot, slug-based) with the display name in the manifest, so parent/child can coexist safely.

## Save flow

Triggered by `A-b` (Alt+b, "bookmark") on a focused session in Sessions view.

Earlier drafts proposed `A-S` / `A-M` (Shift+Alt+s/m), but existing dispatch handlers for `A-s` (app.rs:4438) and global `A-m` (app.rs:4300) match `KeyCode::Char('s' | 'm')` without excluding `KeyModifiers::SHIFT`. Terminals differ on whether Shift+Alt+s arrives as `Char('S')` or `Char('s') + SHIFT` — the existing code's `A-W`/`A-w` precedent at app.rs:4442-4457 handles both forms but only because that handler explicitly checks for SHIFT. To use a Shift+Alt binding we'd have to either modify the existing handlers (touching the mouse-toggle global path) or accept that some terminals would fire the lowercase action. Simpler: pick free letters. Only `b` and `z` are free across all of `tui/src/`, so we use both — `A-b` for save (bookmark mnemonic), `A-z` for the catalog. If the catalog binding ever feels obscure we can revisit, but the catalog is also reachable from inside the create-session form's seed-from picker, so it isn't a hot path.

1. Look up the focused `ManifestEntry`. If `session_type` is not `claude` or `codex`, show error toast "Snapshots only supported for Claude Code / Codex sessions" and bail.
2. Open `InputMode::SaveSnapshot { ws_index, session_idx, name_text, description_text, active_field }`. Two text fields; Tab cycles.
3. On submit, validate name. If conflict → inline error, stay in modal.
4. Resolve the source transcript path:
   - Claude Code: `claude_transcript_path(cwd, transcript_id)` at `tui/src/agent/claude_code.rs:86`.
   - Codex: `codex_transcript_path(transcript_id)` at `tui/src/agent/codex.rs:85` (walks `~/.codex/sessions/`).
5. Create `~/.cm/agent-memories/<name>/` (with parents). Copy:
   - `transcript.jsonl` ← source transcript JSONL.
   - `memory/` ← `~/.claude/projects/<encoded-source-cwd>/memory/` if it exists, claude-code only. Skip silently if absent.
6. Write `manifest.json`.
7. Toast "Snapshot saved: `<name>`".

Atomicity: copies happen into a temp dir `~/.cm/agent-memories/.tmp-<random>/`, then `rename`d to final location. Transcript files are append-only JSONL, so copying mid-write yields a valid prefix.

## Launch flow

Triggered from the existing A-n (new workspace+session) or A-s (add session to workspace) form, via an optional new field.

### Form field

Both `InputMode::NewSession` (app.rs:4977) and `InputMode::NewTerminalSession` (app.rs:5007) gain an optional `seed_from: Option<String>` field, surfaced as a new line in the modal: `Seed from snapshot: [none]`. Pressing Enter on that line opens the catalog picker (see below); Esc clears the selection back to `[none]`.

### Engine constraint

When `seed_from` is set:
- Snapshot's `engine` must match the session being created. If mismatched, the picker filters them out; if the user changes engine after picking, we clear `seed_from`.

### Spawn

When the form submits with `seed_from = Some(snap_name)`:

1. Resolve worktree path the same way as today (`worktree::create_worktree` for A-n; existing workspace cwd for A-s).
2. Engine-specific copy and transcript_id policy (see "Transcript ID policy" below):
   - **Claude Code:**
     - `transcript_id` for the clone = the snapshot's original `source_transcript_id` (kept as-is).
     - Destination transcript file: `~/.claude/projects/<encoded-new-cwd>/<transcript_id>.jsonl`. The encoded-cwd differs from the snapshot's source, so no filename collision in the normal case (A-n always creates a fresh worktree). For A-s into an existing worktree, check whether the destination exists; if it does, reject the launch with a clear error ("This snapshot has already been cloned into this worktree — to clone again, choose A-n or a different workspace"). This avoids the embedded-`sessionId` rewrite problem entirely (see open-question resolution below).
     - Copy `~/.cm/agent-memories/<name>/transcript.jsonl` → destination, unchanged.
     - Copy `~/.cm/agent-memories/<name>/memory/` → `~/.claude/projects/<encoded-new-cwd>/memory/`, merging with anything already there. (For a fresh worktree, nothing is there.)
     - On spawn, `mcp_config::build_args(..., resume_session_id = Some(&transcript_id), ...)` (already exists at `tui/src/mcp_config.rs:137`).
     - **Set `ts.transcript_id = Some(transcript_id.clone())` directly on the live `TerminalSession` immediately after spawn, and set `ts.pending_jsonl_files = None`.** Matches the existing resumed-Claude pattern at app.rs:5512. The id is known at spawn time, and the transcript file is already on disk (we just copied it) — so the file-detector baseline that normally pivots on "first new JSONL after spawn" has no new file to find, and without this direct set the session would never bind to its transcript. Clearing `pending_jsonl_files` ensures the detector isn't left expecting a baseline.
   - **Codex:**
     - `transcript_id` for the clone = a freshly-generated UUIDv4 (via `uuid::Uuid::new_v4()`, not `new_session_uid()` — see "Transcript ID policy"). This is required because Codex's discovery walks the whole `~/.codex/sessions/` tree matching `payload.id`; reusing the original id would make two files claim the same id and discovery becomes non-deterministic.
     - Destination transcript file: `~/.codex/sessions/YYYY/MM/DD/<transcript_id>.jsonl` (today's date).
     - Read snapshot's `transcript.jsonl`, parse line 1 as JSON, rewrite **both** `/payload/id` (to the new transcript_id) **and** `/payload/cwd` (to the new worktree path). The `cwd` rewrite is required because the TUI's Codex session-binding scan at app.rs:2017 requires `/payload/cwd` to equal the worktree path — without it, the cloned transcript would not bind to the live session. Write the rewritten line 1 plus the remaining lines unchanged.
     - **Do NOT set `ts.transcript_id` to the cloned `transcript_id`.** That id is a *resume-source* id — `codex resume <id>` reads our seed JSONL and then mints a **fresh rollout id** for the ongoing session, writing to a brand-new JSONL under `~/.codex/sessions/YYYY/MM/DD/`. Pointing the live session at the seed file would leave it bound to a transcript Codex stops writing to as soon as the agent replies. Instead, leave `ts.transcript_id = None` and let the existing `detect_codex_session_id` path at app.rs:2033 (driven by the `pending_jsonl_files` baseline established at spawn) discover the new rollout id. Diverges from the Claude branch above: Claude's `--resume <id>` keeps writing to the same file, so the seed id is also the live id; Codex's `resume <id>` does not.
     - No memory dir to copy.
     - On spawn, `mcp_config::codex_args(..., resume_session_id = Some(&transcript_id))` (already exists at `tui/src/mcp_config.rs:157`).
3. Set `seeded_from_snapshot = Some(snap_name)` on the live `TerminalSession` (and via that, on the persisted `ManifestEntry` — see "Manifest changes" below).
4. Spawn as normal.

### Transcript ID policy

Two distinct ID spaces. They are not interchangeable:

- `ts.uid` — TUI/MCP identity, produced by `new_session_uid()` at app.rs:917 in the format `ts-<nanos>-<counter>`. Used for the cm-tui-sessions manifest, MCP auth tokens, etc. **Not** an engine-recognized transcript id.
- `ts.transcript_id` — engine-format identity. Claude Code uses UUIDs (the JSONL filename and embedded `sessionId` fields throughout). Codex uses whatever `payload.id` was set on line 1 (also typically a UUID).

For clones:
- Claude Code: reuse the snapshot's `source_transcript_id` verbatim. No new id is minted, no in-file rewrite needed. The id is also the live transcript id post-resume — set it onto `ts.transcript_id` directly. Limitation: a snapshot cannot be cloned twice into the same worktree (see edge case #9 below).
- Codex: mint a fresh `uuid::Uuid::new_v4()`. This requires `uuid` in `tui/Cargo.toml` — verify in chunk 1 and add if missing. The minted id is a **seed-file id**, not the live rollout id — `codex resume <id>` reads our seed JSONL once, then writes a brand-new rollout transcript with its own id. So `ts.transcript_id` must be left for the existing rebind detection to fill in; do not assign the seed id directly. See `ClonedSession` rustdoc.

## Catalog view (`A-z`)

`A-z` is one of two Alt+letter bindings currently unused anywhere in `tui/src/` (`b` and `z`). Weak mnemonic, but the catalog is also reachable via the seed-from picker in the create-session form, so the keybinding is not the primary entry point.

Modeled on `draw_workflow_picker` at app.rs:7643. Flat scrollable list, sorted by name. No fuzzy filter in v1; can be added later if list grows.

Per-row display: `<name>  · <engine>  · <created_at relative>  · <description first line>`.

Actions:
- Enter — open detail pane (read-only) showing full manifest + transcript head/tail.
- `n` — new session from this snapshot. Opens A-s-style form with `seed_from` pre-filled. (For A-n with worktree creation, the user picks worktree fields in that form.)
- `r` — rename. Inline text edit; revalidates name; renames the directory via `std::fs::rename(2)`. Since the manifest no longer contains a `name` field, no JSON write is needed — rename is a single atomic syscall.
- `d` — delete. Confirm modal "Delete snapshot `<name>`? (y/n)".
- Esc / `A-z` — close.

The catalog also doubles as the picker invoked from the seed-from field in the session-create form, but in picker-mode Enter selects rather than opens detail and the rename/delete keys are disabled.

## Manifest changes

The provenance field has to live on **both** the persisted `ManifestEntry` and the in-memory `TerminalSession`. `save_session_manifest` at app.rs:2153 rebuilds entries by reading live `TerminalSession` fields one-by-one — anything not on `TerminalSession` is silently dropped on every save. So:

1. `ManifestEntry` (app.rs:269) gains:

   ```rust
   #[serde(default, skip_serializing_if = "Option::is_none")]
   pub seeded_from_snapshot: Option<String>,
   ```

2. `TerminalSession` gains the same field (not serde-derived since it's in-memory only; populated either when a clone is spawned or when restoring from a manifest entry).

3. `save_session_manifest` (app.rs:2159) gets one new line: `seeded_from_snapshot: ts.seeded_from_snapshot.clone(),`.

4. `restore_sessions` (the deserialize-and-rehydrate path) carries `ManifestEntry.seeded_from_snapshot` back onto the live `TerminalSession`.

Backwards compatible — older manifests deserialize cleanly via `#[serde(default)]`.

The sidebar's session-info hover/preview surfaces "Seeded from: `<name>`" when present.

## Module layout

New file: `tui/src/agent_memory.rs`. Public surface:

```rust
pub struct Snapshot { /* parsed manifest + path */ }

pub fn list() -> io::Result<Vec<Snapshot>>;
pub fn load(name: &str) -> io::Result<Snapshot>;
pub fn save(...) -> io::Result<Snapshot>;     // takes engine, source cwd, source transcript_id, etc.
pub fn delete(name: &str) -> io::Result<()>;
pub fn rename(old: &str, new: &str) -> io::Result<()>;

pub fn clone_into_session(snap: &Snapshot, new_cwd: &Path, new_transcript_id: &str) -> io::Result<()>;
```

Name validation lives in this module. All path construction is centralized here so app.rs / session.rs don't reach into `~/.cm/agent-memories/` directly.

### Path helpers and module visibility

`claude_transcript_path` (claude_code.rs:86) and `codex_transcript_path` (codex.rs:85) are `pub(super)` inside the `agent::` submodules and not callable from `agent_memory.rs`. The comment at claude_code.rs:84 already notes that the same logic is intentionally duplicated rather than coupling agent → workflow internals.

We follow that existing precedent: `agent_memory.rs` duplicates the path-construction logic (it's ~5 lines each) rather than widening the agent submodules' visibility. The duplication risk is bounded — the encoded-cwd convention is a stable, externally-determined interface (Claude Code's filesystem layout, Codex's `~/.codex/sessions/` walk) that won't drift between us and the agent module.

## Keybinding summary

- `A-b` (Sessions view, focused session): save snapshot.
- `A-z` (Sessions view, anywhere): open catalog.
- In existing A-n / A-s form: new optional "Seed from snapshot" field; Enter on that field opens catalog in picker-mode.

Both `A-b` and `A-z` are verified free across all of `tui/src/` (the only two Alt+lowercase letters not bound anywhere). No Shift+Alt collisions, no global handler changes needed. Planning view bindings untouched. The `A-l` ambiguity in CLAUDE.md is not relevant to this feature and out of scope here.

## Edge cases & open questions

1. **Snapshotting a session whose transcript file doesn't exist yet.** Claude Code creates the JSONL only after the first agent message. If a user hits `A-b` on a newly-spawned session that hasn't received any output, the transcript path doesn't resolve. Behavior: show error toast "No transcript yet — let the session produce at least one message first." Don't create an empty snapshot.

2. **Snapshotting an active session.** Copy is non-atomic w.r.t. the agent's writes. For JSONL this is safe (line-oriented append-only). We copy bytes-as-of-now; if the agent writes line N+1 mid-copy, the snapshot may include a partial line. Mitigation: after copying, read the last line of the snapshot transcript; if it doesn't end with `\n`, truncate to the last complete line.

3. **Memory dir collisions on clone.** A fresh worktree's projects dir doesn't exist yet, so copying in the snapshot's `memory/` is fine. If we ever support cloning into an existing worktree that already has a `memory/`, we'd need a merge policy. Out of scope for v1 — A-n always uses a fresh worktree, and A-s uses the existing workspace cwd which the user opted into; we overwrite-on-conflict and toast a warning.

4. **What does Codex actually do with a rewritten `payload.id` and `payload.cwd`?** The format is `codex resume <id>`, and discovery walks `~/.codex/sessions/` for a file whose first-line `payload.id` matches. We also rewrite `payload.cwd` because the TUI's session-binding scan at app.rs:2017 requires it. Both rewrites should be verified by hand-rolling a cloned file and running `codex resume` before committing the implementation. Falls out naturally during chunk 3 / chunk 6.

   Subquestion: are there `payload.id`-style fields on lines 2+ of a Codex JSONL? **Resolved (chunk 1 inspection):** the session UUID (`payload.id` on line 1) appears nowhere else in the transcript — confirmed by grep against a real 99-line sample. `payload.cwd` is also line-1 only as a metadata field, but the cwd path string itself appears 30+ times in content (tool call args, agent outputs) as historical references. We rewrite `payload.id` and `payload.cwd` on line 1 only; we do **not** rewrite content path references on lines 2+. The agent's conversation history will show messages that reference the original worktree path, but those are inert content (not control fields) — analogous to history that mentions a renamed branch or moved file. Other UUID-shaped strings on later lines are per-event ids (Codex tags each event with its own UUID), not session-level, and need no rewriting.

5. **Workflow participants.** A workflow participant session has `workflow_run_id` and `workflow_role` set. Snapshotting one captures only its slice of the conversation; the snapshot has no notion of "this was a reviewer in feedback mode." On clone, those fields are NOT carried over — the cloned session is just a normal session. This is consistent with "snapshot is a single-session concept." Future work: workflow-level snapshots.

6. **Memory dir is shared across sessions in the same worktree.** Two Claude Code sessions in the same workspace share `~/.claude/projects/<encoded>/memory/`. Snapshotting either captures the same memory dir. This is correct behavior — memory belongs to the cwd, not the session. Worth noting in the catalog detail view ("Memory: 5 files (worktree-shared)").

7. **What if the user renames a snapshot that another `ManifestEntry.seeded_from_snapshot` points at?** The provenance field becomes stale. We don't update it; it's informational. Same for delete.

8. **Disk usage.** A snapshotted transcript can be megabytes. We don't enforce limits in v1; can add a cumulative size check or LRU eviction later. Add a "Total snapshots / total size" line in the catalog header for visibility.

9. **Claude Code: cloning a snapshot twice into the same worktree.** Because we reuse the snapshot's `source_transcript_id` for Claude Code clones (to keep embedded `sessionId` fields coherent), the destination file path `~/.claude/projects/<encoded-cwd>/<transcript_id>.jsonl` would collide on a second clone into the same worktree. Behavior: detect the collision pre-spawn and reject with a clear error. The common path (A-n creates a fresh worktree) is unaffected. Workaround for the rare A-s case: clone into a different workspace, or first delete the colliding session.

10. **Resolved open question — Claude embedded `sessionId` fields.** Claude Code's JSONL embeds `sessionId` in each message line. Earlier draft proposed minting a new id and rewriting in-file occurrences, which would have been an unbounded rewrite (we'd need to track every place Claude Code stores the id). Resolved by keeping the snapshot's original `source_transcript_id` verbatim on Claude Code clones — no in-file rewrite, no field-discovery risk. The trade-off is edge case #9 above, which is an acceptable limitation.

## Implementation chunks

1. **`tui/src/agent_memory.rs`** — module with list/load/save/delete/rename/clone_into_session, name validation, atomic write via tmp+rename, JSONL-truncate-to-last-newline. Duplicates the path helpers locally (does not import from `agent::`). Verifies `uuid` is in `tui/Cargo.toml` and adds it if missing. Inspects a real Codex JSONL to confirm whether lines 2+ have id references. No UI. Unit tests for name validation and the Codex line-1 rewrite (`payload.id` + `payload.cwd`).
2. **`seeded_from_snapshot` field on both `ManifestEntry` and `TerminalSession`** — serde-default on the persisted entry; in-memory only on `TerminalSession`. Update `save_session_manifest` (app.rs:2159) to copy the field across, and update the restore path to rehydrate it. Surface in sidebar session-info.
3. **Save modal (`A-b`)** — `InputMode::SaveSnapshot`, Sessions-view key dispatch for Alt+b, handler calls `agent_memory::save`. Errors (engine not supported, name conflict, no transcript yet) shown inline in the modal.
4. **Catalog view (`A-z`)** — list/detail/rename/delete + picker-mode entry point. New `InputMode` variant(s) or a small dedicated view enum, plus Sessions-view dispatch for Alt+z. Cloned from `draw_workflow_picker` pattern. Rename is `std::fs::rename(2)` only — no manifest writes.
5. **Seed-from-snapshot field in A-n / A-s** — extra field in `InputMode::NewSession` (app.rs:4977) and `InputMode::NewTerminalSession` (app.rs:5007), picker-mode invocation of the catalog, spawn-time call to `agent_memory::clone_into_session`, pass `resume_session_id` to `build_args` / `codex_args`. **Engine-asymmetric** post-spawn handling:
   - **Claude Code:** set `ts.transcript_id = Some(cloned.transcript_id.clone())` and `ts.pending_jsonl_files = None` (matches the resumed-Claude pattern at app.rs:5512). The cloned id is also the live transcript id — `claude --resume` keeps writing to that same file.
   - **Codex:** do **not** assign `cloned.transcript_id` to `ts.transcript_id`. Leave `ts.transcript_id = None` and seed `ts.pending_jsonl_files` with the normal at-spawn baseline so the existing `detect_codex_session_id` (app.rs:2033) discovers the new rollout id Codex mints after `codex resume`. The cloned id is a seed-file id only; Codex stops writing to that file once it starts the new rollout. See `ClonedSession` rustdoc for the full rationale.
6. **Manual verification** — snapshot a real Claude Code session, clone it, confirm the agent resumes mid-conversation and sees its memory files. Same for Codex (confirm `payload.cwd` rewrite makes the session bind in the TUI sidebar). Verify the collision-rejection error for the "clone twice into same worktree" Claude Code case.

Chunks 1–2 are independent and can land first. 3 depends on 1. 4 depends on 1. 5 depends on 1 + 4 (picker). 6 is the verification gate before merge.
