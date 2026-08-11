# Agent Orchestration — Design Doc

## Goal

Give agents running inside the TUI the same control surface a human user has: spawn subtasks (in the same worktree or in a child worktree branched from the parent), spawn and manage sibling sessions, prompt and read other sessions, launch and steward workflows, and mark work done. Everything stays visible in the TUI sidebar with clear markers for "this is a subtask of X" and "this session is being driven by agent Y."

Permission is a prompt-level convention ("ask the user before acting"), not a hard gate. The user remains in the loop via a real-time activity feed in the TUI.

This is a substantial expansion of the MCP surface and requires three foundational pieces of infra before the user-facing tools land:

1. A generic **`Agent` trait** abstracting Claude Code and Codex behind one interface (submit prompt, read messages, idle detection, interrupt). This is shared by workflows and by the new MCP tools. Closing a session is a workspace-level operation, not a trait method (see "Closing is workspace-level" in Phase 0).
2. A **TUI control socket** (Unix domain socket at `~/.cm/tui.sock`) speaking length-prefixed JSON. The MCP server is a client; the TUI is the server.
3. A **subtask data model** — `parent_task_id` and `worktree_mode` on tasks, plus rendering in the planning view.

---

## Architecture

```
┌──────────────────────────────┐
│           TUI                │   owns: PTYs, tasks, workflows,
│  ┌────────────────────────┐  │          worktrees, sidebar,
│  │ Agent trait + impls    │  │          activity feed
│  │   ClaudeCodeAgent      │  │
│  │   CodexAgent           │  │
│  └────────────────────────┘  │
│  ┌────────────────────────┐  │
│  │ Control socket server  │◄─┼── ~/.cm/tui.sock
│  └────────────────────────┘  │
└──────────────────────────────┘
              ▲
              │ JSON-RPC over Unix socket
              │
┌─────────────┴────────────────┐
│      MCP server (Python)     │   Spawned per agent session by Claude Code
│  ┌────────────────────────┐  │   or Codex. Reads CM_TUI_SESSION_ID from env
│  │ Tool handlers          │  │   (capability token, see "Authorization
│  │  start_session()       │  │   model" below). Task/workspace are derived
│  │  send_input()          │  │   server-side from the UID, not env-supplied.
│  │  read_session_output() │  │
│  │  start_workflow()      │  │
│  │  create_subtask()      │  │
│  │  ...                   │  │
│  └────────────────────────┘  │
│  ┌────────────────────────┐  │
│  │ Transcript readers     │  │   Direct file reads, but only AFTER a
│  │   ~/.claude/projects   │  │   resolve_authorized_session call returning
│  │   ~/.codex/sessions    │  │   {state, engine, transcript_path?, generation}.
│  └────────────────────────┘  │
└──────────────────────────────┘
```

Reads of session output use a two-step pattern. Each `read_session_output` call first hits the socket with `resolve_authorized_session(target_session_uid)`; the TUI authorizes the request (descendant-only check) and returns:

```
{
  state: "ready" | "pending" | "exited",
  engine: "claude-code" | "codex",
  transcript_path: string | null,    // null when state == "pending"
  generation: integer
}
```

- **`ready`**: session is bound to a transcript file. `transcript_path` is set and the file exists.
- **`pending`**: session was spawned but no transcript ID has been detected yet (the detector at `tui/src/app.rs:~1909` is asynchronous). Or `/clear` was just run and rebinding hasn't completed. `transcript_path` is `null`. The MCP server returns `messages=[]` with the same input cursor and `state: "pending"` to the agent caller — the agent can poll again, or move on.
- **`exited`**: session is gone but the last-known transcript still exists; `transcript_path` is populated from the manifest. Reads work; `submit_prompt` / `kill_session` would error.

The resolve call is cheap (in-memory tree walk + path lookup), so the high-frequency cost — JSONL parsing — stays off the TUI main loop while authorization stays centralized.

Why re-resolve every call instead of caching: when a target runs `/clear`, the TUI rebinds it to a fresh transcript file. A cache that persists the old `transcript_path` would silently keep reading a stale file. The `generation` field bumps on every rebind. The default path re-resolves every call. Callers that want to cache across a burst of reads can compare `generation` to detect staleness.

---

## Data model

### Task fields (additions)

```
parent_task_id   string | null   FK to tasks.id; null for top-level tasks
worktree_mode    "inherit" | "branch"   only meaningful when parent_task_id is set
                                        default: "inherit"
```

For `branch` mode, the child's worktree is created from the parent's `wip_branch` with branch name `cm-sub/<slug-chain>-<short_id>`, where `<slug-chain>` is the parent → child slug path joined by `-` and `<short_id>` is the first 7 chars of the child task's UUID. Examples:

- One-level child of parent `refactor-auth`, child `extract-helper`: `cm-sub/refactor-auth-extract-helper-a1b2c3d`
- Grandchild under that: `cm-sub/refactor-auth-extract-helper-rename-fns-e4f5a6b`

The `cm-sub/` prefix is followed by **exactly one path component** (no further slashes), which makes it impossible for any branch in this namespace to be a prefix of another. Earlier drafts used hierarchical slashes (`cm-sub/<parent>/<child>`), but that recreates the git ref-prefix collision at depth ≥ 2: `refs/heads/cm-sub/a/b` cannot coexist with `refs/heads/cm-sub/a/b/c`. The flat-with-id form sidesteps that entirely. The `<short_id>` suffix also handles the edge case of two children with identical slug chains (e.g. two `extract-helper` subtasks under the same parent) without collision. `git branch --list 'cm-sub/*'` still enumerates every subtask branch; readers can recover lineage from the slug chain.

For `inherit` mode, the child has no worktree of its own; its sessions spawn in the parent's worktree directory. Two agents stepping on each other's edits is the user's concern, by design (matches the doc-level convention).

### Workspace-less tasks: `start_session` mints (ux-1a)

A task can be spawn-authorized while having no workspace at all. The canonical case is `propose_task`: the row lands in the planning backlog, the daemon records a creator edge so the proposer may launch it, but nothing has ever created a checkout for it. `create_subtask` in `branch` mode before its worktree registration lands is the other.

`start_session(task_id=<that task>)` **mints one**. The daemon reads the task's planning row for its `repo_url`, resolves that repo on the host (the caller's own `main_repo_path` when it's the same repo, else `resolve_repo`), and cuts a fresh branch worktree from the project's trunk:

- Branch: `cm-sub/<name-slug>-<first 7 chars of the task id>`. The suffix is **derived from the task id, not random** (unlike `create_subtask`'s), so re-minting the same task is idempotent — it finds the directory already there and hands back the original checkout rather than scattering a second one. The `cm-sub/` prefix means `recover_worktree_path` and the TUI's reconcile recovery map it back for free.
- Cut point: `main`, `origin/main`, `master`, `origin/master` in that order, all resolved locally first; only a total miss spends a `git fetch`. Local-first for the same reason `create_subtask(base=…)` is local-first — the operator's own commits are usually the point.
- The workspace is registered and bound (`task_workspaces` + `bindings`) only **after** the checkout exists on disk, so a failed mint leaves nothing half-made. The next `start_session` on the task resolves through the ordinary binding and joins that same checkout.
- The task row gets `wip_branch` + `worktree_mode="branch"` recorded, best-effort — a PATCH failure costs recovery metadata, not the spawn.

Before this, the daemon route silently fell back to the **caller's** workspace whenever it had minted the task itself, and the TUI route answered a flat `NotFound` — two routes, two answers. The daemon's fallback caused a live incident: three `propose_task` workers spawned into the proposer's own worktree, followed their "branch off main" instructions by rewriting HEAD under a live session, and left uncommitted edits behind.

**`allow_shared_workspace=true`** opts back into that co-tenancy, for when a worker is genuinely meant to work in the caller's tree (or when minting isn't possible and the caller accepts the risk). It is honored but never silent: the response carries `shared_workspace: true`, a `warning` naming the shared checkout, and `workspace_shared_with` listing the other live sessions in it. When a mint is impossible and the flag was *not* passed, the spawn is **refused** rather than silently placed — and every such refusal names the flag as the way through.

### Session manifest fields (additions)

`~/.cm/tui-sessions.json` entries gain:

```
uid              string         stable per-session UID, generated at creation
managed_by_uid   string | null  the agent session that spawned/owns this one
generation       integer        bumps every time the transcript binding rebinds
                                (e.g. /clear). Starts at 0.
```

`ManifestEntry` stays a live-session-only record — no `exited` flag, no `exited_at`, no `last_transcript_id`. Once a session closes, its row leaves `ws.sessions` entirely (see Tombstones below). This avoids carrying half-populated tombstone fields on live entries and keeps the live-vs-dead split unambiguous.

A session is "agent-managed" iff `managed_by_uid` is set. Used for sidebar markers and for the descendants-only authorization check.

`TerminalSession.uid` exists today (`tui/src/app.rs`) but is **explicitly in-memory only** ("Stable per-session id, generated at creation … not persisted"). For agent orchestration the UID *is* the identity used for authorization and capability tokens, so it must survive TUI restart. Phase 2 promotes it to a persisted field on `ManifestEntry` and reuses the same UID on restoration. Newly-spawned sessions still generate fresh UIDs; sessions restored from a manifest written by an older TUI build (no UID present) get a fresh UID assigned at load time and the manifest is re-saved.

`generation` lives on `TerminalSession` (in-memory authoritative) and is mirrored into `ManifestEntry` so it survives restart. It increments at every transcript rebind: today the only rebind path is `/clear` (`tui/src/app.rs` clears `session_id` and re-snapshots `pending_jsonl_files`), so generation bumps there. New rebind paths added later must bump it. The resolver (`resolve_authorized_session`) reads `generation` from the live `TerminalSession`; on cold-start before any restored session has rebound, the persisted value is used as-is.

**Tombstones for exited sessions.** The current close paths remove session entries entirely from the live list (`close_active_session` calls `ws.sessions.remove(si)`; `close_active_workspace` calls `ws.sessions.clear()`), and the manifest serializer only iterates live `ws.sessions`. That's incompatible with the resolver's `state: "exited"` contract — there'd be nothing left to resolve. But we **don't** want to keep the full `TerminalSession` alive after exit either: it owns a `Session`, which owns a private PTY `File` writer (`tui/src/session.rs`), and dragging those resources around for resolved-but-dead sessions is wasteful and error-prone.

The fix is a separate, lightweight tombstone record:

```rust
// tui/src/app.rs (or alongside ManifestEntry)

pub struct SessionTombstone {
    pub uid: String,
    pub managed_by_uid: Option<String>,
    pub label: String,
    pub session_type: String,        // "claude" | "codex"
    pub task_id: Option<String>,
    pub last_transcript_id: Option<String>,
    pub generation: u64,
    pub exited_at: f64,
}
```

`Workspace` gains a parallel `tombstones: Vec<SessionTombstone>` field. When a close path fires, it extracts the relevant fields off the dying `TerminalSession` into a `SessionTombstone`, pushes it into `ws.tombstones`, and **then** drops the live entry as today (preserving the existing PTY teardown). No PTY resources outlive their session.

The manifest serializer round-trips both `sessions` (live) and `tombstones` (read-only metadata). Sidebar rendering and cursor navigation never look at tombstones — the user-visible behavior doesn't change. The resolver consults live `ws.sessions` first; if the UID isn't found there, it falls back to `ws.tombstones` and returns `state: "exited"` with `last_transcript_id` resolved to a path. Retention: on TUI startup, drop tombstones older than 30 days (configurable). The bound is generous because the data is small and these are exactly the records an agent might want to look at "what did session X do yesterday?"

### Authorization model

The transcript session ID (the UUID an agent uses to name its `~/.claude/projects/<encoded>/<id>.jsonl` file) is **not known before the agent starts** — Claude Code and Codex generate it themselves on first run. So the MCP server can't be given a transcript ID via env, and the existing workflow MCP env wiring (`tui/src/workflow/spawn.rs`) only sets `CM_WORKFLOW_RUN_ID` / `CM_ROLE` for that reason.

Instead, the TUI assigns a **stable TUI session UID** at PTY-spawn time, before the agent process starts. This UID is the identity used for authorization throughout the system; the transcript ID is bound to it later (when the first transcript line appears, or on the next session-list refresh). The MCP server only ever sees the TUI session UID.

Env vars set when the TUI spawns an agent session:

```
CM_TUI_SESSION_ID   stable UID assigned by TUI before spawn
CM_TUI_SOCKET       path to ~/.cm/tui.sock (override-able for tests)
```

There is **no `CM_TUI_TASK_ID` in the env**, even though earlier drafts called for one. Regular `A-n` sessions are explicitly taskless today (`tui/src/app.rs:~3499`, `task_id: None`), and trusting a caller-supplied task ID would also be a hole — the agent could overwrite the env to claim a different task. Instead, the **TUI derives** the caller's workspace and (optional) task from `session_uid` by looking it up in the live manifest. The MCP request envelope only carries `session_uid`; the server fills in workspace/task on its side.

Today only **workflow-participant** spawns inject MCP config / env vars (`tui/src/workflow/spawn.rs` writes a per-role `<role>-claude.json` with `CM_WORKFLOW_RUN_ID` / `CM_ROLE`). Regular local spawns and planning-launched spawns (`tui/src/app.rs:~3530, ~4085`) start the agent with `--dangerously-skip-permissions` and **no MCP config or env injection at all**. That means without changes, none of the new tools would be reachable from a regular session — only workflow participants could call them.

**Phase 1 has to extend MCP injection to every agent spawn path.** Concretely:

1. Pull the current MCP config writer out of `workflow/spawn.rs` into a shared helper (`tui/src/mcp_config.rs`) that takes only the session UID and optional workflow metadata (`run_id`, `role`), and writes a per-session config file under `~/.cm/mcp/<session_uid>/claude.json`. **No task ID parameter** — taskless `A-n` sessions are first-class callers and the helper must work for them. The MCP server derives task/workspace server-side from the UID; nothing about MCP wiring depends on knowing the task at spawn time.
2. Every spawn site (regular `A-n`, planning `A-l`, workflow participant) calls the helper before `Session::new` and injects `--mcp-config <path>` into the args plus `CM_TUI_SESSION_ID` / `CM_TUI_SOCKET` (and `CM_WORKFLOW_RUN_ID` / `CM_ROLE` when applicable) into the env. **No `CM_TUI_TASK_ID`** — see the auth model: task/workspace are derived server-side from the UID.
3. Codex spawns get the equivalent treatment via whatever MCP config mechanism Codex supports (verify before Phase 1 — if Codex MCP is wired differently, branch on `session_type`).

Without this step, Phase 3+ tools are dead code from the perspective of regular sessions.

These act as **soft capability tokens**, not a security boundary. Same-user processes can read each other's environments (`/proc/<pid>/environ` on Linux), and the agents themselves can run shell commands as the same user — so any process running on the box could in principle observe these values and impersonate a session. The descendant-only auth check is therefore an **accidental-misuse guardrail**, not a defense against a malicious local process. Its job is to keep an honest agent from drifting into unrelated work, and to make agent-driven changes traceable in the activity feed. If you ever need a real security boundary (cross-user, multi-tenant, untrusted code), this design has to be revisited — short-lived signed tokens issued per-call would be a starting point, but it's out of scope here.

When the MCP server receives a tool call, it includes its `CM_TUI_SESSION_ID` in the socket request. The TUI looks up the caller's workspace and task from the live manifest, then authorizes:

- **Caller has a task** (workflow-launched or planning-launched): authorize per the **task tree** rule. Read/mutate target T allowed iff `T.task_id` is `caller.task_id` or a descendant in the parent_task_id tree. Same for sessions, scoped via the session's bound task.
- **Caller is taskless** (regular `A-n` session): authorize per the **workspace** rule. Read/mutate target allowed iff target is in the same workspace as the caller. The taskless agent can manage sibling sessions in its workspace and create child sessions there, but cannot see or touch tasks (it has none) or other workspaces.
- **`create_subtask` from a taskless caller**: errors. Subtasks need a parent task. The taskless agent should call `propose_task` instead to add a top-level task (existing tool, unchanged).
- **Resolve transcript path for session S**: same workspace-or-task rule as above. Returns `{state, engine, transcript_path, generation}` (full contract in the Architecture section). `transcript_path` is `null` when `state == "pending"`; resolution falls back to the workspace's tombstone list when `state == "exited"`.

By default there's no global access — an agent can't see or touch unrelated work. The one exception is **global-permissions** sessions (below).

#### Global permissions

A session may carry a **`global_perms`** flag. When set, that session's caller short-circuits the scope rule above to authorized for **any** target: it can `list_sessions` across every task and workspace, and `send_input` / `read_session_output` / `kill_session` / `start_session` against any session — not just its task-tree descendants. This is the "privileged orchestrator" capability for an agent meant to supervise unrelated work.

The flag lives on the session record on both sides of the auth split: `DaemonSession.global_perms` (daemon — gates daemon-routed `kill_session` / `read_session_output` / `list_sessions` / `start_session`) and `TerminalSession.global_perms` (TUI — gates the TUI-routed `send_input`). It persists in the manifest (`ManifestEntry.global_perms`) so the grant survives restart, and is checked in `daemon/src/control/auth.rs::check_session_caller` (short-circuit to `Allow`) and its TUI mirror `tui/src/control/methods.rs::caller_authorized_for`.

Granting (two paths, both gated so a normal agent can never self-escalate):

- **Operator (human)**: toggle "Global perms" in the A-e session-settings dialog. The TUI updates the live `DaemonSession` via the operator-only `session.set_global_perms` RPC and persists the manifest. This is also the bootstrap path for the first global agent.
- **Agent propagation**: a session that is *already* global may pass `global_perms=true` to `start_session` to mint a global child. The daemon's `mcp_start_session` enforces an **escalation guard** — the grant is honored only if the caller itself is global; otherwise `unauthorized`. A non-global agent therefore cannot create a privileged child to escape its own scope.

Like the rest of this model, `global_perms` is an **accidental-misuse guardrail, not a security boundary** (same-user processes can already impersonate each other — see the soft-token note above). Its job is to let a deliberately-privileged orchestrator reach across the session graph while keeping ordinary agents scoped. The prompt-level convention still applies: a global agent should state its intent and ask the user before driving unrelated sessions.

---

## Phase 0 — `Agent` trait (do this first; gate all later phases on it)

### Trait shape

```rust
// tui/src/agent/mod.rs

use anyhow::Result;
use std::path::{Path, PathBuf};

pub enum Engine { ClaudeCode, Codex }

pub enum Role { User, Assistant, Tool }

pub struct Message {
    pub role: Role,
    pub content: String,    // tool calls rendered as one-line summaries
    pub ts: f64,
}

pub struct Cursor {
    /// Opaque to callers. Encodes both a generation and an offset so the
    /// parser can detect cursor-vs-file mismatch after a transcript
    /// rebind (e.g. `/clear`). On generation mismatch, the parser
    /// silently restarts from offset 0 of the current file. Format:
    /// `v1:<generation>:<offset>` (the `v1` prefix lets us evolve later).
    pub raw: String,
}

/// Bag of refs the engine needs to operate on a session. Identity and
/// transcript binding live on `TerminalSession`; `worktree_path` lives on
/// `Workspace` (a session has no direct field for it). Claude Code's
/// transcript path is derived from `(worktree_path, transcript_id)`
/// (`tui/src/workflow/transcript.rs`), so the trait can't take just
/// `&TerminalSession` — it needs the workspace path too. Codex's path
/// is per-user-and-date and doesn't need the worktree, but giving every
/// trait method the same context shape keeps the API uniform.
#[derive(Clone, Copy)]
pub struct AgentCtx<'a> {
    pub ts: &'a TerminalSession,
    pub worktree_path: &'a Path,
}
// Copy is safe and zero-cost: AgentCtx is just two shared refs.
// It also makes the default impl of `assistant_turn_completed_since`
// compile — that helper uses ctx twice (count_assistant_turns then
// is_idle), so it needs to be either Copy or passed by reference.

pub struct AgentCtxMut<'a> {
    pub ts: &'a mut TerminalSession,
    pub worktree_path: &'a Path,
}
// AgentCtxMut intentionally does NOT derive Copy — the &mut ref isn't
// Copy. The methods that take it (submit_prompt, interrupt) consume
// it once; no reuse needed.

/// The trait is purely the engine-specific strategy: how to encode a
/// prompt, how to parse a transcript, what idle means for this engine.
/// Implementations are zero-sized strategy objects.
pub trait Agent: Send + Sync {
    fn engine(&self) -> Engine;

    /// Queue a prompt for delivery. **Returns immediately** — actual
    /// delivery is deferred to the TUI main loop, which uses the existing
    /// `PendingWrite` mechanism (see `app.rs::PendingWrite`,
    /// `deliver_pending_write`). That mechanism enforces:
    ///   1. an `earliest_deliver_at` floor (don't fire before the user
    ///      has had a chance to notice),
    ///   2. a quiet window (`require_quiet`) — no PTY wakeups for N ms
    ///      so the agent has finished rendering its prompt,
    ///   3. a separate later-fired `pending_enter` so the body and the
    ///      Enter keystroke are NOT seen as a single paste (codex treats
    ///      paste-with-trailing-\r as literal text, never submitting).
    ///
    /// Implementations push a `PendingWrite` into `ctx.ts.pending_write`
    /// (or the equivalent slot — exact field TBD by Phase 0
    /// implementation) rather than touching the PTY directly.
    /// Engine-specific Enter encoding (kitty-keyboard mode) is selected
    /// at delivery time by the main-loop drainer, not at queue time.
    fn submit_prompt(&self, ctx: AgentCtxMut<'_>, text: &str) -> Result<()>;

    /// Cursor-based read of the transcript file located via
    /// `transcript_path(ctx)`. The implementation handles cursor
    /// versioning + generation-mismatch detection internally.
    fn read_messages(
        &self,
        ctx: AgentCtx<'_>,
        since: Option<&Cursor>,
        limit: usize,
    ) -> Result<(Vec<Message>, Cursor)>;

    /// Resolve the on-disk transcript path for this session. Claude Code
    /// derives it from `(ctx.worktree_path, ctx.ts.transcript_id)`; Codex
    /// ignores `worktree_path` and derives from `transcript_id` alone.
    /// Returns `None` when the session is freshly spawned (or
    /// just-cleared) and the transcript ID hasn't been detected yet — see
    /// "not-ready contract" in Phase 1.
    fn transcript_path(&self, ctx: AgentCtx<'_>) -> Option<PathBuf>;

    /// Idle = the last assistant turn is complete and there are no
    /// pending tool calls. Used as the turn-complete predicate inside
    /// the workflow gate (see `assistant_turn_completed_since`) and
    /// for surface state in `read_session_output`.
    fn is_idle(&self, ctx: AgentCtx<'_>) -> bool;

    /// Count of assistant turns visible in the current transcript.
    /// "Assistant turn" means a top-level assistant message; tool-use
    /// blocks within a turn don't add to the count. Engine-specific
    /// because Claude Code and Codex frame turns differently in their
    /// JSONL schemas.
    fn count_assistant_turns(&self, ctx: AgentCtx<'_>) -> usize;

    /// Workflow gate predicate: has the role produced a complete
    /// assistant turn at or after `baseline`? `baseline` is the
    /// assistant-turn count snapshotted at activation time by the
    /// workflow driver in `app.rs`.
    ///
    /// Default impl: `count_assistant_turns(ctx) > baseline && is_idle(ctx)`.
    /// Engines override only if they need a different "turn boundary"
    /// definition — neither current engine does.
    ///
    /// This is what static `on_idle` workflow transitions check; raw
    /// `is_idle` alone would fire on stale idle output before the freshly
    /// delivered prompt produced a response.
    fn assistant_turn_completed_since(
        &self,
        ctx: AgentCtx<'_>,
        baseline: usize,
    ) -> bool {
        self.count_assistant_turns(ctx) > baseline && self.is_idle(ctx)
    }

    fn interrupt(&self, ctx: AgentCtxMut<'_>);   // ctrl-C; PTY-level only
}
```

Identity (`uid`), transcript binding (`transcript_id`), and `generation` are accessed directly on the `TerminalSession` — no trait method, just field access. This means callers always have a clean separation between *who is this session* (TerminalSession fields) and *how does this engine behave* (Agent trait methods).

### Closing is workspace-level, not a trait method

Earlier drafts had `fn kill(&self, ts: &mut TerminalSession)` on the trait. That can't work cleanly: closing a session has to (1) tear down the PTY (engine-agnostic — the bytes for "close" are just dropping the `Session`), (2) build a `SessionTombstone` from the dying entry's metadata, (3) push it into the **workspace's** `tombstones` vec, and (4) drop the live entry. Steps 2–4 are workspace-level mutations the trait has no access to.

So closing lives on the App: `App::close_session(session_uid) -> Result<()>`. It does the four steps above, in order, and is the implementation of the `kill_session` MCP tool, the `A-w` keybind in the sidebar, and any other "close this session" entry point. The trait's job stops at queueing input bytes and parsing transcripts.

### Field rename: `TerminalSession.session_id` → `TerminalSession.transcript_id`

The current field name `session_id` (`tui/src/app.rs`) is misleading once `uid` is the stable identity: the value it holds is actually the transcript-file UUID, which is unstable (absent at startup, changes on `/clear`). Phase 0 renames the field to `transcript_id` for clarity and updates every reference (`ManifestEntry.session_id` → `transcript_id`, the detector at `~1909`, the `/clear` handler at `~6228`, all read sites). The on-disk manifest field is also renamed; restored manifests written by older builds backfill from the old `session_id` key.

The doc throughout uses `ts.transcript_id` to refer to this field; that's the post-rename name.

### Implementations

- `ClaudeCodeAgent` — zero-sized strategy struct. `transcript_path(ctx)` resolves to `~/.claude/projects/<encoded(ctx.worktree_path)>/<ctx.ts.transcript_id>.jsonl` when `ctx.ts.transcript_id` is `Some`, else `None`. The encoding is the existing `claude_transcript_path` helper in `tui/src/workflow/transcript.rs` (path → slashes/dots replaced with `-`). `read_messages` parses Claude Code's JSONL schema (user/assistant/tool entries with timestamps). Idle = last entry is `assistant` with no pending `tool_use`. `transcript_id` becomes `Some` asynchronously via the existing detector (`tui/src/app.rs:~1909`) that's already running today.
- `CodexAgent` — same shape; `transcript_path(ctx)` ignores `ctx.worktree_path` and resolves to `~/.codex/sessions/YYYY/MM/DD/<ctx.ts.transcript_id>.jsonl`. Codex emits a different schema; `read_messages` normalizes to the same `Message` type. `submit_prompt` queues a `PendingWrite` whose drainer applies the kitty-keyboard-aware Enter encoding already present in `app.rs`.

The existing PTY paste code in `app.rs` (workflow activation prompts) moves into the trait impls. Workflows then call `agent.submit_prompt` instead of doing PTY writes inline.

### Test suite (the heart of phase 0)

Tests live in `tui/src/agent/tests.rs`, run with `cargo test`. The strategy is **fixture-based**: real Claude Code and Codex transcripts are committed under `tui/testdata/transcripts/{claude_code,codex}/*.jsonl`. The tests exercise the parser/cursor logic against them without spawning real agents.

PTY-byte-level testing is **out of scope** for this suite. `Session` (`tui/src/session.rs`) owns a private `File` PTY writer with no testable seam, and `submit_prompt` no longer writes to the PTY at all — it queues a `PendingWrite` into the `TerminalSession`. So the testable behavior is "given input X, does the right `PendingWrite` end up queued?" and that's verified directly:

- The `submit_prompt` impls delegate to a pure helper (e.g. `make_prompt_pending_write(text, engine_quirks) -> PendingWrite`).
- Tests construct a synthetic `TerminalSession` (or just operate on the helper directly) and assert the resulting `PendingWrite` shape: body bytes, `earliest_deliver_at` floor, `require_quiet` window, deferred-Enter scheduling.

The actual byte-on-PTY behavior is covered by the manual feedback-workflow smoke test that's part of the Phase 0 exit criteria — that test exercises real PTY writes through the existing drainer and would catch any regression in the queue → PTY path.

Coverage matrix:

| Concern | Claude Code | Codex |
|---|---|---|
| Empty transcript | `read_messages(None)` → `[]` | `read_messages(None)` → `[]` |
| Single user turn | parses correctly | parses correctly |
| Single assistant turn | parses correctly | parses correctly |
| Tool use rendered as one-liner | yes | yes |
| Multi-turn conversation | order preserved | order preserved |
| Cursor advances | second call with returned cursor → no overlap | same |
| Cursor stable across appends | append a turn → next call returns just the new one | same |
| `limit` honored | request 2 of 5 → 2 returned, cursor mid-stream | same |
| `is_idle` after assistant final | true | true |
| `is_idle` mid tool-call | false | false |
| `is_idle` after user msg, before assistant | false | false |
| `submit_prompt` helper produces a `PendingWrite` with body bytes + a deferred `pending_enter` (separate, not concatenated) | yes, with engine-specific quiet-window timing | yes, with codex's longer settle delay |
| `interrupt` queues 0x03 in the right slot | yes | yes |
| `App::close_session(uid)` builds a `SessionTombstone`, pushes to `ws.tombstones`, drops live entry (PTY torn down with it). Workspace-level — covered by an integration test, not a trait test. | covered once, engine-agnostic | same |
| Malformed JSONL line | skipped, doesn't poison cursor | skipped |
| Missing transcript file | `read_messages` → `Ok(([], cursor))` | same |

Aim for ~30 tests at the trait level. They're fast and high-leverage — every later phase relies on this layer being correct.

### Migration of existing code

After Phase 0:
- `app.rs` workflow activation flow uses `Agent::submit_prompt`.
- Workflow `on_idle` detection is **partially** ported to the trait. The current gate (`tui/src/app.rs:~5944`, `~6134`) does two distinct things:
  1. Snapshots an assistant-turn count at activation, then waits until `current_count > start_count` — i.e., proves the newly activated role has actually produced a new assistant turn for *this* activation, not a stale one.
  2. Confirms that turn is complete (no pending tool calls / not mid-stream).
  Only (2) maps cleanly to `Agent::is_idle`. Replacing the whole gate with `is_idle` alone would let static transitions fire on stale idle output before the freshly delivered prompt produced a response. So the migration keeps the activation baseline/count machinery in `app.rs` (the snapshot at activation, the comparison) and routes the actual predicate through `Agent::assistant_turn_completed_since(ctx, baseline)` (defined in the trait snippet above), which combines both checks. The workflow `on_idle` driver calls this with the snapshot it already takes today; nothing else in the gate moves.
- The existing detection is verified-equivalent against the test fixtures before swap-over: the test suite includes a fixture transcript with mid-turn idle followed by a new prompt and assistant reply, and asserts that `assistant_turn_completed_since(baseline=N)` returns `false` for the stale state and `true` only after the new turn lands.

---

## Phase 1 — Control socket

### Server (Rust, in TUI)

`tui/src/control/server.rs` runs an accept loop on `~/.cm/tui.sock` from a dedicated thread. Connections are one-shot (open, request, response, close) for simplicity; streaming subscriptions get added in a later phase if needed.

Wire format: 4-byte big-endian length prefix, then a UTF-8 JSON object.

Request envelope:

```json
{
  "id": "uuid",
  "caller": { "session_uid": "..." },
  "method": "start_session",
  "params": { ... }
}
```

Response envelope:

```json
{ "id": "uuid", "ok": true,  "result": { ... } }
{ "id": "uuid", "ok": false, "error": { "code": "...", "message": "..." } }
```

Errors: `unauthorized` (caller can't see this session/task), `not_found`, `invalid_params`, `internal`.

The server forwards each request to a queue read by the TUI main loop. The main loop is the only thing that mutates app state, so all handlers run on it (no shared-state mutex juggling).

### Client (Python, in MCP server)

`mcp_server/control_client.py` — ~50 lines. Opens the socket, writes a length-prefixed JSON envelope, reads the response, raises on error. The only caller identity sent is `CM_TUI_SESSION_ID`; the socket path comes from `CM_TUI_SOCKET` (falling back to `~/.cm/tui.sock`). Task/workspace context is derived server-side, not env-supplied.

### Tests

Server: integration tests that spin up the socket in a thread, send canned requests, assert responses. Python client: unit tests against a fake socket server (`socketpair`).

---

## Phase 2 — Subtask data model & planning-view tree

### Backend

- Schema migration adds `parent_task_id` and `worktree_mode` columns (cloud DB) and the equivalent fields in the local task store.
- API: existing task CRUD accepts the new fields. List endpoints don't change shape — clients filter/group by `parent_task_id` themselves.

### Planning view

- Tasks with children render with a leading `▶` (collapsed) or `▼` (expanded). Default: collapsed.
- Children are indented one level under their parent, recursively.
- Expand/collapse focused row: `Space` (tree-view convention; doesn't conflict with existing planning bindings).
- Bulk operations (`A-A` archive done) operate on the visible flat list — collapsed children aren't touched. Re-confirm this is the desired behavior.

### Sessions / Task sub-view

- Render up to 3 levels of depth inline.
- `A-z` to **focus** the selected task: the visible "root" becomes the focused task, descendants render fresh from there. A focus-stack is maintained.
- `A-Z` (or `Esc` if not in another modal) to pop one level.
- Subtask header prefix: `↳`. Indent one cell per level.
- Agent-managed sessions get a `[mgr: <label>]` suffix in muted color, where `<label>` is the manager session's short label.

---

## Phase 3 — Session-management MCP tools

These hit the control socket. The `task_id` parameter is **always optional** — when omitted, the default is:

- For a caller with a task (workflow- or planning-launched): the caller's task. Listings/spawns are scoped to that task.
- For a taskless caller (`A-n`): the caller's **workspace**, no task binding. Listings include all sessions in the workspace; new sessions inherit the same taskless state.

When `task_id` is explicitly provided, the auth model still applies. A taskless caller passing a `task_id` gets `unauthorized` (it has no relation to that task tree).

All session-targeting tools take a **`session_uid`** (the stable TUI-assigned UID), never a transcript ID. Transcript IDs are unstable (absent at startup, change on `/clear`) and never appear in the MCP API surface — agents have no reason to know they exist.

```
list_sessions(task_id?, include_exited=false)
   -> [{session_uid, label, type, state, idle, status, reported_done,
        managed_by_uid, task_id, workspace_id, worktree_path,
        workspace_shared_with?}]
   # state in {"ready", "pending", "exited"}.
   # When include_exited=false (default), only live ws.sessions entries are
   # returned and "exited" never appears. When true, the workspace's
   # tombstones are merged in and surface as state="exited" with idle=null.
   # The `exited` state remains reachable via read_session_output even
   # when the caller hasn't passed include_exited — read_session_output
   # always consults tombstones during resolve.
   # `workspace_id` / `worktree_path` ride TUI-owned rows too — the TUI's
   #   session-snapshot push carries both, so a TUI-launched sibling is no
   #   longer a workspace-less, checkout-less row.
   # Exited rows additionally carry how they ended: exited_at, killed,
   #   killed_by (see kill_session).
   # `reported_done` / `reported_done_at` / `report_reason` = the agent's
   #   own done signal (see report_done). Present on live rows and
   #   carried onto the tombstone, so "finished, then exited" is
   #   distinguishable from "stopped mid-task".
   # `status` collapses (state, idle, reported_done) into one word:
   #   starting | working | awaiting_input | reported | exited.
   #   "reported" is the only one that means COMPLETION — awaiting_input
   #   means only that the agent stopped talking, which it also does
   #   mid-task and between fan-out waves.
   # `workspace_shared_with` = [{session_uid, label}] for the OTHER LIVE
   #   sessions running in the same worktree_path — the "who else is
   #   editing this checkout?" answer. Present only when the checkout is
   #   shared; exited rows neither receive it nor count as sharers. It is
   #   computed MCP-side from the rows the host already authorized for
   #   this caller — never from a wider re-query, which would leak uids
   #   across the auth boundary. Same field in list_sessions_grouped, and
   #   worth reading there too: the grouping key is workspace_id, and two
   #   workspaces can point at one checkout.
start_session(task_id?, type: "claude-code"|"codex", label, prompt?,
              allow_shared_workspace=false)
   -> {session_uid, worktree_path, task_id?, prompt_source,
       shared_workspace?, warning?, workspace_shared_with?}
   # task_id omitted = "spawn in caller's workspace, no task binding"
   #   (the only valid form for a taskless caller).
   # task_id provided = bind the new session to that task; only allowed
   #   if the caller has authority over it (own task or descendant).
   # A named task with NO workspace gets one MINTED: see
   #   "Workspace-less tasks" below. allow_shared_workspace=true opts
   #   back into spawning the worker in the CALLER's checkout.
   # worktree_path = the checkout the child actually landed in. Worth
   #   reading even when you passed no task_id: binding to a branch-mode
   #   subtask spawns the child in ITS worktree, not the caller's.
   # task_id in the RESPONSE = the task the child ended up bound to
   #   (absent when it isn't bound to one).
   # prompt_source in {"caller", "task", "none"}: an empty/absent prompt
   #   on a spawn that names a task EXPLICITLY (the task_id arg, or the
   #   task isolated=true mints) auto-delivers that task's stored
   #   description+prompt — the same text the operator's launch key
   #   sends — so start_session(task_id=<backlog task>) is a complete
   #   launch. A merely INHERITED binding never triggers it: a
   #   promptless spawn without task_id stays promptless (the classic
   #   spawn-now-drive-later pattern). Best-effort: no stored prompt, or
   #   an unreachable planning API, degrades to a promptless spawn and
   #   never fails the spawn.
   #   Agent session types only: a bash "prompt" is a command line the
   #   shell would EXECUTE, so bash never auto-delivers. The
   #   auto-registered done-monitor treats an auto-delivered task prompt
   #   exactly like a caller-supplied one.
send_input(session_uid, text, submit=true)
   -> {ok}
read_session_output(session_uid, since_cursor?, max_messages=20)
   -> {messages: [...], cursor, generation, state, idle, status}
   # Also carries the outcome fields the resolver knows — reported_done /
   #   reported_done_at / report_reason, and killed / killed_by /
   #   exited_at once the session has ended. Read them before trusting
   #   the text: they answer "is this a conclusion, or wherever the
   #   process happened to stop?". Same on read_last_turn.
   # generation echoes the resolver's value; state surfaces "pending"/"exited"
   # so the caller can decide to poll or move on. Cursors include the
   # generation they were issued under; on mismatch, the parser silently
   # restarts from the beginning of the new transcript and the response
   # carries the new generation.
kill_session(session_uid) -> {ok}
   # Provenance is recorded: the exit tombstone gets killed=true and
   #   killed_by — a who-OR-WHAT, rendered verbatim and never parsed:
   #     <session uid>                    an agent's kill_session
   #     "operator"                       the operator routes (the TUI's
   #                                      A-w, resolve_stuck, the
   #                                      continuous scheduler watchdog)
   #     "memory-cap"                     the memory cap's cgroup/OOM
   #                                      SIGKILL, which reached the
   #                                      tombstone as a plain exit
   #                                      before 4a and so had its
   #                                      truncated tail read as a report
   #     "<uid> (mark_subtask_done)"      the session sweep run when a
   #                                      subtask is marked done
   #   killed can still be true with killed_by null for any kill path
   #   that records no request. Downstream readers use it so a killed
   #   session's truncated transcript tail is never presented as its
   #   final report: exited list_sessions rows carry the fields, and the
   #   async monitors' fire message reads "killed by <who> at <ts>" with
   #   any transcript text labelled a fragment.
   # Killing a session that is already gone is a no-op, not a mystery:
   #   it errors `not_found` with "session '<uid>' already exited at
   #   <ts>" (plus the killer when known) instead of a message that
   #   reads like a bad uid.
```

```
report_done(reason?) -> {ok, reported, reported_at, status, done, task_id}
   # The caller's own "my work is finished" signal — no session_uid arg;
   #   the daemon resolves the caller from its own session. Two effects,
   #   applied per the caller's identity:
   #     1. Session-scoped (every Session caller): stamps the mark behind
   #        status="reported" and fires any until="final" monitor
   #        watching it. Superseded automatically by new input, so a
   #        worker handed follow-up work reports again when THAT is done.
   #     2. Continuous-run (a continuous-task tick only): also flips the
   #        active run Running -> Done. `done` / `task_id` describe THAT
   #        flip and are false / null for everyone else.
   # Widened rather than split into a second RPC: the two effects are one
   #   fact — "the agent is finished" — with two consumers, and two verbs
   #   would fail silently on a wrong guess (the continuous one used to
   #   answer Unauthorized to plain workers; a plain one called from a
   #   tick would leave the run Running until the wedge watchdog closed
   #   it). Wire-compatible: `done` / `task_id` / `message` keep their
   #   meanings and the new fields are additive.

monitor_sessions(session_uids, mode="any", until="turn_end", note?,
                 timeout_s=1800, edge=true)
   -> {monitor_id, watching, mode, until, already_idle, already_reported,
       async_note}
   # Two independent axes. `mode` = how many must finish (any | all).
   #   `until` = what finished MEANS per session:
   #     "turn_end"  the next completed turn (the default).
   #     "final"     only an exit or an explicit report_done. Interim
   #                 turn ends RE-ARM the watch against the turn that
   #                 just ended and are counted onto the eventual entry
   #                 as interim_turn_ends. This is the fix for a worker
   #                 that ends its launch turn with background subagents
   #                 still running: one watch, one wake-up, when it is
   #                 genuinely done.
   #   mode="final" is accepted as sugar for mode="any", until="final",
   #   and until="task_done" as a synonym for "final".
   # `already_reported` is the final-mode twin of `already_idle`: those
   #   sessions had reported BEFORE the watch armed, so that (anchored)
   #   report will not fire it — read them directly instead.
   # A session that never calls report_done — including any bash
   #   session, which cannot — completes a final watch only by exiting,
   #   so pair it with a timeout_s you are willing to wait.
   # start_session / send_input take notify_until with the same values
   #   for the monitor they auto-register.
```

`type: "bash"` is **not** offered in `start_session`. Bash sessions remain user-only — they have no transcript and no `Agent` impl.

`read_session_output` is split into one socket call per invocation plus direct file IO:

1. Resolve: `resolve_authorized_session(session_uid) -> {state, engine, transcript_path, generation}` where `state` is one of `"ready" | "pending" | "exited"` and `transcript_path` is `null` when `state == "pending"`. See the Architecture section for the full contract.
2. Read: if `state == "ready"` or `"exited"`, the MCP process reads the transcript file directly using the Python parser (see "Cross-language transcript parsers" below). If `state == "pending"`, the MCP returns `messages=[]` to the agent and surfaces the `state` so the agent can poll again.

Re-resolving every call (rather than caching) handles `/clear`-driven transcript rebinds correctly: when a target runs `/clear`, the TUI binds it to a new JSONL file and bumps `generation`. The next `read_session_output` resolves to the new path automatically. Callers that want to cache across a burst of reads can compare `generation` to detect staleness, but the default code path doesn't bother — the resolve call is cheaper than parsing even a few JSONL lines.

This preserves the descendant-only authorization model (every read clears auth first) while keeping the actual parsing off the TUI main loop.

---

## Phase 4 — Workflow MCP tools

Reuses the existing workflow infrastructure (`~/.cm/workflow-runs/...`) but adds *external* control. The existing `workflow_transition` and `workflow_done` tools (called by participants) remain as-is.

```
start_workflow(task_id, workflow_name, role_overrides?) -> {run_id}
stop_workflow(run_id) -> {ok}
get_workflow_state(run_id) -> {active_role, history, role_sessions, ...}
list_workflows(task_id?) -> [{run_id, name, active_role, ...}]
```

`start_workflow` spawns the workflow's participant sessions on the named task. The caller is **not** a participant — it's the orchestrator, sibling to the participants. Activity feed records every transition the caller's workflows produce.

---

## Phase 5 — Subtask MCP tools + worktree branching

```
create_subtask(name, prompt, worktree_mode="inherit"|"branch", project?, base?)
   -> {task_id, worktree_path, base_sha, launched: false}
   # base = explicit committish the new branch is cut from, REPLACING the
   #   parent-wip-branch default — use it to fork off clean upstream
   #   instead of inheriting the parent's in-progress work. Anything git
   #   resolves: sha, tag, local branch ("main"), remote-tracking ref
   #   ("origin/main"); resolved locally first, with one
   #   `git fetch origin <base>` if it doesn't resolve yet.
   #   worktree_mode="branch" ONLY — inherit / in-place cut no branch, so
   #   passing it there is invalid_params ("base only valid with
   #   worktree_mode=branch"). An unresolvable base is invalid_params
   #   ("base '<x>' does not resolve to a commit") raised BEFORE the task
   #   row is created, so a typo'd ref can't leave a half-made subtask to
   #   roll back.
   # base_sha = the commit the new checkout actually sits on (the
   #   resolved base or the parent branch's tip in branch mode, the
   #   shared checkout's HEAD otherwise; null when the path isn't
   #   readable as a git checkout).
   # launched is ALWAYS false: create_subtask mints a task and (in
   #   branch mode) a worktree, then stops. Nothing runs until
   #   start_session(task_id=<task_id>).
list_subtasks(task_id?) -> [{task_id, name, status, worktree_mode, ...}]
mark_subtask_done(task_id, close_worktree=true)
   -> {ok}
```

`mark_subtask_done` does NOT auto-merge. The agent is expected to run `git merge` itself (or rebase, or whatever) inside the parent worktree *before* marking done.

The cleanup behavior is **a new code path** — neither of the existing close paths is right as-is:

- `close_active_workspace` (`tui/src/app.rs:1241`) is a *soft* close: it kills the session PTYs and hides the workspace from the sidebar but leaves the worktree on disk. Too gentle.
- The workspace-delete path (`tui/src/app.rs:3935`) removes the worktree *and* deletes the local branch (`git branch -D`) and the remote branch (`git push origin --delete`). Too aggressive — we just merged the branch and want to keep its ref for history/safety.

`mark_subtask_done(close_worktree=true)` needs a hybrid:

1. Soft-close the workspace (kill PTYs, mark closed in manifest) — reuse the soft-close logic.
2. Run `git worktree remove <path>` to take the worktree off disk — reuse `worktree::remove_worktree`.
3. **Skip** the `git branch -D` / `git push origin --delete` step. Branch ref stays. User can prune manually with `git branch -d cm-sub/<slug-chain>-<short_id>` (the same name shown in `git branch --list 'cm-sub/*'`) once they're confident.

`close_worktree=false` (the looser option) does step 1 only, leaving everything on disk so the user can come back to it.

This is intentionally minimal — agents that want fancier merge flows just shell out to git. The MCP surface stays narrow.

---

## Phase 6 — TUI visibility polish

### Sidebar

Widen by ~6 cells to make room for `[mgr: …]` markers and deeper indent.

### Activity feed

Bottom of the screen, single line, opt-in (off by default; toggle with `A-,`). Shows the last N agent-initiated mutations:

```
[14:32:11] worker → start_session(refactor-helpers, codex)
[14:32:14] worker → send_input(refactor-helpers, "extract …")
[14:33:02] manager → workflow_done(reviewer satisfied)
```

Backed by a ring buffer in app state. Every control-socket mutation appends an entry. Reads (`read_session_output`, `list_*`) don't appear — they're high-frequency and uninteresting.

---

## Phase 7 — Permission instructions

Add a section to project `CLAUDE.md` and to agent activation prompts delivered by workflow templates:

> You have tools that can spawn subtasks, start sessions, send input to other sessions, kill sessions, and start workflows. **Before using any of them, state your intent in plain language and ask the user to confirm.** The tools will not refuse you, but the user expects to stay in the loop. Apply the same convention to anything destructive (killing a session, marking a subtask done).

No hard gates. The activity feed is the "trust but verify" layer.

---

## Cross-language transcript parsers

The `Agent` trait's `read_messages` lives in Rust (used by the TUI for idle detection and workflow logic). The MCP server (Python) needs the same parsing for `read_session_output`. Two options:

1. **Port the parsers to Python.** Two small modules in `mcp_server/transcripts/{claude_code.py, codex.py}` mirroring the Rust impls. Adds maintenance cost (two implementations of one schema) but each is small and the schemas are stable.

2. **Route reads through the socket.** TUI does the parsing, returns normalized `Message` objects. Cleaner architecturally, but every `read_session_output` call now blocks on the TUI main loop, which is bad for an operation that may be called in a tight polling loop by an agent watching its subtask.

**Recommendation: port to Python** (option 1). The schemas are simple enough that the duplication is cheap, and keeping reads off the TUI main loop is worth it. Add a contract test: a fixture transcript run through both parsers must yield identical normalized output.

---

## Implementation plan (sequenced)

| Phase | Deliverable | Done when |
|---|---|---|
| 0 | `Agent` trait + Claude Code + Codex impls + ~30 unit tests + workflow code migrated to use the trait | All tests pass; workflows still work end-to-end (manual smoke test of feedback workflow) |
| 1 | Control socket server in TUI; Python client lib; **MCP config/env injection extended to all spawn paths** (regular `A-n`, planning `A-l`, workflow participants — see "Authorization model" for the full required env set) via the new shared `tui/src/mcp_config.rs` helper | (a) Round-trip a no-op `ping` request from MCP to TUI and back, AND (b) a regular non-workflow Claude session spawned via `A-n` can successfully call `ping` from its MCP context. Both must pass; (b) is the one that catches the "MCP only wired up for workflows" foot-gun. |
| 2 | (a) Persist `uid`, `managed_by_uid`, `generation` on `ManifestEntry`; backfill rule for older manifests. (The `session_id` → `transcript_id` rename already happened in Phase 0 — the trait was written against the post-rename field.) (b) Add `Workspace.tombstones: Vec<SessionTombstone>` and persist alongside live entries; change close paths to extract tombstone-then-drop-live (the four-step close from "Closing is workspace-level"). (c) `parent_task_id` + `worktree_mode` in task schema; planning-view tree with collapse/expand; sessions-view focus mode. | (a) Restart the TUI: live sessions retain their UIDs, generation persists across `/clear`. (b) Close a session: a tombstone appears in the manifest with the right metadata; the live row is gone. (c) Hand-insert a subtask via SQL/CLI; both planning view and sessions-view focus mode render it correctly. **All three must pass before Phase 3** — Phase 3's resolver and `kill_session` rely on (a) and (b). |
| 3 | Session-management MCP tools; Python transcript parsers; contract test | Agent can `start_session`, `send_input`, `read_session_output` against a sibling session in a manual repro |
| 4 | Workflow MCP tools | Agent can launch a feedback workflow on its own task and observe transitions |
| 5 | Subtask MCP tools; worktree-branch creation | Agent can create a `branch`-mode subtask, work in it, mark done, and see the worktree cleaned up |
| 6 | Sidebar markers, focus mode polish, activity feed | All agent-initiated mutations visible in the feed |
| 7 | CLAUDE.md additions; activation-prompt template additions | Agent visibly asks before acting in the manual smoke tests |

Each phase ends with a manual smoke test in the TUI before moving on.

---

## Human checkpoints

Implementation runs unattended between checkpoints. The agent should still stop and self-test between every phase, but the user only verifies at three points:

1. **End of Phase 0 — trait works.** `cargo test` green on the agent suite. Manual smoke test: feedback workflow runs end-to-end after the workflow code is migrated to use `Agent::submit_prompt` for prompt delivery and `Agent::assistant_turn_completed_since(ctx, baseline)` (NOT raw `is_idle`) as the static-transition gate, with the activation baseline still snapshotted in `app.rs`. The smoke test should specifically include a transition that would have fired on stale idle output under the wrong gate — verify it now waits correctly. If anything is flaky here, do not proceed — every later phase relies on this layer being correct.

2. **End of Phase 3 — first end-to-end agent control.** This is the single most informative mid-implementation checkpoint: it exercises the Agent trait, the control socket, authorization, the Python transcript parser, and the contract test all at once. Phases 4 and 5 add more verbs on the same plumbing, so if Phase 3 is solid they are largely incremental. If the architecture is wrong, this is where it shows up — before Phase 5's git/worktree complexity is built on top of it.

   **The single test**: ask agent A to start agent B as a sibling via `start_session`, send a prompt to B via `send_input`, read B's output via `read_session_output`, kill B via `kill_session`. If that round-trips cleanly, the foundation is proven and the remaining phases can ride to completion.

3. **End of full implementation.** Drive an agent through the full stack: create a subtask in `branch` mode, have it spawn sessions, run a workflow inside it, merge back, mark done. Activity feed should show every mutation. Sidebar markers (`↳`, `[mgr: …]`) should render correctly under focus mode.

---

## Open questions / risks

1. ~~**Two MCP server copies.**~~ *Resolved.* The predictionTrading copy (`scripts/mcp/claude_manager_server.py`) is retired — that repo's `scripts/mcp/claude-manager.sh` now execs this repo's `server.py` through the generated `~/.cm/mcp/launcher.sh`, which tracks the active claude-manager checkout/venv. Tool changes land in `mcp_server/server.py` only.
2. **Concurrent edits in `inherit` mode.** Two sibling sessions in the same worktree both running `cargo build` will fight. Document this as a user concern; don't solve in v1.
3. **Activity feed in the activation prompt context.** When an agent resumes after a transition, the activity feed records the transition — but the agent doesn't see it. Is that OK? Probably yes; the feed is for the user, not the agents.
4. **Worktree cleanup races.** If a subtask agent is mid-write when the user kills the parent, child sessions may be killed mid-edit. Existing worktree-close logic already handles this; reuse it.
5. **Recursive subtask depth in the activity feed.** If a grandchild does something, label it `<grandparent> → <parent> → <child>` or just the direct caller? Direct caller only — keeps the feed readable; tree view in sidebar already shows lineage.
6. **Cursor format stability.** The `Cursor` struct's `raw` is opaque but persisted by agent calls (they pass it back). If we change the parser internals, old cursors break. Versioning the cursor (`v1:...`) is cheap insurance.

---

## What's NOT in scope

- A web UI for any of this. TUI only.
- Cross-machine agent control. Single-host only via Unix socket.
- Hard permission enforcement. Convention only, by design.
- Bash session spawning via MCP. Bash stays user-only.
- Auto-merging subtask branches. Agents do their own git.
- Agent control of unrelated tasks. Descendants only.
