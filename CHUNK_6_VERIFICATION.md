# Chunk 6: manual verification checklist

The chunks 1–5 unit tests cover save/load/clone semantics, rollback,
the picker round-trip, and the engine-asymmetric session wiring against
mocked HOME. Chunk 6 is the merge gate that exercises the feature
end-to-end against real `claude` and `codex` processes.

## Build

```bash
cargo build --release --manifest-path tui/Cargo.toml
```

Then run the release binary:

```bash
./tui/target/release/claude-manager-tui
```

## Test plan

### 1. Claude Code: save + clone + resume

Verifies the seed transcript + memory dir land at the right paths, the
new session resumes mid-conversation, and the cloned session inherits
memory.

1. In the TUI, open a Claude Code session on a workspace.
2. Hold a short conversation. Suggested: "Remember the magic word is
   `ostrich-violet-23`" then "What's the magic word?". The agent
   should confirm.
3. Save the per-cwd memory file with content the agent will see on
   resume. Create the `memory/` directory first — on a fresh Claude
   project path it won't exist yet, and the redirect will silently
   fail otherwise (the snapshot would then save with no memory and
   the HALCYON check below would fail for the wrong reason):
   ```
   ENCODED=$(echo "$PWD" | tr '/' '-' | tr '.' '-')
   mkdir -p ~/.claude/projects/"$ENCODED"/memory
   echo 'The current project codename is HALCYON.' > \
       ~/.claude/projects/"$ENCODED"/memory/CONTEXT.md
   ```
   (Or trigger Claude's own memory-writing flow if you'd rather.)
4. With the session focused, press **A-b** to open the save modal.
5. Type a snapshot name like `ostrich-test` and press Enter.
6. Confirm the toast `Snapshot saved: ostrich-test`.
7. Verify the on-disk layout:
   ```bash
   ls ~/.cm/agent-memories/ostrich-test/
   # expect: manifest.json, transcript.jsonl, memory/
   jq . ~/.cm/agent-memories/ostrich-test/manifest.json
   # engine should be "claude-code"; transcript_bytes > 0
   ```
8. Press **A-n** to open the new-workspace form.
9. Fill in label / repo / branch as usual. Tab to the **Seed** field
   (field 4, marked `[none]`).
10. Press Enter on the Seed field. The catalog opens in picker mode
    titled " Pick Snapshot ", filtered to claude-code engines only.
11. Select `ostrich-test` with j/k and press Enter. The form reopens
    with `Seed: ostrich-test` filled in.
12. Submit the form (Enter on a non-seed field).
13. The new workspace spawns. Once the agent prompt is ready, ask:
    > "What's the magic word, and what's the project codename?"

    **Expected:** the agent answers `ostrich-violet-23` and `HALCYON`.
    This confirms (a) `claude --resume <id>` picked up the cloned
    transcript and (b) the memory dir landed in the new worktree's
    `~/.claude/projects/<encoded-cwd>/memory/` so the agent can see it.

### 2. Claude Code: collision rejection

Verifies the "snapshot can't be cloned twice into the same worktree"
guard.

1. From the **new-session** workspace created in step 1, press **A-s**
   to add a session.
2. Tab to the **Seed** field; press Enter; pick `ostrich-test` again
   from the picker.
3. Submit. **Expected:** toast contains `snapshot already exists` (or
   similar, surfacing the `AlreadyExists` error from
   `clone_into_session`). The session row should NOT be added.
4. Verify no orphan transcript:
   ```bash
   ls ~/.claude/projects/$(pwd | tr '/' '-' | tr '.' '-')/
   # The same <transcript_id>.jsonl should still be there from the
   # original spawn — exactly one file, not two.
   ```

### 3. Codex: save + clone + binding

Verifies `payload.id` and `payload.cwd` are rewritten correctly and the
TUI's `detect_codex_session_id` binds the live session to the new
rollout (not the seed file).

1. Add a Codex session to a workspace (**A-s**, j/k to codex, Enter).
2. Hold a short conversation: "Remember the magic word is `puffin-9`".
3. Save: **A-b**, name it `puffin-test`, Enter.
4. Verify:
   ```bash
   jq . ~/.cm/agent-memories/puffin-test/manifest.json
   # engine: "codex"; memory_files: 0
   ```
5. **A-s** on a fresh workspace's terminal to add another codex
   session. The dialog defaults to `claude`; press **j/k to codex**
   first (the seed picker filters by engine, so without this step
   `puffin-test` won't appear). Then Tab → Seed field → Enter → pick
   `puffin-test`.
6. Submit the form.
7. Once the agent prompt is ready, ask:
    > "What was the magic word?"

    **Expected:** the agent answers `puffin-9`.
8. Look at the sidebar — the new session row should show a
   `Seeded from: puffin-test` line in **A-e** (session settings).
9. **Critical:** confirm the TUI bound the live session to the NEW
   rollout. There are up to three relevant transcript IDs in play:
   - `manifest.json`'s `source_transcript_id` — the **original** saved
     Codex session (lives in the original date dir under
     `~/.codex/sessions/`, untouched by the clone).
   - The **cloned seed** — `clone_codex` mints a fresh UUID and
     rewrites `payload.id`, so this file does NOT match the manifest
     id. It lands under today's date dir.
   - The **live rollout** — Codex writes a new file when it actually
     starts running; this also has a fresh UUID, different from both
     of the above.

   Check:
   ```bash
   ls -lt ~/.codex/sessions/$(date +%Y/%m/%d)/*.jsonl | head -5
   ```
   (Note: this lists every Codex session created today, so unrelated
   sessions may appear too. Filter mentally by mtime.) Expected: at
   least two recent files — the cloned seed (older mtime, written at
   clone time) and the live rollout (newer mtime, written when the
   session began streaming). The sidebar's `transcript_id` should
   match the **newer** (live rollout) file and should NOT equal
   `manifest.json`'s `source_transcript_id`.

### 4. Catalog browse / rename / delete

Sanity-check the standalone catalog UI.

1. Press **A-z** (anywhere in Sessions view). The catalog opens.
2. Use j/k to navigate. Press Enter on a snapshot — the detail pane
   shows manifest fields + transcript head/tail.
3. Press Esc/Enter to return to the list.
4. Press `r` on a snapshot. Inline rename editor opens with the
   current name pre-filled. Type `-v2`, press Enter.
5. Confirm the renamed entry appears at its new name and the dir
   was renamed:
   ```bash
   ls ~/.cm/agent-memories/
   ```
6. Press `d` on a snapshot. Confirm prompt `Delete snapshot <name>?`.
   Press `y`. Confirm the row disappears and the dir is gone.

### 5. Picker-cancel preserves form state

Regression for the seed_from-preservation fix.

1. Press **A-n**, fill in label/repo/branch.
2. Tab to **Seed** field; Enter to open picker; pick a snapshot.
3. Form reopens with `Seed: <name>` set.
4. Press Enter on the Seed field again to re-open the picker.
5. Press Esc to cancel. **Expected:** form reopens with the same typed
   label/branch/repo **and** the same `Seed: <name>` still set.

### 6. Failure modes

These are harder to trigger without artificial faults but worth a
quick check:

- **Engine-not-supported:** focus a bash session, press A-b. Toast
  should read `Snapshots only supported for Claude Code / Codex sessions`.
- **No transcript yet:** spawn a Claude session and immediately press
  A-b before sending a prompt. Toast should read `No transcript yet —
  let the session produce at least one message first`.
- **Bad snapshot name (invalid char):** A-b → type `bad/name` → Enter.
  Modal should stay open with
  `invalid snapshot name: name cannot contain path separators`
  (the validator special-cases `/` and `\` separately from the
  per-character allowlist). For a non-separator invalid char like
  `bad@name`, expect
  `invalid snapshot name: name contains invalid character '@' (allowed: A-Z a-z 0-9 _ - .)`.
- **Bad seed in A-n:** edit a manifest on disk to corrupt the
  engine field, then try to launch a seeded A-n. The pre-flight
  `validate_seed_loadable` should fail BEFORE any worktree is created
  (verify no new dir under `~/.cm/worktrees/`).

## After verification

Open a PR. Ship.
