"""MCP server for Claude instances to propose tasks to the backlog."""

import asyncio
import os
import sys
import time

# Add project root to path so cli.planning_client is importable
sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from mcp.server.fastmcp import FastMCP

try:
    from cli.planning_client import PlanningClient
except ModuleNotFoundError:
    # Headless/remote deployment (e.g. cm-manager: only `mcp_server/` is
    # deployed under /opt/cm-daemon, so the repo `cli/` package isn't on the
    # path). The PLANNING tools (propose_task, list_projects, ...) need
    # PlanningClient; the WORKFLOW control tools (workflow_transition /
    # workflow_done) and session tools do NOT. A top-level crash here used to
    # take down the ENTIRE MCP server — so a headless workflow's manager role
    # could never call workflow_done and every run wedged. Degrade instead:
    # let the server start (workflow/session tools live), and surface a clear
    # error only if a planning tool is actually invoked.
    class PlanningClient:  # type: ignore[no-redef]
        def __init__(self, *_a, **_k):
            raise RuntimeError(
                "planning tools are unavailable in this deployment: the `cli` "
                "package is not installed alongside the MCP server "
                "(headless/remote host). Workflow and session tools are unaffected."
            )

from mcp_server import control_client
from mcp_server.transcripts.types import Role
# Session status + transcript-tail helpers and the multi-session monitor
# core live in `monitor` (FastMCP-free, so the cm-wait CLI can reuse them
# without pulling in the MCP server's deps). Imported into this namespace
# so the tools below — and `from mcp_server.server import _session_status`
# in the tests — keep working unchanged.
from mcp_server.monitor import (
    _last_assistant,
    _monitor_sessions,
    _parser_for,
    _read_all_messages,
    _READ_ALL_LIMIT,
    _session_status,
)

mcp = FastMCP("claude-manager")

# 11g-2 (A2): the `_append_event` direct file-write helper and the
# `_workflow_run_dir` accessor have been retired. Pre-11g-2 the
# workflow tools fell back to writing `events.jsonl` directly when
# the daemon socket wasn't pinned (the TuiLocal SpawnTarget path).
# Since 10f the daemon has been mandatory and TUI launches always
# pin `CM_DAEMON_SOCKET`, making the fallback unreachable.
#
# Post-11g-2 the TUI's controller no longer reads events.jsonl at
# all (channel-only path via `events.subscribe`), so a stranded
# file-only write would be invisible to the controller. Removing
# the fallback closes the failure mode entirely.
#
# `_require_workflow_env` (below) absorbed `_workflow_run_dir`'s
# operator-friendly error message about the per-session MCP config
# (folded in during the Phase 2 merge).


_BRIEF_FIELDS = (
    "id", "slug", "project", "name", "status", "source",
    "priority", "difficulty", "is_cloud", "kind",
)
_FULL_EXTRA_FIELDS = (
    "description", "prompt", "depends", "repo_url", "repo_branch",
    "wip_branch", "session_id", "ttyd_url", "worker_vm",
    "blocked_at", "created_at", "updated_at",
    "parent_task_id", "worktree_mode", "metadata",
)


def _shape_task(task: dict, *, full: bool) -> dict:
    """Project an API task dict to a stable shape for MCP responses.

    `source` is always present — "user" for tasks created by the human owner
    in the TUI, "claude" for tasks proposed by an agent. Agents should check
    this to distinguish the two.
    """
    out = {k: task.get(k) for k in _BRIEF_FIELDS}
    if full:
        for k in _FULL_EXTRA_FIELDS:
            out[k] = task.get(k)
    return out


# Substrings that indicate the caller confused the tool-call wrapper syntax
# (`<parameter name="X">VALUE</parameter>`) with the task model fields, and
# stuffed nested tag-shaped content into a single string parameter. Caught at
# write time, BEFORE the bad row hits the planning queue and someone launches
# it. See the discussion at <2026-05-09 propose_task miswrite>.
_PARAMETER_CONFUSION_MARKERS = (
    "<prompt>",
    "</prompt>",
    "<description>",
    "</description>",
    "<difficulty>",
    "</difficulty>",
    "</invoke>",
    "<parameter name=",
)


def _check_parameter_confusion(field_name: str, value: str) -> None:
    """Raise ValueError if `value` looks like the caller serialized other
    parameters as nested XML inside this string. The most common shape:
    `description` ends with literal `</description><prompt>...</prompt>`
    because the caller forgot that `prompt` is a separate top-level
    parameter, not a nested child."""
    if not value:
        return
    for marker in _PARAMETER_CONFUSION_MARKERS:
        if marker in value:
            raise ValueError(
                f"propose_task: `{field_name}` contains literal '{marker}'. "
                f"`description`, `prompt`, and `difficulty` are SEPARATE "
                f"top-level parameters of this tool, not nested fields. "
                f"Pass each as its own parameter; do not embed XML-style "
                f"tags inside one string. Re-call the tool with the fields "
                f"split out."
            )


@mcp.tool()
def propose_task(
    project: str,
    name: str,
    description: str = "",
    prompt: str = "",
    difficulty: int | None = None,
    depends: list[str] | None = None,
) -> str:
    """Propose a new task to the project backlog.

    The task is created with source='claude' in draft status.
    The project owner will review and accept or reject it in the TUI.

    `description` and `prompt` serve different purposes — set BOTH:
      - `description`: what the user sees in the planning queue when
        deciding whether to accept the task. Context, motivation,
        background. The user reads this.
      - `prompt`: the launch instructions the worker agent receives when
        the task is accepted and launched. Concrete steps, files to
        touch, constraints. The agent reads this.
    They are SEPARATE top-level parameters. Do NOT serialize one inside
    the other — calls that contain literal `<prompt>`, `</prompt>`,
    `<description>`, etc. inside a string field will be rejected.

    Args:
        project: Project name (use list_projects to see valid names)
        name: Short task title
        description: Background/motivation shown to the user in the queue
        prompt: Launch instructions delivered to the worker agent
        difficulty: Optional difficulty rating (1-10)
        depends: Optional list of task slugs this depends on
    """
    _check_parameter_confusion("description", description)
    _check_parameter_confusion("prompt", prompt)
    _check_parameter_confusion("name", name)
    # Sub-2b-2 (10d-mcp-surface-2b-2): route through the daemon
    # when CM_DAEMON_SOCKET is set. Daemon now owns the planning
    # API talk (centralizes auth/audit/policy; survives TUI
    # restarts). Falls back to the direct PlanningClient path
    # when no daemon socket is configured — ad-hoc CLI usage,
    # tests, agents not spawned through the TUI.
    #
    # Wire shape: daemon's propose_task expects `repo_url`
    # explicitly (the daemon doesn't know the agent's cwd, so
    # it can't auto-detect via `git remote get-url origin` the
    # way the local PlanningClient does). We detect here and
    # forward.
    #
    # Sub-2c review-2: single-decision binding (restoring the
    # round-8 #2 fix). `resolve_socket_route()` is called ONCE
    # and its `path` is passed through to `call(socket_path=)`,
    # so the daemon-vs-PlanningClient branch decision AND the
    # actual dial are bound to the same resolution.
    route = control_client.resolve_socket_route()
    if route.chose_daemon:
        try:
            from cli.planning_client import _detect_repo_url
        except ModuleNotFoundError:
            return (
                "propose_task unavailable: the `cli` package is not installed "
                "alongside the MCP server (headless/remote host)."
            )
        try:
            repo_url = _detect_repo_url()
        except (RuntimeError, OSError) as e:
            # _detect_repo_url shells out to `git remote get-url origin`; it
            # raises RuntimeError when there's no origin remote and OSError
            # (FileNotFoundError) when git is absent. Return a clear message
            # instead of crashing the tool with an opaque traceback when the
            # agent's cwd isn't a git repo with an origin.
            return (
                f"propose_task: could not detect the repo URL from the current "
                f"directory (no git 'origin' remote?): {e}"
            )
        params = {
            "project": project,
            "name": name,
            "description": description,
            "prompt": prompt,
            "repo_url": repo_url,
        }
        if difficulty is not None:
            params["difficulty"] = difficulty
        if depends:
            params["depends"] = list(depends)
        try:
            task = control_client.call("propose_task", params, socket_path=route.path)
        except control_client.ControlError as e:
            # Surface the daemon's error message + code to the
            # agent so it sees what the planning queue rejected.
            return f"propose_task failed ({e.code}): {e.message}"
    else:
        client = PlanningClient()
        task = client.propose_task(
            project=project,
            name=name,
            description=description,
            prompt=prompt,
            difficulty=difficulty,
            depends=depends,
        )
    # Inline round-trip preview so the caller sees what actually landed
    # without an extra get_task. Truncated to keep the response readable.
    desc_preview = (description or "")[:140]
    prompt_preview = (prompt or "")[:140]
    return (
        f"Proposed task '{name}' in project '{project}' (id: {task['id']})\n"
        f"  description ({len(description)} chars): {desc_preview}{'...' if len(description) > 140 else ''}\n"
        f"  prompt ({len(prompt)} chars): {prompt_preview}{'...' if len(prompt) > 140 else ''}"
    )


@mcp.tool()
def list_projects() -> list[dict]:
    """List all available projects and their repo URLs.

    Use this to discover valid project names before calling propose_task.
    """
    client = PlanningClient()
    return client.list_projects()


@mcp.tool()
def list_tasks(
    project: str | None = None,
    status: str | None = None,
    source: str | None = None,
) -> list[dict]:
    """List tasks across the planning system.

    Returns BOTH human-created and agent-proposed tasks by default. Each
    entry exposes a `source` field:
      - "user"   — created by the human owner in the TUI
      - "claude" — proposed by a Claude agent via `propose_task`

    Inspect that field to tell them apart, and prefer leaving user tasks
    untouched unless the user explicitly asked you to modify them.

    Args:
        project: Optional project name filter.
        status: Optional status filter ("draft", "backlog", "running",
            "blocked", "done", "archived").
        source: Optional source filter ("user" or "claude"). Default returns both.
    """
    route = control_client.resolve_socket_route()
    if route.chose_daemon:
        # Headless host: the daemon serves the read (it holds the planning-API
        # creds). It returns ALL raw rows — the API `GET /tasks` has no
        # project/status query — so filter client-side here.
        tasks = control_client.call("list_tasks", {}, socket_path=route.path)
        if project:
            tasks = [t for t in tasks if t.get("project") == project]
        if status:
            tasks = [t for t in tasks if t.get("status") == status]
    else:
        client = PlanningClient()
        tasks = client.list_tasks(project=project, status=status)
    if source:
        tasks = [t for t in tasks if t.get("source") == source]
    return [_shape_task(t, full=False) for t in tasks]


@mcp.tool()
def get_task(task_id: str) -> dict:
    """Get full details of a single task by its UUID.

    The returned object includes a `source` field — "user" for tasks
    created by the human owner, "claude" for agent-proposed tasks.

    Args:
        task_id: Task UUID (find one via `list_tasks`).
    """
    route = control_client.resolve_socket_route()
    if route.chose_daemon:
        task = control_client.call("get_task", {"task_id": task_id}, socket_path=route.path)
    else:
        task = PlanningClient().get_task(task_id)
    return _shape_task(task, full=True)


@mcp.tool()
def update_task(
    task_id: str,
    name: str | None = None,
    description: str | None = None,
    prompt: str | None = None,
    status: str | None = None,
    priority: int | None = None,
    difficulty: int | None = None,
    project: str | None = None,
    depends: list[str] | None = None,
    parent_task_id: str | None = None,
    metadata: dict | None = None,
) -> dict:
    """Edit a task's planning fields.

    This tool can modify ANY task — including tasks created by the human
    owner (source="user"). That power cuts both ways: be conservative when
    touching user tasks. Don't rewrite their prompt, change scope, or move
    them between statuses unless the user explicitly asked you to. Agent-
    proposed tasks (source="claude") are fair game for self-revision.

    The returned object includes the `source` field so you can confirm
    what you just modified.

    Only the fields you pass (non-None) will be updated. To clear
    `parent_task_id` (promote a subtask back to top-level), pass the
    literal string "null".

    Args:
        task_id: Task UUID.
        name: New title.
        description: New description.
        prompt: New launch prompt.
        status: "draft", "backlog", "running", "blocked", "done", or "archived".
        priority: Lower number = higher priority.
        difficulty: 1-10.
        project: Reassign to a different project.
        depends: Replace the dependency list (task slugs).
        parent_task_id: Reparent this task under another task's UUID,
            or "null" to detach (make it top-level).
        metadata: Free-form JSONB bag for skill/agent attachments. PATCH
            REPLACES the whole object — read the existing `metadata` via
            `get_current_task` (or `get_task`) first and re-send the
            merged dict if you want to preserve other keys. The
            design-doc skills nest under a `resume` key
            (`metadata.resume.design_doc_path`,
            `metadata.resume.designer_session_uid`); other skills should
            namespace their own keys here rather than churn the schema.
    """
    fields: dict = {
        k: v for k, v in {
            "name": name,
            "description": description,
            "prompt": prompt,
            "status": status,
            "priority": priority,
            "difficulty": difficulty,
            "project": project,
            "depends": depends,
            "metadata": metadata,
        }.items() if v is not None
    }
    if parent_task_id is not None:
        fields["parent_task_id"] = None if parent_task_id == "null" else parent_task_id
    if not fields:
        raise ValueError("No fields to update — pass at least one field.")
    try:
        client = PlanningClient()
    except Exception as planning_unavailable:
        # Headless deployment (e.g. a daemon-spawned agent on cm-manager):
        # the cli-routed PlanningClient is unavailable — the `cli` package
        # isn't deployed alongside the MCP server, and/or CM_API_URL /
        # CM_API_TOKEN aren't in the agent's env. A STATUS-ONLY update can
        # still go through the daemon-routed `set_subtask_status` (the daemon
        # holds the planning creds). Any OTHER field has no headless path, so
        # surface the original error. This lets a bug-fix subtask agent flag
        # `blocked` (fix-ready) via its natural `update_task(status=...)` call
        # without the orchestrator prompt needing to know the daemon routing.
        if list(fields.keys()) == ["status"]:
            return control_client.call(
                "set_subtask_status",
                {"status": fields["status"], "task_id": task_id},
            )
        raise planning_unavailable
    return _shape_task(client.update_task(task_id, **fields), full=True)


@mcp.tool()
def get_current_task() -> dict:
    """Return the task this session is bound to, plus its full
    metadata bag.

    Resolves your caller UID against the TUI's session→workspace→task
    bindings and then fetches the live task row from the API. Skills use
    this to discover the doc path / designer session a task is bundled
    with (the design-doc bundle nests under `metadata.resume.*`) without
    the user having to pass them in.

    Returns:
        {
          "task": <full task dict>, or null when the caller has no
            bound task (e.g. an `A-n` taskless session),
          "workspace_id": <workspace UUID or null>,
          "is_tombstone": <bool — true when the caller session has been
            closed; the task lookup still works during the 30-day
            retention window>,
        }

    The `task` dict carries the same shape as `get_task` plus a
    `metadata` field (JSONB bag, or null). To update fields on it, call
    `update_task` with the returned `task.id` — note that `metadata` is
    REPLACED on PATCH, so merge first if you want to preserve other keys.
    """
    route = control_client.resolve_socket_route()
    if route.chose_daemon:
        # Headless host: the cli-routed `get_caller_task` + PlanningClient path
        # isn't available. `ping` already returns the caller's task_id +
        # workspace_id; fetch the row via the daemon-served `get_task`.
        # `is_tombstone` isn't surfaced headless — a live caller is False.
        pong = control_client.call("ping", {}, socket_path=route.path)
        task_id = pong.get("task_id")
        workspace_id = pong.get("workspace_id")
        if not task_id:
            return {"task": None, "workspace_id": workspace_id, "is_tombstone": False}
        task = control_client.call("get_task", {"task_id": task_id}, socket_path=route.path)
        return {
            "task": _shape_task(task, full=True),
            "workspace_id": workspace_id,
            "is_tombstone": False,
        }
    ctx = control_client.call("get_caller_task")
    task_id = ctx.get("task_id")
    if not task_id:
        return {
            "task": None,
            "workspace_id": ctx.get("workspace_id"),
            "is_tombstone": bool(ctx.get("is_tombstone")),
        }
    client = PlanningClient()
    task = client.get_task(task_id)
    return {
        "task": _shape_task(task, full=True),
        "workspace_id": ctx.get("workspace_id"),
        "is_tombstone": bool(ctx.get("is_tombstone")),
    }


@mcp.tool()
def ping() -> dict:
    """Check connectivity AND learn who you are: your own identity, scope,
    and permissions.

    Returns the host's pong response:
        {pong, uid, caller_kind,
         global_perms: bool,        # ← do you have global permissions?
         task_id: str | null,       # ← the task you're bound to (if any)
         workspace_id: str | null}  # ← the workspace you live in

    `global_perms` is the one to check before you try to drive another
    task's sessions: when true, you can prompt / read / kill / spawn
    against ANY session anywhere (see `list_sessions`), not just your own
    task tree. When false you're scoped to your task tree (or, if you
    have no task, your workspace).

    Also a smoke test that the agent's MCP env was wired up correctly
    (CM_TUI_SESSION_ID + CM_DAEMON_SOCKET/CM_TUI_SOCKET must be set by
    the host at spawn time).
    """
    try:
        return control_client.call("ping")
    except control_client.ControlError as e:
        return {"ok": False, "error": str(e)}
    except control_client.TransportError as e:
        return {"ok": False, "transport_error": str(e)}


# `_session_status`, `_parser_for`, `_read_all_messages`, `_last_assistant`,
# and `_READ_ALL_LIMIT` are imported from `mcp_server.monitor` (top of file).


def _list_sessions_raw(task_id: str | None, include_exited: bool) -> list[dict]:
    """Shared implementation behind `list_sessions` and
    `list_sessions_grouped`: call the host, then enrich each entry with
    the legible `status` word."""
    params: dict = {"include_exited": include_exited}
    if task_id:
        params["task_id"] = task_id
    sessions = control_client.call("list_sessions", params)
    for s in sessions:
        s["status"] = _session_status(
            s.get("state", "pending"), bool(s.get("idle", False))
        )
    return sessions


@mcp.tool()
def list_sessions(
    task_id: str | None = None,
    include_exited: bool = False,
) -> list[dict]:
    """List sessions visible to you.

    Scope: your own task tree (or your whole workspace if you have no
    task). If you hold global permissions (check `ping().global_perms`),
    this returns EVERY session across all tasks and workspaces. For a
    readable tree grouped by workspace → task, use
    `list_sessions_grouped` instead.

    Args:
        task_id: Optional explicit task scope. Leave unset for the default
            (your task tree, or whole workspace if taskless; everything if
            you're global). A non-global taskless caller passing `task_id`
            gets `unauthorized`.
        include_exited: When true, also include closed sessions
            (state=`exited`). Default false. Read-after-exit still
            works via `read_session_output` regardless of this flag.

    Returns: list of per-session dicts:
        {session_uid, label, type, state, idle, status, managed_by_uid,
         task_id, workspace_id, workflow_run_id, workflow_role,
         global_perms}
        state ∈ {"ready", "pending", "exited"}.
        status ∈ {"starting", "working", "awaiting_input", "exited"} —
            the legible summary of (state, idle); branch on this instead
            of decoding the pair yourself. See the status legend on
            `wait_for_session_idle`.
        managed_by_uid: the session that spawned this one (None if
            operator-spawned) — the parent link for orchestration.
        task_id / workspace_id: grouping keys (see `list_sessions_grouped`).
        global_perms: true if THIS session is a privileged orchestrator.
    """
    return _list_sessions_raw(task_id, include_exited)


@mcp.tool()
def list_sessions_grouped(
    task_id: str | None = None,
    include_exited: bool = False,
) -> dict:
    """List visible sessions as a tree grouped by workspace → task — the
    readable companion to `list_sessions`.

    Use this when you're orchestrating across more than one task or
    workspace (the common case once you hold global permissions) and want
    the structure handed to you instead of bucketing a flat list yourself.

    Args: same as `list_sessions`.

    Returns:
        {
          "you": {                      # your own identity + scope
             "session_uid", "task_id", "workspace_id", "global_perms"
          },
          "total": <int>,               # session count across all groups
          "workspaces": [
             {
               "workspace_id": str | null,
               "tasks": [
                  {
                    "task_id": str | null,   # null = taskless sessions
                    "sessions": [ <same per-session dicts as list_sessions> ]
                  }, ...
               ]
             }, ...
          ]
        }

    Sessions with no workspace_id (e.g. some host-owned snapshots) land
    under a `workspace_id: null` group; taskless sessions under a
    `task_id: null` task within their workspace.
    """
    sessions = _list_sessions_raw(task_id, include_exited)

    # Self-context so the agent can see at a glance whether it's
    # privileged and where it sits — saves a separate ping().
    try:
        me = control_client.call("ping")
        you = {
            "session_uid": me.get("uid"),
            "task_id": me.get("task_id"),
            "workspace_id": me.get("workspace_id"),
            "global_perms": bool(me.get("global_perms", False)),
        }
    except (control_client.ControlError, control_client.TransportError):
        you = None

    # Group by workspace_id, then task_id, preserving first-seen order so
    # the output is stable across calls.
    ws_order: list[str | None] = []
    by_ws: dict[str | None, dict[str | None, list[dict]]] = {}
    for s in sessions:
        ws = s.get("workspace_id")
        tid = s.get("task_id")
        if ws not in by_ws:
            by_ws[ws] = {}
            ws_order.append(ws)
        task_buckets = by_ws[ws]
        task_buckets.setdefault(tid, []).append(s)

    workspaces = []
    for ws in ws_order:
        tasks = [
            {"task_id": tid, "sessions": sess}
            for tid, sess in by_ws[ws].items()
        ]
        workspaces.append({"workspace_id": ws, "tasks": tasks})

    return {"you": you, "total": len(sessions), "workspaces": workspaces}


@mcp.tool()
def start_session(
    type: str,
    label: str,
    prompt: str = "",
    task_id: str | None = None,
    global_perms: bool = False,
) -> dict:
    """Spawn a new agent or shell session in your workspace.

    Args:
        type: "claude-code", "codex", or "bash". A bash session is a raw
            shell in the workspace's worktree — useful when you want to
            drive a terminal and have the user share it. No MCP
            injection, no transcript, but `send_input` and
            `wait_for_session_idle` still work (writes raw bytes + Enter
            to the PTY; idle flips via the same burst detector as
            agents).
        label: Sidebar label for the new session.
        prompt: Optional initial prompt to deliver once the session is
            ready (queued via the same PendingWrite drainer the workflow
            activation flow uses). For a bash session this is just a
            command line.
        task_id: Optional task to bind to. Omitted = your own task (if
            any) or no task. Cross-task binding is rejected — UNLESS you
            hold global permissions, in which case you may bind the child
            to any task.
        global_perms: Grant the new session global permissions (it can
            then prompt/read/control ANY session). **Only honored if YOU
            already hold global permissions** (check `ping().global_perms`);
            a non-global caller passing this gets `unauthorized`. This is
            how a privileged orchestrator spins up more privileged
            workers. Leave false for ordinary, scoped children.

    Returns: {"session_uid": "<uid>"} for the freshly-spawned session.

    State your intent in plain language and ask the user to confirm
    before calling this tool. The user expects to be in the loop on
    every spawn — doubly so when `global_perms=true`, which mints a child
    that can reach across the whole session graph.
    """
    params: dict = {"type": type, "label": label}
    if prompt:
        params["prompt"] = prompt
    if task_id:
        params["task_id"] = task_id
    if global_perms:
        params["global_perms"] = True
    # Sub-2c review-2: single-decision binding (restoring the
    # round-8 #2 fix). `resolve_socket_route()` is called ONCE
    # to get both the chosen path AND the route-chose-daemon
    # bool. Method is picked from the bool; the SAME path is
    # passed through to `call(socket_path=...)` so the dial
    # uses exactly the resolution that informed the method
    # choice.
    #
    # Pre-fix the method was picked from
    # `daemon_socket_pinned()` and `call()` independently re-
    # resolved — a daemon socket appearing or disappearing
    # between the two resolutions would route the wrong
    # method shape to the wrong server.
    route = control_client.resolve_socket_route()
    method = "mcp_start_session" if route.chose_daemon else "start_session"
    return control_client.call(method, params, socket_path=route.path)


@mcp.tool()
def send_input(session_uid: str, text: str, submit: bool = True) -> dict:
    """Deliver a prompt to a session you can see.

    If you want the agent's REPLY (the usual case), prefer
    `send_input_and_wait` — it sends, waits for the turn to finish, and
    returns the response in one call, without the post-send idle race.
    Use bare `send_input` for fire-and-forget or when you'll monitor a
    batch with `wait_for_any_session_idle`.

    Args:
        session_uid: Target session's stable UID (from list_sessions).
        text: Body to send.
        submit: True (default) appends Enter so the agent receives the
            input as a fresh keystroke; the body and Enter are separated
            in time by the TUI drainer to avoid the body being seen as
            a multi-line paste.

    State your intent and ask the user before calling this tool.
    """
    return control_client.call(
        "send_input",
        {"session_uid": session_uid, "text": text, "submit": submit},
    )


@mcp.tool()
def notify_user(message: str = "") -> dict:
    """Alert the user that you need their attention.

    Fires a desktop notification and makes the icon next to YOUR session
    blink in the TUI sidebar. The blink stops once the user selects your
    session. Use this when you're blocked on the user — a question, a
    decision, an approval, or "I'm done, come look" — and don't want to
    sit idle unnoticed.

    Args:
        message: Short reason shown in the notification (e.g. "need your
            decision on the migration approach"). Optional; if omitted the
            notification just says your session needs attention.

    This only ever pings the user about your own session, so — unlike the
    session-spawning / killing tools — you do NOT need to ask first. Just
    call it when you genuinely need the user.
    """
    return control_client.call("notify_user", {"message": message})


@mcp.tool()
def kill_session(session_uid: str) -> dict:
    """Close a session you can see. The PTY is torn down and a tombstone
    is recorded so `read_session_output` still works for the closed
    session's transcript.

    Ask the user before calling. Killing a session is destructive —
    pending work in that agent stops.
    """
    return control_client.call("kill_session", {"session_uid": session_uid})


@mcp.tool()
def read_session_output(
    session_uid: str,
    since_cursor: str | None = None,
    max_messages: int = 20,
) -> dict:
    """Read transcript messages from a session you can see.

    Two-step: first calls `resolve_authorized_session` to authorize and
    get the transcript path; then reads the file directly with the
    Python parser (no per-message round-trip through the TUI). Cursors
    encode the session's generation; mismatch (e.g. after `/clear`)
    silently restarts at offset 0 of the new transcript.

    Args:
        session_uid: Target session's stable UID.
        since_cursor: Cursor returned by a previous call, or None to
            start from the beginning of the current transcript.
        max_messages: Stop after this many messages. Default 20.

    To grab only the FINAL message of a long session, don't page all the
    way through with this — call `read_last_turn` instead (it reads the
    tail directly).

    Returns: {messages, cursor, generation, state, idle, status}.
        - messages: list of {role, content, ts}
        - cursor: opaque, pass back on next call
        - state: "ready" | "pending" | "exited"
        - status: "starting"|"working"|"awaiting_input"|"exited" — the
          legible summary of (state, idle)
        - When state="pending", messages is empty and you can poll again.
    """
    try:
        resolved = control_client.call(
            "resolve_authorized_session",
            {"session_uid": session_uid},
        )
    except control_client.ControlError as e:
        return {"error": e.code, "message": e.message}

    state = resolved.get("state", "pending")
    engine = resolved.get("engine", "claude-code")
    transcript_path = resolved.get("transcript_path")
    generation = int(resolved.get("generation", 0))
    idle = bool(resolved.get("idle", False))

    if state == "pending" or transcript_path is None:
        return {
            "messages": [],
            "cursor": None,
            "generation": generation,
            "state": state,
            "idle": idle,
            "status": _session_status(state, idle),
        }

    parser = _parser_for(engine)
    messages, cursor = parser.read_messages(
        transcript_path,
        generation,
        since_cursor,
        max_messages,
    )
    return {
        "messages": [m.to_dict() for m in messages],
        "cursor": cursor,
        "generation": generation,
        "state": state,
        "idle": idle,
        "status": _session_status(state, idle),
    }


@mcp.tool()
def read_last_turn(session_uid: str, context_messages: int = 6) -> dict:
    """Read the TAIL of a session's transcript — the fast way to see what
    an agent just said without paging through the whole thing.

    `read_session_output` pages FORWARD from the start, so fetching the
    final message of a long session means walking the entire transcript
    20 messages at a time (the trap where you read the first N and never
    reach the end). When all you want is "what did it conclude / what is
    it asking me?", call this: it returns the last assistant message
    directly, plus a little trailing context.

    Args:
        session_uid: Target session's stable UID.
        context_messages: How many trailing messages to include in
            `messages` for context (default 6, clamped to [0, 100]).
            `last_assistant` is the final assistant message regardless.

    Returns: {last_assistant, messages, status, state, idle, generation,
        cursor}.
        - last_assistant: {role, content, ts} for the final assistant
          message, or null if the agent hasn't produced one yet.
        - messages: the last `context_messages` rendered messages
          (oldest-first), for context around the final turn.
        - status: "starting"|"working"|"awaiting_input"|"exited".
        - cursor: end cursor — pass to `read_session_output(since_cursor=)`
          if you later want to resume forward reads from here.
    """
    try:
        resolved = control_client.call(
            "resolve_authorized_session",
            {"session_uid": session_uid},
        )
    except control_client.ControlError as e:
        return {"error": e.code, "message": e.message}

    state = resolved.get("state", "pending")
    engine = resolved.get("engine", "claude-code")
    transcript_path = resolved.get("transcript_path")
    generation = int(resolved.get("generation", 0))
    idle = bool(resolved.get("idle", False))
    status = _session_status(state, idle)

    if transcript_path is None:
        return {
            "last_assistant": None,
            "messages": [],
            "status": status,
            "state": state,
            "idle": idle,
            "generation": generation,
            "cursor": None,
        }

    n = max(0, min(context_messages, 100))
    messages, cursor = _read_all_messages(engine, transcript_path, generation)
    tail = messages[-n:] if n else []
    return {
        "last_assistant": _last_assistant(messages),
        "messages": [m.to_dict() for m in tail],
        "status": status,
        "state": state,
        "idle": idle,
        "generation": generation,
        "cursor": cursor,
    }


@mcp.tool()
def start_workflow(
    task_id: str,
    workflow_name: str,
    goal: str = "",
    role_sessions: dict[str, str] | None = None,
) -> dict:
    """Launch a workflow on a task you have authority over.

    The caller is the orchestrator — NOT a participant. The daemon spawns
    every workflow participant as a FRESH session with its TOML-declared
    engine, writes the initial state, and drives the run to completion via
    its poller (it keeps running even with no TUI attached). Use
    `get_workflow_state` and `read_session_output` to observe progress;
    the existing `workflow_transition` / `workflow_done` tools are for
    participants, not orchestrators.

    Args:
        task_id: Target task. Must be the caller's own task or a
            descendant in the parent_task_id tree.
        workflow_name: Workflow definition name (e.g. "feedback").
        goal: Optional initial goal string passed to the worker's
            activation prompt template ({{ goal }}), or delivered verbatim
            when the worker role has no activation_prompt.
        role_sessions: Optional map of `role -> existing daemon session
            uid`. For each entry the daemon ADOPTS the already-running live
            session as that role instead of fresh-spawning it — so a worker
            you've already explored a codebase with (or accepted a plan in)
            starts WARM, keeping its context, and the goal is delivered to
            that live agent. Get the uid from `list_sessions`.

            Eligibility (enforced daemon-side): a role is bindable ONLY when
            it is `context = "persistent"` AND `needs_mcp = false` in the
            workflow TOML (e.g. the feedback `worker`). `Context::Fresh`
            roles (reset every activation) and `needs_mcp = true` roles (a
            manager that calls `workflow_done`) are always fresh-spawned. The
            session must also be a live daemon session in the run's
            workspace, with an engine matching the role and not already a
            participant of another active run. An ineligible or unresolvable
            entry FAILS the whole launch (it is NOT silently fresh-spawned),
            so a worker you expected to keep its context never starts cold
            without a signal. Omit (or pass None) to fresh-spawn every role.

    Returns: {"run_id": "<id>"}.

    State your intent and ask the user to confirm before calling.
    """
    params: dict = {"task_id": task_id, "workflow_name": workflow_name}
    if goal:
        params["goal"] = goal
    # Existing-session binding (Phase 2): forward `role_sessions` verbatim only
    # when provided, so existing callers' wire shape is unchanged (the daemon's
    # StartWorkflowParams.role_sessions is #[serde(default)] — absent == None).
    if role_sessions is not None:
        params["role_sessions"] = role_sessions
    # P-3c: the daemon serializes each participant's transcript-detector binding
    # (the cross-bind fix), waiting up to ~20s/role before spawning the next, so
    # a normal 3-role feedback launch can exceed the default 30s control_client
    # timeout. Use the same generous budget the TUI uses (client_session.rs's
    # START_WORKFLOW_RPC_READ_TIMEOUT = 150s) so the MCP launch path doesn't
    # spuriously report a transport timeout while the daemon goes on to create
    # the run (which would look like a failed launch + trigger duplicate
    # retries). The daemon saves+broadcasts the run only on full success.
    return control_client.call("start_workflow", params, timeout=150.0)


@mcp.tool()
def stop_workflow(run_id: str) -> dict:
    """Mark a workflow run as detached. Participant sessions stay open
    but no further transitions fire. Caller must have authority over
    the workflow's task.

    Ask the user before calling — stopping a workflow is destructive.
    """
    return control_client.call("stop_workflow", {"run_id": run_id})


@mcp.tool()
def get_workflow_state(run_id: str) -> dict:
    """Read full state for a workflow run you have authority over.

    Returns active_role, iteration, paused, status, history (each
    activation's role/trigger/transcript_id), role_sessions (per-role
    session_label + current_transcript_id), goal, started_at,
    done_reason. Use this to monitor a workflow you launched.
    """
    return control_client.call("get_workflow_state", {"run_id": run_id})


@mcp.tool()
def list_workflows(task_id: str | None = None) -> list[dict]:
    """List workflow runs in your scope.

    Args:
        task_id: Optional task filter. Without it, returns runs across
            your task and any descendants.

    Returns: list of {run_id, name, task_id, active_role, iteration,
        paused, status, started_at, done_reason}.
    """
    params: dict = {}
    if task_id:
        params["task_id"] = task_id
    return control_client.call("list_workflows", params)


@mcp.tool()
async def wait_for_workflow_done(
    run_id: str,
    timeout_s: float = 1800.0,
    poll_interval_s: float = 5.0,
) -> dict:
    """Block until a workflow run finishes, then return its final state.

    Finished means `status` is `done` or `detached`, or `done_reason` is
    set (a `paused` run is NOT done — this keeps waiting). Internally
    polls `get_workflow_state` on `poll_interval_s` (clamped to
    [1.0, 60.0]) until done or `timeout_s` elapses (clamped to
    [1.0, 86400.0]).

    Use this to orchestrate multi-step work: launch a workflow, wait,
    inspect, repeat. No need to poll yourself.

    Args:
        run_id: Workflow run id returned by `start_workflow`.
        timeout_s: Max seconds to wait. Default 1800 (30 min).
        poll_interval_s: Seconds between polls. Default 5.

    Returns: {"done": bool, "timed_out": bool, "state": <state dict>}.
        - done=True: workflow finished; inspect `state["status"]` and
          `state["done_reason"]`.
        - done=False, timed_out=True: deadline reached; `state` is the
          last snapshot read.

    Raises ControlError on auth failure or unknown run_id — does not
    retry past those.
    """
    deadline = time.monotonic() + max(1.0, min(timeout_s, 86400.0))
    interval = max(1.0, min(poll_interval_s, 60.0))
    while True:
        state = await asyncio.to_thread(
            control_client.call, "get_workflow_state", {"run_id": run_id}
        )
        status = state.get("status")
        if status in ("done", "detached") or state.get("done_reason") is not None:
            return {"done": True, "timed_out": False, "state": state}
        if time.monotonic() >= deadline:
            return {"done": False, "timed_out": True, "state": state}
        await asyncio.sleep(interval)


@mcp.tool()
async def wait_for_workflow_stop(
    run_id: str,
    timeout_s: float = 3600.0,
    poll_interval_s: float = 5.0,
    stuck_after_s: float = 60.0,
) -> dict:
    """Block until a workflow finishes OR gets stuck, then return.

    Stuck = the active role's session has been idle for `stuck_after_s`
    seconds with no transition (iteration counter unchanged). Use this
    when a workflow can dead-end without anyone calling
    `workflow_done` — e.g. an agent that just stops without a final
    handoff. The orchestrator can then inspect, prod, or stop the run.

    Done = `status` is `done`/`detached` or `done_reason` is set
    (same predicate as `wait_for_workflow_done`).

    Internally polls `get_workflow_state` + `list_sessions` on
    `poll_interval_s` (clamped to [1.0, 60.0]) until done/stuck or
    `timeout_s` elapses. The stuck timer resets every time `iteration`
    changes.

    Args:
        run_id: Workflow run id returned by `start_workflow`.
        timeout_s: Max seconds to wait. Default 3600 (1 hour).
        poll_interval_s: Seconds between polls. Default 5.
        stuck_after_s: Seconds the active session must stay idle (with
            no transition) before the run is declared stuck. Default 60.

    Returns: {"done": bool, "stuck": bool, "timed_out": bool,
        "state": <state dict>}. Exactly one of done/stuck/timed_out is
        true on return.
    """
    deadline = time.monotonic() + max(1.0, min(timeout_s, 86400.0))
    interval = max(1.0, min(poll_interval_s, 60.0))
    stuck_window = max(5.0, min(stuck_after_s, 3600.0))

    last_iteration: int | None = None
    idle_since: float | None = None

    while True:
        state = await asyncio.to_thread(
            control_client.call, "get_workflow_state", {"run_id": run_id}
        )
        status = state.get("status")
        if status in ("done", "detached") or state.get("done_reason") is not None:
            return {
                "done": True, "stuck": False, "timed_out": False, "state": state,
            }

        iteration = state.get("iteration", 0)
        active_role = state.get("active_role")
        role_sessions = state.get("role_sessions") or {}

        if iteration != last_iteration:
            last_iteration = iteration
            idle_since = None

        active_label = None
        if active_role and active_role in role_sessions:
            active_label = (role_sessions[active_role] or {}).get("session_label")

        active_idle = False
        if active_label:
            # Omit task_id — defaults to the caller's scope, which
            # includes the workflow's participant sessions for any
            # orchestrator authorized to launch the run.
            sessions = await asyncio.to_thread(
                control_client.call, "list_sessions", {"include_exited": False}
            )
            for s in sessions:
                if s.get("label") == active_label:
                    active_idle = bool(s.get("idle", False))
                    break

        now = time.monotonic()
        if active_idle:
            if idle_since is None:
                idle_since = now
            elif now - idle_since >= stuck_window:
                return {
                    "done": False, "stuck": True, "timed_out": False, "state": state,
                }
        else:
            idle_since = None

        if time.monotonic() >= deadline:
            return {
                "done": False, "stuck": False, "timed_out": True, "state": state,
            }
        await asyncio.sleep(interval)


@mcp.tool()
async def wait_for_session_idle(
    session_uid: str,
    timeout_s: float = 600.0,
    poll_interval_s: float = 2.0,
    pending_idle_grace_s: float = 8.0,
) -> dict:
    """Block until a session becomes idle (agent at the prompt), then
    return.

    "Idle" mirrors the same signal surfaced by `read_session_output`
    and `list_sessions`. An `exited` session is reported as idle (no
    PTY left to be busy on). Internally polls
    `resolve_authorized_session` on `poll_interval_s` (clamped to
    [0.5, 30.0]) until idle or `timeout_s` elapses (clamped to
    [1.0, 86400.0]).

    Use this after `send_input` or `start_session` instead of looping
    on `read_session_output` yourself.

    Pending-but-quiet sessions: a session is `state="pending"` until the
    daemon binds a transcript path for it. A `ready` session that goes
    quiet returns immediately. A `pending` session that goes quiet is
    given a grace window (`pending_idle_grace_s`) for its transcript to
    bind into `ready` — if it stays continuously pending+quiet past the
    grace, we return idle anyway with state="pending" rather than
    blocking to `timeout_s`. This bounds the wait for sessions that will
    NEVER bind a transcript (bash sessions, or claude/codex on a daemon
    with no transcript detector watching it) while still letting a
    freshly-spawned agent's transcript self-heal first. Any busy poll
    resets the grace clock.

    Args:
        session_uid: Target session's stable UID.
        timeout_s: Max seconds to wait. Default 600 (10 min).
        poll_interval_s: Seconds between polls. Default 2.
        pending_idle_grace_s: Max seconds to keep waiting on a
            continuously pending+quiet session for its transcript to
            bind before returning idle anyway. Default 8. Clamped to
            [1.0, 60.0].

    Returns: {"idle": bool, "timed_out": bool, "state":
        "ready"|"pending"|"exited", "status":
        "starting"|"working"|"awaiting_input"|"exited"}.
        `status` is the legible summary of (state, idle) — branch on it
        instead of decoding the pair:
        - status="awaiting_input" (state="ready", idle): agent finished
          its turn and is back at the prompt.
        - status="exited" (state="exited"): session terminated.
        - status="starting" (state="pending", idle): PTY quiet but no
          transcript bound; best-effort idle after the grace window
          (transcript never bound — e.g. a bash session or a
          detector-less daemon).
        - status="working" (idle=False, timed_out=True): deadline
          reached while still busy.

    Note: this returns on the FIRST quiet-at-prompt poll, which can race
    a slow-to-start agent right after `send_input` (the session looks
    quiet for ~2s before the agent's first token). To send a prompt and
    reliably get the reply to THAT prompt, use `send_input_and_wait`,
    which anchors on transcript progress instead.

    Raises ControlError on auth failure or unknown session_uid.
    """
    deadline = time.monotonic() + max(1.0, min(timeout_s, 86400.0))
    interval = max(0.5, min(poll_interval_s, 30.0))
    grace = max(1.0, min(pending_idle_grace_s, 60.0))
    resolved_once = False
    # Monotonic time the session FIRST went (pending && idle) in an
    # unbroken streak; reset to None on any non-(pending && idle) poll.
    pending_idle_since = None
    while True:
        try:
            resolved = await asyncio.to_thread(
                control_client.call,
                "resolve_authorized_session",
                {"session_uid": session_uid},
            )
        except control_client.ControlError as e:
            # The daemon evicts an exited session from its registry within a
            # few seconds of child exit (no tombstone in
            # resolve_authorized_session), so a session that resolved fine and
            # then turns not_found has EXITED mid-wait. The eviction IS the
            # exit signal — report it rather than letting not_found escape as
            # a ControlError (which would crash the canonical "send_input then
            # wait" helper exactly when the watched turn finishes by exiting).
            # A uid that NEVER resolved is a genuine bad-uid error — re-raise.
            if resolved_once and getattr(e, "code", None) == "not_found":
                return {
                    "idle": True, "timed_out": False, "state": "exited",
                    "status": "exited",
                }
            raise
        resolved_once = True
        state = resolved.get("state", "pending")
        idle = bool(resolved.get("idle", False))
        now = time.monotonic()
        if state == "exited":
            return {
                "idle": True, "timed_out": False, "state": state,
                "status": _session_status(state, idle),
            }
        # A READY session (transcript bound) that's quiet is unambiguously
        # done with its turn — return immediately.
        if state == "ready" and idle:
            return {
                "idle": True, "timed_out": False, "state": state,
                "status": _session_status(state, idle),
            }
        # A `pending` session reports idle=True as soon as its PTY is
        # quiet, but quiet-and-pending is ambiguous: the transcript may
        # just not be bound YET (a freshly-spawned agent the detector
        # hasn't caught up to), OR it may NEVER bind (a bash session, or
        # claude/codex on a daemon with no transcript detector). Returning
        # on the first quiet poll would risk a false "done" in the former
        # case; blocking forever (the pre-fix behavior) hangs the latter
        # until `timeout_s`. Compromise: start a grace clock on the first
        # pending+quiet poll and keep waiting for the transcript to bind;
        # if the streak survives `grace`, treat PTY-quiet as the idle
        # signal and return with state="pending". Any busy poll (or a
        # bind into "ready") resets the streak.
        if state == "pending" and idle:
            if pending_idle_since is None:
                pending_idle_since = now
            elif now - pending_idle_since >= grace:
                return {
                "idle": True, "timed_out": False, "state": state,
                "status": _session_status(state, idle),
            }
        else:
            pending_idle_since = None
        if now >= deadline:
            return {
                "idle": False, "timed_out": True, "state": state,
                "status": _session_status(state, idle),
            }
        await asyncio.sleep(interval)


@mcp.tool()
async def send_input_and_wait(
    session_uid: str,
    text: str,
    submit: bool = True,
    timeout_s: float = 600.0,
    poll_interval_s: float = 2.0,
    pending_idle_grace_s: float = 8.0,
) -> dict:
    """Send a prompt to a session and block until it finishes replying,
    then return the reply. The canonical "ask an agent something and get
    its answer" call.

    This folds the three-step dance (`send_input` → `wait_for_session_idle`
    → `read_last_turn`) into one and — crucially — closes the race in the
    gap between them. `send_input` makes the session look busy for only
    ~2s, so if the agent takes longer than that to emit its first token, a
    plain idle-wait can return "done" BEFORE the agent has even started,
    handing you the PREVIOUS turn's message. This tool anchors completion
    to transcript progress: it records where the transcript ends BEFORE
    sending, then reports complete only once a NEW assistant message has
    appeared AND the session has gone quiet — so you always get the reply
    to THIS input.

    For transcript-less sessions (bash, or an agent whose transcript never
    binds) there is nothing to anchor on, so it falls back to the same
    bounded idle-wait as `wait_for_session_idle` and returns no message.

    Args:
        session_uid: Target session's stable UID.
        text: The prompt/body to deliver.
        submit: Append Enter so the agent submits it (default true). With
            submit=False the agent won't act, and this will time out.
        timeout_s: Max seconds to wait for the reply. Default 600 (10 min).
        poll_interval_s: Seconds between polls. Default 2.
        pending_idle_grace_s: Grace for a transcript-less session to
            settle before reporting idle (see `wait_for_session_idle`).
            Default 8.

    Returns: {delivered, completed, timed_out, status, state, idle,
        last_message}.
        - completed=True: the agent produced a reply and went quiet;
          `last_message` is its final assistant message {role, content,
          ts} (null for transcript-less sessions even on success).
        - completed=False, timed_out=True: no completed reply before the
          deadline (agent may be stuck, or mid-tool-call on a very long
          turn) — inspect `status`/`state`, or `read_last_turn` to see
          partial progress.

    State your intent and ask the user before calling — this delivers
    input to another session, same as `send_input`.
    """
    deadline = time.monotonic() + max(1.0, min(timeout_s, 86400.0))
    interval = max(0.5, min(poll_interval_s, 30.0))
    grace = max(1.0, min(pending_idle_grace_s, 60.0))

    # Anchor: capture the transcript end BEFORE sending so we can tell the
    # reply to THIS input apart from the turn that was already there. The
    # file persists across exit, so the captured path also lets us read
    # the final reply even if the agent exits right after answering.
    engine = "claude-code"
    transcript_path: str | None = None
    anchor_cursor: str | None = None
    generation = 0
    try:
        pre = await asyncio.to_thread(
            control_client.call,
            "resolve_authorized_session",
            {"session_uid": session_uid},
        )
        engine = pre.get("engine", "claude-code")
        transcript_path = pre.get("transcript_path")
        generation = int(pre.get("generation", 0))
        if transcript_path is not None:
            _pre_msgs, anchor_cursor = await asyncio.to_thread(
                _read_all_messages, engine, transcript_path, generation
            )
    except control_client.ControlError as e:
        return {"error": e.code, "message": e.message}

    # Deliver the input. Routes to the TUI's encoding-aware drainer
    # (kitty Enter etc.), same as the standalone `send_input` tool.
    await asyncio.to_thread(
        control_client.call,
        "send_input",
        {"session_uid": session_uid, "text": text, "submit": submit},
    )

    cursor = anchor_cursor
    saw_new_assistant = False
    last_message: dict | None = None
    pending_idle_since: float | None = None
    resolved_once = False

    def _final_read() -> dict | None:
        if transcript_path is None:
            return last_message
        try:
            msgs, _ = _read_all_messages(engine, transcript_path, generation)
        except OSError:
            return last_message
        return _last_assistant(msgs) or last_message

    while True:
        try:
            resolved = await asyncio.to_thread(
                control_client.call,
                "resolve_authorized_session",
                {"session_uid": session_uid},
            )
        except control_client.ControlError as e:
            # Evicted-after-seen == exited mid-turn (see
            # `wait_for_session_idle`). The agent answered then exited
            # (one-shot) — read the final reply from the captured path.
            if resolved_once and getattr(e, "code", None) == "not_found":
                last_message = await asyncio.to_thread(_final_read)
                return {
                    "delivered": True, "completed": True, "timed_out": False,
                    "status": "exited", "state": "exited", "idle": True,
                    "last_message": last_message,
                }
            raise
        resolved_once = True
        state = resolved.get("state", "pending")
        idle = bool(resolved.get("idle", False))
        # A fresh agent's transcript may bind only AFTER we sent — pick it
        # up the moment it appears.
        if transcript_path is None and resolved.get("transcript_path"):
            transcript_path = resolved.get("transcript_path")
            engine = resolved.get("engine", engine)
            generation = int(resolved.get("generation", generation))
        now = time.monotonic()

        if state == "exited":
            last_message = await asyncio.to_thread(_final_read)
            return {
                "delivered": True, "completed": True, "timed_out": False,
                "status": "exited", "state": "exited", "idle": True,
                "last_message": last_message,
            }

        if transcript_path is not None:
            # Transcript-anchored: pull new messages since the anchor,
            # latch the reply, and complete only when a NEW assistant
            # message exists AND the session is quiet at the prompt.
            new_msgs, cursor = await asyncio.to_thread(
                _parser_for(engine).read_messages,
                transcript_path, generation, cursor, _READ_ALL_LIMIT,
            )
            for m in new_msgs:
                if m.role == Role.ASSISTANT:
                    saw_new_assistant = True
                    last_message = m.to_dict()
            if saw_new_assistant and state == "ready" and idle:
                return {
                    "delivered": True, "completed": True, "timed_out": False,
                    "status": _session_status(state, idle),
                    "state": state, "idle": idle, "last_message": last_message,
                }
        else:
            # Transcript-less fallback == `wait_for_session_idle` semantics
            # (no reply to return).
            if state == "ready" and idle:
                return {
                    "delivered": True, "completed": True, "timed_out": False,
                    "status": _session_status(state, idle),
                    "state": state, "idle": idle, "last_message": None,
                }
            if state == "pending" and idle:
                if pending_idle_since is None:
                    pending_idle_since = now
                elif now - pending_idle_since >= grace:
                    return {
                        "delivered": True, "completed": True,
                        "timed_out": False,
                        "status": _session_status(state, idle),
                        "state": state, "idle": idle, "last_message": None,
                    }
            else:
                pending_idle_since = None

        if now >= deadline:
            return {
                "delivered": True, "completed": False, "timed_out": True,
                "status": _session_status(state, idle),
                "state": state, "idle": idle, "last_message": last_message,
            }
        await asyncio.sleep(interval)


@mcp.tool()
async def wait_for_any_session_idle(
    session_uids: list[str],
    timeout_s: float = 1800.0,
    poll_interval_s: float = 2.0,
    pending_idle_grace_s: float = 8.0,
    return_last_message: bool = True,
) -> dict:
    """Monitor MANY sessions at once and return as soon as ANY finishes.
    The multi-session counterpart to `wait_for_session_idle`.

    `wait_for_session_idle` blocks on a single session, so watching a
    fan-out of workers means waiting on them one at a time and missing
    whoever finishes first. This watches the whole set in one call and
    returns the moment one (or more) reaches a terminal state — finished
    its turn (`awaiting_input`) or `exited` — telling you WHICH, and
    (by default) handing you each one's final message inline so you don't
    need a follow-up read.

    Canonical fan-out loop:

        remaining = [a, b, c]
        while remaining:
            r = wait_for_any_session_idle(remaining)
            for done in r["completed"]:
                ...handle done["session_uid"], done["last_message"]...
            remaining = r["still_running"]
            if r["timed_out"]:
                break

    Each session uses the same idle rule as `wait_for_session_idle`
    (ready+quiet, or exited; a transcript-less pending+quiet session
    reports after `pending_idle_grace_s`). SAME race caveat too: a session
    that is already quiet at call time is reported immediately, so don't
    pass a worker you JUST sent input to here — use `send_input_and_wait`
    for that one, then monitor the rest with this.

    Args:
        session_uids: Sessions to watch (UIDs from `list_sessions`).
        timeout_s: Max seconds to wait. Default 1800 (30 min).
        poll_interval_s: Seconds between polls. Default 2.
        pending_idle_grace_s: Grace for transcript-less pending sessions.
            Default 8.
        return_last_message: Include each completed session's final
            assistant message. Default true.

    Returns: {completed, still_running, timed_out}.
        - completed: list of {session_uid, status, state, idle,
          last_message} for sessions that finished this call (>=1 unless
          timed out). A bad/unauthorized uid is reported here once with
          status="error" and an `error` code, so a typo can't block the
          call forever. last_message is null for transcript-less sessions
          / when return_last_message is false.
        - still_running: UIDs not yet finished — pass back to keep waiting.
        - timed_out: true if the deadline passed with nothing finished.

    Blocking. To monitor a fan-out WITHOUT parking this orchestrator —
    so you stay free to handle the user or other work while workers run —
    run the `cm-wait` CLI (`mcp_server/wait.py`) under
    `Bash(run_in_background=true)` instead; it wraps this same core and
    the harness wakes you when it exits. See the orchestrator skills.
    """
    return await _monitor_sessions(
        session_uids,
        mode="any",
        timeout_s=timeout_s,
        poll_interval_s=poll_interval_s,
        pending_idle_grace_s=pending_idle_grace_s,
        return_last_message=return_last_message,
    )


@mcp.tool()
def create_subtask(
    name: str,
    prompt: str = "",
    worktree_mode: str = "inherit",
    project: str | None = None,
) -> dict:
    """Create a subtask under your current task.

    The new task gets `parent_task_id` set to your task. You must have
    a bound task (workflow- or planning-launched session). Taskless
    callers (`A-n`) should use `propose_task` for top-level tasks.

    Args:
        name: Display name for the subtask. Slugified for the branch
            name (alphanumerics + dash, max 40 chars).
        prompt: Optional initial prompt for the subtask's worker.
        worktree_mode: "inherit" (default) — same worktree as parent;
            sessions spawn in the parent's worktree directory.
            "branch" — new worktree branched off the parent's
            wip_branch with name `cm-sub/<slug-chain>-<short_id>`.
            "in-place" — spawn directly in the parent's MAIN repo
            checkout: no new worktree, no new branch.
        project: Optional explicit project; defaults to the parent's
            project.

    Returns: {"task_id": "<uuid>", "worktree_path": "<absolute path>"}.

    State your intent and ask the user before calling. Subtasks are a
    real fork — branch mode creates real git state.
    """
    params: dict = {
        "name": name,
        "worktree_mode": worktree_mode,
    }
    if prompt:
        params["prompt"] = prompt
    if project:
        params["project"] = project
    return control_client.call("create_subtask", params)


@mcp.tool()
def list_subtasks(task_id: str | None = None) -> list[dict]:
    """List direct children of a task.

    Args:
        task_id: Optional explicit task. Defaults to your own task.
            Must be your task or a descendant; cross-task scoping
            is rejected.

    Returns: list of {task_id, name, status, worktree_mode, wip_branch,
        workspace_id}. Only direct children — to walk a deeper tree,
        recurse manually with successive list_subtasks calls.
    """
    params: dict = {}
    if task_id:
        params["task_id"] = task_id
    return control_client.call("list_subtasks", params)


@mcp.tool()
def mark_subtask_done(task_id: str, close_worktree: bool = True) -> dict:
    """Mark a subtask done. Optionally tear down its worktree.

    Args:
        task_id: The subtask. Must be your task or a descendant.
        close_worktree: When true (default) AND the subtask used
            branch mode, tombstone its sessions, run `git worktree
            remove`, and mark its workspace closed. The branch ref
            stays so merge history is preserved — prune manually with
            `git branch -d cm-sub/<slug-chain>-<short_id>` once
            you're confident.

    Returns: {"ok": true, "worktree_removed": bool}.

    For branch-mode subtasks, run your `git merge` (or rebase, or
    cherry-pick) into the parent worktree BEFORE calling this — once
    the worktree is removed there's no working copy to merge from.
    Inherit-mode subtasks have nothing to remove; close_worktree is
    ignored.

    Ask the user before calling — this is a multi-step destructive
    operation.
    """
    return control_client.call(
        "mark_subtask_done",
        {"task_id": task_id, "close_worktree": close_worktree},
    )


@mcp.tool()
def set_subtask_status(status: str, task_id: str | None = None) -> dict:
    """Set the planning status of your task (or a descendant subtask).

    The headless-capable way to flip a task's status. It works even where
    the cli-routed `update_task` is unavailable — e.g. a daemon-spawned
    agent on a remote host whose MCP env lacks the planning `cli` package +
    `CM_API_URL`. The status change routes through the daemon, which holds
    the planning creds. Unlike `mark_subtask_done` it does NO worktree or
    session teardown: `blocked` means the work is waiting for review, so
    your session stays alive.

    Common use: a bug-fix subtask agent flips its OWN task to `blocked`
    (= fix-ready for the user to review/merge) after committing its fix.

    Args:
        status: One of "draft", "backlog", "running", "blocked", "done",
            "archived".
        task_id: Optional explicit task. Defaults to your own task. Must be
            your task or a descendant; cross-task scoping is rejected.

    Returns: {"task_id": <id>, "status": <status>}.
    """
    params: dict = {"status": status}
    if task_id:
        params["task_id"] = task_id
    return control_client.call("set_subtask_status", params)


@mcp.tool()
def trigger(
    task_id: str,
    prompt: str = "",
    args: dict | None = None,
    mode: str = "",
    fire_token: str = "",
) -> dict:
    """Fire a continuous task NOW (Continuous Tasks Phase 2 — manual fan-out).

    A continuous task is a durable, repeatable unit of work pinned to its own
    worktree (created when the task was registered). Triggering it runs one
    iteration. In `fresh` run_mode the daemon spawns a NEW session per trigger
    and leaves prior sessions idle (it does NOT kill them); continuity flows
    through the per-task NOTES.md (the default prompt instructs read-NOTES-
    first). `persistent` run_mode is not yet implemented in Phase 2 and returns
    {"fired": false, "reason": "persistent_not_yet_implemented"} cleanly.

    Args:
        task_id: The continuous task to fire. Must be YOUR OWN task or a
            self-or-descendant in the parent_task_id tree — cross-task
            triggering is rejected (the downstream-allowlist fan-out is a
            later phase).
        prompt: Optional one-off prompt for this run, overriding the task's
            default prompt / mode preset.
        args: Optional free-form JSON blob passed through to the run; the
            daemon does not parse it.
        mode: Optional named mode preset (selects a registered prompt/args
            preset on the task).
        fire_token: Optional idempotency token. If it equals the last run's
            fire_token, the call is a no-op and returns
            {"fired": false, "reason": "duplicate_fire_token"}. Absent => the
            daemon mints one (`ft_<hex>`).

    Returns:
        fired => {"fired": true, "fire_token": str, "session_uid": str,
            "run_mode": str}.
        not fired => {"fired": false, "reason": "duplicate_fire_token" |
            "busy" | "paused" | "persistent_not_yet_implemented"}. "busy"
            means a concurrent trigger is still inside the spawn window.

    State your intent and ask the user to confirm before calling — this spawns
    a session.
    """
    params: dict = {"task_id": task_id}
    if prompt:
        params["prompt"] = prompt
    if args:
        params["args"] = args
    if mode:
        params["mode"] = mode
    if fire_token:
        params["fire_token"] = fire_token
    return control_client.call("trigger", params)


@mcp.tool()
def report_done(reason: str | None = None) -> dict:
    """Signal that YOUR continuous-task run is complete (Continuous Tasks Phase 3b).

    Call this from inside a continuous-task tick once you've finished the
    iteration's work. The daemon resolves which task and run you are from your
    own session — there is NO task_id argument. It flips the active run's status
    from Running to Done (recording finished_at), which clears the watchdog's
    stuck-detection clock so a slow-but-real run is never misread as wedged.

    The call is scoped to YOUR session only: it marks done IFF you are the
    session that owns the currently-active run. If you are not (a stale/older
    run, or you've already been superseded) it is a SOFT no-op that returns a
    clear message rather than an error.

    Args:
        reason: Optional short note about why the run is complete.

    Returns:
        {"ok": true, "done": bool, "task_id": str, "message": str}. `done` is
        false on the soft no-op case.

    This only flips your own run's status, so — like notify_user — you do NOT
    need to ask the user first. Just call it when your run is genuinely done.
    """
    params: dict = {}
    if reason:
        params["reason"] = reason
    return control_client.call("report_done", params)


@mcp.tool()
def resolve_stuck(
    task_id: str,
    seq: int,
    action: str,
    reason: str | None = None,
) -> dict:
    """Render a verdict on a stuck continuous-task run (Continuous Tasks Phase 3b).

    INVESTIGATOR-ONLY. When the watchdog detects a fresh continuous-task run that
    has run past its time budget but is still alive, it snapshots the evidence and
    spawns YOU (the investigator) in the task's worktree. Read the snapshot dir
    (metadata.json + the run transcript + NOTES.md) and inspect the worktree, then
    call this exactly ONCE with one of three actions:

      - mark_unstuck: the run is making real progress / is just slow. Keep it
        running and reset the watchdog clock.
      - restart: the run is wedged but the task is sound. Kill the stuck session
        and re-fire a fresh run.
      - escalate: the run needs a human. Kill the stuck session, mark the run
        Stuck, and surface it to the user.

    After this call your investigation is finished (you may exit / report_done).

    Args:
        task_id: The continuous task whose stuck run you are resolving (from your
            daemon-constructed prompt).
        seq: The stuck run's sequence number (from your prompt / metadata.json).
        action: One of "mark_unstuck", "restart", "escalate".
        reason: Optional explanation; surfaced to the user on "escalate".

    State your intent and ask the user to confirm before calling — `restart` and
    `escalate` kill (and `restart` re-fires) a session.
    """
    params: dict = {"task_id": task_id, "seq": seq, "action": action}
    if reason:
        params["reason"] = reason
    return control_client.call("resolve_stuck", params)


def _require_workflow_env() -> tuple[str, str]:
    """Return (run_id, role) from env, or raise if not in a workflow session.

    Both `workflow_transition` and `workflow_done` (and 11e-onward
    `workflow_reject_finding`) need the same env-driven context; this
    helper deduplicates the lookup + the RuntimeError message.

    Phase 2 merge: absorbed main's `772d8fe` operator hint about the
    per-session MCP config. When a workflow role's env block is
    missing CM_WORKFLOW_RUN_ID — the symptom of a stale config or
    an un-respawned MCP subprocess — the error names the file to
    fix and the respawn requirement.
    """
    run_id = os.environ.get("CM_WORKFLOW_RUN_ID", "").strip()
    if not run_id:
        session_uid = os.environ.get("CM_TUI_SESSION_ID", "").strip() or "<unknown>"
        cfg_hint = (
            f"~/.cm/mcp/{session_uid}/claude.json"
            if session_uid != "<unknown>"
            else "the session's per-session MCP config under ~/.cm/mcp/<session_uid>/claude.json"
        )
        raise RuntimeError(
            "CM_WORKFLOW_RUN_ID is not set — workflow tools are only "
            "usable inside a workflow-participant session. If you are "
            f"running inside a workflow role (session UID {session_uid}) "
            f"and seeing this, the per-session MCP config is missing the "
            f"workflow env block. Check {cfg_hint} — the env block should "
            "include CM_WORKFLOW_RUN_ID and CM_ROLE. Fixing the file alone "
            "is not enough; the agent process must be respawned for the "
            "MCP subprocess to pick up the new env."
        )
    role = os.environ.get("CM_ROLE", "").strip() or "unknown"
    return run_id, role


@mcp.tool()
def workflow_transition(to: str, prompt: str) -> str:
    """Hand control to another role in the current workflow.

    Use this to end your turn and activate a different role with a specific prompt.
    The TUI delivers `prompt` to that role's session when it next activates.

    Args:
        to: Name of the role to transition to (must be declared in the workflow).
        prompt: The message to send to that role when it activates.
    """
    # 11g-2 (A2): routes unconditionally through the daemon. Pre-11g-2
    # an `else` branch wrote events.jsonl directly via `_append_event`
    # for TUI-local spawns; daemon-mandatory since 10f makes that
    # path unreachable, and the controller's channel-only consumption
    # (post-11g-2-a) would render any stranded file-only write
    # invisible. See the module-head comment for the rationale.
    run_id, role = _require_workflow_env()
    result = control_client.call(
        "workflow_transition",
        {"to": to, "prompt": prompt, "run_id": run_id, "role": role},
    )
    return f"Queued transition to '{to}' (event {result['event_id']})."


@mcp.tool()
def workflow_done(reason: str) -> str:
    """End the current workflow run.

    Use this when the workflow's goal is achieved and no further iteration is needed.
    All participant sessions remain open in the TUI but the workflow stops firing
    transitions.

    Args:
        reason: Short explanation of why the workflow is complete.
    """
    # 11g-2 (A2): see `workflow_transition` above — same daemon-only
    # routing.
    run_id, role = _require_workflow_env()
    result = control_client.call(
        "workflow_done",
        {"reason": reason, "run_id": run_id, "role": role},
    )
    return f"Workflow marked done (event {result['event_id']})."


@mcp.tool()
def workflow_reject_finding(text: str) -> str:
    """Permanently dismiss a reviewer finding for the rest of this workflow run.

    Use this when a reviewer surfaces a finding you've decided is not worth
    acting on (a nit, an out-of-scope concern, a paranoid threat model, etc.)
    AND you expect the reviewer to keep raising it round after round. The
    rejection is appended to a stash on the workflow run and surfaced to the
    reviewer on its next (fresh-context) activation so it knows to stop
    re-raising the same concern.

    This is a side-effect call. It does NOT end your turn or advance the
    workflow — follow up with `workflow_transition` or `workflow_done` for
    that. Call it 0–N times per round, before the final transition.

    Paraphrase the rejected finding into one concise line. "Symlink check on
    /tmp paranoid for a single-user laptop — won't re-raise" reads better in
    the reviewer's prompt than pasting the reviewer's whole multi-paragraph
    finding back.

    Args:
        text: One-line paraphrase of the rejected finding (plus a short why,
            optionally). Free-text; surfaced to the reviewer verbatim as a
            bullet point.
    """
    # 11g-2 (A2): routes unconditionally through the daemon. Same
    # rationale as `workflow_transition` / `workflow_done` above —
    # TUI-local fallback was removed once the daemon became
    # mandatory and the controller stopped reading events.jsonl.
    run_id, role = _require_workflow_env()
    result = control_client.call(
        "workflow_reject_finding",
        {"text": text, "run_id": run_id, "role": role},
    )
    return f"Recorded rejection (event {result['event_id']})."


if __name__ == "__main__":
    if "--selftest" in sys.argv:
        # Startup preflight (invoked by the daemon's run_mcp_preflight). Reaching
        # here means this module already imported on THIS interpreter/host — so
        # the missing-`cli`-package crash and a missing interpreter are already
        # caught. Confirm tool registration too, then exit. The daemon logs a
        # loud, actionable error at startup when this fails, instead of a
        # workflow participant silently failing to start its MCP server later.
        try:
            _tools = asyncio.run(mcp.list_tools())
        except Exception as e:  # noqa: BLE001 — surface any failure verbatim
            print(f"selftest FAILED: {e}", file=sys.stderr)
            sys.exit(1)
        print(f"selftest OK: {len(_tools)} tools registered", file=sys.stderr)
        sys.exit(0)
    mcp.run()
