"""MCP server for Claude instances to propose tasks to the backlog."""

import asyncio
import json
import os
import re
import sys
import time

# Optional: full JSON-Schema validation for the `schema=` structured-output
# option on the spawn-and-run / send-and-wait tools. Absent on the lean
# headless deploy (requirements.txt lists it, but a host that hasn't
# re-installed won't have it) — we degrade to a minimal type+required check
# (`_minimal_validate`) rather than hard-fail. See `_validate_schema`.
try:
    import jsonschema as _jsonschema
except Exception:  # pragma: no cover - exercised only where the dep is absent
    _jsonschema = None

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
    SEMANTIC_IDLE_GRACE_S,
    transcript_turn_complete,
)
from mcp_server import async_monitor

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


def _git_origin_url() -> str:
    """Repo URL from ``git remote get-url origin``, with NO dependency on the
    ``cli`` package.

    propose_task's daemon path needs the repo URL to forward to the daemon (the
    daemon doesn't know the agent's cwd), but it must not reach into
    ``cli.planning_client`` to get it — ``cli/`` isn't deployed alongside the MCP
    server on headless/remote hosts (cm-manager), which used to make propose_task
    fail there even though the daemon path needs nothing else from ``cli``
    (create_subtask etc. already work headless via the control socket)."""
    import subprocess

    result = subprocess.run(
        ["git", "remote", "get-url", "origin"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(
            f"could not detect repo URL from git remote origin: {result.stderr.strip()}"
        )
    return result.stdout.strip()


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

    If YOU have a bound task, the task you propose counts as part of
    your own scope: you can later `start_session(task_id=<proposed id>)`
    to run it yourself (with the user's approval) and drive the worker
    you spawned. Prefer `create_subtask` when the work should live in
    its own worktree under your task; prefer propose_task when the user
    should triage it on the planning board first.

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
        # Detect the repo URL to forward to the daemon with a plain
        # `git remote get-url origin` shell-out (see _git_origin_url) — NOT via
        # cli.planning_client, which isn't deployed on headless/remote hosts
        # (cm-manager) and made propose_task fail there even though the daemon
        # path needs nothing else from `cli`. Raises RuntimeError with no origin
        # remote and OSError (FileNotFoundError) if git is absent.
        try:
            repo_url = _git_origin_url()
        except (RuntimeError, OSError) as e:
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
    except Exception:
        # Headless deployment (e.g. a daemon-spawned agent on cm-manager):
        # the cli-routed PlanningClient is unavailable — the `cli` package
        # isn't deployed alongside the MCP server, and/or CM_API_URL /
        # CM_API_TOKEN aren't in the agent's env. Route the update through the
        # daemon, which holds the planning creds, and column-allowlists +
        # status-validates the fields. It re-reads the row, so the return is a
        # full, `_shape_task`-shaped task — UNIFORM with the cli path (and the
        # status-only case, which used to short-circuit to `set_subtask_status`
        # and return a bare {task_id, status}). A daemon-spawned agent's natural
        # `update_task(...)` call works headless without the orchestrator prompt
        # needing to know the routing, and always sees the same shape.
        #
        # (The standalone `set_subtask_status` tool remains for callers that
        # deliberately want the minimal {task_id, status} shape + no re-read.)
        return _shape_task(
            control_client.call("update_task", {"task_id": task_id, "fields": fields}),
            full=True,
        )
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


# Terminal geometry the daemon reports per session but that's pure noise
# for orchestration — an agent never branches on PTY width/height. Dropped
# from the MCP projection (P2 field trim) so the per-session dict is all
# orchestration-relevant fields. Not a wire-shape change: these keys just
# stop appearing.
_SESSION_NOISE_FIELDS = ("cols", "rows")


def _annotate_shared_worktrees(sessions: list[dict]) -> None:
    """Stamp `workspace_shared_with` on every LIVE row that shares its
    `worktree_path` with another live row.

    The hint answers "who else is editing this checkout right now?" —
    the question an agent has to answer before it edits a file, and one
    it previously could not answer at all (it had to eyeball paths and
    guess). Value is a list of `{"session_uid", "label"}` for the OTHER
    live sessions on the same path; the key is omitted when a session
    has its checkout to itself, so the common case stays quiet.

    Scope safety: computed STRICTLY from `sessions` — the rows the host
    already filtered down to what this caller may see. Both host
    implementations authorize per row before emitting it (the daemon's
    `list_sessions` runs every row through `should_include`, which walks
    the caller's task tree / workspace via `check_session_caller`; the
    TUI twin runs `caller_authorized_for` per row). So this can only
    ever name a session the caller could already list. Never re-query
    the host for a wider set to build this — that would leak session
    uids and labels across auth boundaries.

    Exited rows (present only under `include_exited`) neither receive
    the hint nor count as sharers: a closed session isn't touching the
    checkout anymore.
    """
    by_path: dict[str, list[dict]] = {}
    for s in sessions:
        if s.get("state") == "exited":
            continue
        path = s.get("worktree_path")
        uid = s.get("session_uid")
        if not path or not uid:
            continue
        by_path.setdefault(path, []).append(s)
    for peers in by_path.values():
        if len(peers) < 2:
            continue
        for s in peers:
            s["workspace_shared_with"] = [
                {"session_uid": o.get("session_uid"), "label": o.get("label")}
                for o in peers
                if o.get("session_uid") != s.get("session_uid")
            ]


def _list_sessions_raw(task_id: str | None, include_exited: bool) -> list[dict]:
    """Shared implementation behind `list_sessions` and
    `list_sessions_grouped`: call the host, enrich each entry with the
    legible `status` word and the `workspace_shared_with` hint, and drop
    terminal-geometry noise."""
    params: dict = {"include_exited": include_exited}
    if task_id:
        params["task_id"] = task_id
    sessions = control_client.call("list_sessions", params)
    for s in sessions:
        s["status"] = _session_status(
            s.get("state", "pending"),
            bool(s.get("idle", False)),
            bool(s.get("reported_done", False)),
        )
        for k in _SESSION_NOISE_FIELDS:
            s.pop(k, None)
    _annotate_shared_worktrees(sessions)
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

    Returns: a list of per-session dicts. (As a list-returning tool, the
    list arrives wrapped as `{"result": [ ... ]}` on the wire; dict-returning
    tools like `ping` are bare — a known shape split we keep for
    back-compat.) Each dict:
        {session_uid, label, type, state, idle, status, managed_by_uid,
         task_id, workspace_id, workflow_run_id, workflow_role,
         global_perms, continuous_task_id, worktree_path,
         workspace_shared_with?}
        state ∈ {"ready", "pending", "exited"}.
        Exited rows (include_exited=true) additionally carry HOW they
        ended: exited_at (unix seconds), killed (true when the exit
        followed a kill request rather than the agent stopping on its
        own), and killed_by (who or what asked — a session uid,
        "operator", "memory-cap" for a session the memory cap OOM-killed,
        or a uid annotated with the sweep that killed it). Use them
        before reading a dead session's tail — a killed session's last
        message is wherever the SIGKILL cut it off, not a conclusion.
        reported_done / reported_done_at / report_reason: whether the
            agent called `report_done` — its own "my work is finished"
            signal — and what it said. Present on live rows AND on exited
            ones, so "finished, then exited" is distinguishable from
            "stopped mid-task".
        status ∈ {"starting", "working", "awaiting_input", "reported",
            "exited"} — the legible summary; branch on this instead of
            decoding (state, idle) yourself. "reported" means the agent
            declared itself done and has had no new input since — it is
            the one status that means completion. "awaiting_input" means
            only that it stopped talking. See the status legend on
            `wait_for_session_idle`.
        managed_by_uid: the session that spawned this one (None if
            operator-spawned) — the parent link for orchestration.
        task_id / workspace_id: grouping keys (see `list_sessions_grouped`).
        global_perms: true if THIS session is a privileged orchestrator.
        continuous_task_id: set if this session belongs to a continuous task.
        worktree_path: the checkout the session runs in.
        workspace_shared_with: PRESENT ONLY when another live session is
            running in the same `worktree_path` — a list of
            `{session_uid, label}` for those others. Absent means the
            session has that checkout to itself. Treat its presence as
            "concurrent edits are possible here": coordinate before you
            edit shared files (or `send_input` to the other session)
            rather than assuming the working tree is yours. Exited
            sessions never appear in it and never receive it.
    (Terminal geometry — `cols`/`rows` — is intentionally omitted; it's
    noise for orchestration.)
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

    The per-session dicts carry `workspace_shared_with` on the same terms
    as `list_sessions` (see there). It is worth reading even though the
    output is already grouped: the grouping key is `workspace_id`, and
    two DIFFERENT workspaces can point at the same checkout (an in-place
    launch, or a workspace re-created around an existing worktree), so a
    shared checkout does not always show up as a shared group.
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
async def start_session(
    type: str,
    label: str,
    prompt: str = "",
    task_id: str | None = None,
    global_perms: bool = False,
    wait: bool = False,
    timeout_s: float = 600.0,
    poll_interval_s: float = 2.0,
    pending_idle_grace_s: float = 8.0,
    schema: dict | None = None,
    schema_retries: int = 1,
    isolated: bool = False,
    notify_on_done: bool = True,
    notify_until: str = "turn_end",
) -> dict:
    """Spawn a new agent or shell session in your workspace.

    With `wait=true` this is the one-call "spawn a worker, get its answer"
    primitive — the cm analogue of Claude Code's `Agent(prompt)`: it
    spawns, waits for the initial `prompt`'s reply, and returns the
    worker's final message inline, so you don't have to hand-write the
    spawn → `wait_for_session_idle` → `read_last_turn` sequence (and its
    post-send race). Leave `wait=false` (default) for fire-and-forget
    spawns you'll drive yourself. `isolated=true` additionally gives the
    worker its own git worktree in one call — the analogue of
    `Agent(isolation="worktree")`.

    Args:
        type: "claude-code", "codex", or "bash". A bash session is a raw
            shell in the workspace's worktree — useful when you want to
            drive a terminal and have the user share it. No MCP
            injection, no transcript, but `send_input` and
            `wait_for_session_idle` still work (writes raw bytes + Enter
            to the PTY; idle flips via the same burst detector as
            agents). A bash session has no transcript, so `wait=true`
            returns after it goes quiet with `last_message=null`.
        label: Sidebar label for the new session.
        prompt: Optional initial prompt to deliver once the session is
            ready (queued via the same PendingWrite drainer the workflow
            activation flow uses). For a bash session this is just a
            command line. Required in practice when `wait=true` — there's
            nothing to wait for otherwise.
            **Omit it to launch a planning task as written**: when
            `prompt` is empty/absent AND you named a task EXPLICITLY
            (the `task_id` arg, or the one `isolated=true` mints), the
            server looks up that task's stored description + prompt and
            delivers THAT to the worker — the same text the operator's
            launch key would have sent. A binding merely INHERITED from
            your own session does not trigger this: a promptless spawn
            without `task_id` stays promptless (the "spawn now, drive
            with send_input later" pattern). So `start_session(type=...,
            label=..., task_id=<backlog task>)` is a complete launch;
            you don't have to fetch the task and re-type its prompt.
            The response's `prompt_source` tells you what happened
            ("caller" / "task" / "none"). The lookup is best-effort: a
            task with no stored prompt, or an unreachable planning API,
            just spawns the worker promptless — it never fails the
            spawn. Agents only: a `bash` session's prompt is a command
            line the shell would EXECUTE, so bash never auto-delivers.
        task_id: Optional task to bind to. Omitted = your own task (if
            any) or no task. You may bind to your own task, any of its
            descendants, or a task YOU created via `propose_task` /
            `create_subtask` (the daemon records a creator edge at mint
            time, so propose-then-launch works). Any other cross-task
            binding is rejected — UNLESS you hold global permissions, in
            which case you may bind the child to any task. A proposed
            task has no worktree of its own, so its worker spawns in
            YOUR workspace.
        global_perms: Grant the new session global permissions (it can
            then prompt/read/control ANY session). **Only honored if YOU
            already hold global permissions** (check `ping().global_perms`);
            a non-global caller passing this gets `unauthorized`. This is
            how a privileged orchestrator spins up more privileged
            workers. Leave false for ordinary, scoped children.
        isolated: Give the worker its OWN git worktree instead of sharing
            the caller's. One flag instead of the two-step
            `create_subtask(worktree_mode="branch")` + `start_session(
            task_id=...)` dance — the cm analogue of Claude Code's
            `Agent(isolation="worktree")`. Under the hood it branches a
            subtask off your current task (so you must have a bound task),
            creates its worktree, and binds the new session there. The
            returned dict then also carries `task_id` (the subtask) and
            `worktree_path` — merge that branch and call
            `mark_subtask_done(task_id)` when finished. An explicit `task_id`
            arg is ignored when isolated (the subtask is always a child of
            YOUR task).
        wait: Block until the spawned worker replies to `prompt`, then
            return its final message (see Returns). Default false.
        timeout_s / poll_interval_s / pending_idle_grace_s: Waiting knobs,
            same semantics as `send_input_and_wait`. Only used when wait.
        schema: Optional JSON Schema for structured output (only meaningful
            with wait). The initial prompt is decorated with a "reply with
            ONLY JSON matching this schema" instruction; the reply is
            parsed + validated at the tool layer, with up to
            `schema_retries` re-prompts on a miss. The parsed value comes
            back as `result`.
        schema_retries: Max re-prompts on a schema miss. Default 1.
        notify_on_done: With wait=false and a prompt to run, auto-register
            an async monitor on the new worker (default true): when it
            finishes the prompt, a `[cm-monitor ...]` message is
            delivered into YOUR session with its reply — so END YOUR
            TURN after spawning; don't poll and don't park in blocking
            `wait_*` calls. An auto-delivered task prompt (see `prompt`,
            `prompt_source == "task"`) counts, so a task launch wakes
            you too. Ignored for bash sessions, when wait=true, or when
            the worker was spawned with no prompt at all.
        notify_until: What the auto-monitor waits for — "turn_end"
            (default: the worker finishing this prompt) or "final" (only
            an exit or the worker's own `report_done`, with interim turn
            ends silently re-arming the watch). Use "final" for a worker
            you expect to run a multi-turn job on its own and want to
            hear from ONCE, when it is genuinely finished. See
            `monitor_sessions`.

    Returns:
        Every spawn returns, alongside the shape below:
          - `worktree_path` — the checkout the new session actually
            landed in. Worth reading even when you didn't pass
            `task_id`: binding to a branch-mode subtask spawns the
            worker in ITS worktree, not yours.
          - `task_id` — the task the new session is bound to, when it is
            bound to one.
          - `prompt_source` — "caller" (your `prompt`), "task" (the
            bound task's stored prompt, auto-delivered because you
            passed none), or "none" (spawned promptless).

        - wait=false: {"session_uid": "<uid>"} for the freshly-spawned
          session, plus `monitor` when one was auto-registered (see
          notify_on_done).
        - wait=true: {session_uid, completed, timed_out, status, state,
          idle, last_message}. `last_message` is the worker's final
          assistant message (null for a bash/transcript-less session, or on
          timeout before any reply). `session_uid` is ALWAYS present so you
          can fall back to polling a slow worker that outran `timeout_s`.
          With `schema`, also `result` (parsed value or null) and
          `schema_error` (null on success, else the reason).
        - isolated=true: `task_id` / `worktree_path` name the branched
          subtask and its fresh checkout, whatever `wait` is.

    State your intent in plain language and ask the user to confirm
    before calling this tool. The user expects to be in the loop on
    every spawn — doubly so when `global_perms=true`, which mints a child
    that can reach across the whole session graph.
    """
    # isolated: branch a subtask worktree under the caller's task, then bind
    # the new session there. Folds the create_subtask(branch)+start_session
    # dance into one flag. Requires a bound task (create_subtask enforces it).
    isolated_task_id: str | None = None
    isolated_worktree: str | None = None
    if isolated:
        try:
            sub = await asyncio.to_thread(
                control_client.call,
                "create_subtask",
                {"name": label, "worktree_mode": "branch"},
            )
        except control_client.ControlError as e:
            return {
                "error": e.code,
                "message": (
                    f"isolated=true needs its own worktree, which requires a "
                    f"bound task to branch from: {e.message}"
                ),
            }
        isolated_task_id = sub.get("task_id") if isinstance(sub, dict) else None
        isolated_worktree = sub.get("worktree_path") if isinstance(sub, dict) else None
        # The isolated subtask is the session's task, overriding any arg.
        task_id = isolated_task_id

    body = prompt
    if schema is not None and prompt:
        body = prompt + _schema_instruction(schema)
    elif schema is not None:
        body = _schema_instruction(schema).lstrip("\n-")

    params: dict = {"type": type, "label": label}
    if body:
        params["prompt"] = body
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
    spawn = await asyncio.to_thread(
        control_client.call, method, params, socket_path=route.path
    )

    # ux-5c: the server tells us which prompt (if any) the child was
    # actually handed — "caller" (the `prompt` arg), "task" (the task's
    # own stored prompt, auto-delivered because we passed none while
    # naming a task_id), or "none". When present it is AUTHORITATIVE:
    # the server treats a whitespace-only `prompt` as absent, so keying
    # off our raw argument would register a done-monitor against a
    # worker that was never handed anything (it can never fire, and the
    # orchestrator parks on it). Older servers omit the field; only
    # then fall back to what we sent, with the same blank-normalization.
    prompt_source = spawn.get("prompt_source") if isinstance(spawn, dict) else None
    if prompt_source is not None:
        delivered_a_prompt = prompt_source in ("caller", "task")
    else:
        delivered_a_prompt = bool(prompt and prompt.strip())

    def _with_isolation(d: dict) -> dict:
        """Tag the subtask task_id / worktree_path onto a result so the
        caller can later merge the branch and `mark_subtask_done`.

        ux-1c: the server now returns `worktree_path` (always) and
        `task_id` (when bound) on EVERY spawn, not just isolated ones —
        so this only has to fill in the isolated-specific values, and
        must not clobber a real server-supplied path with a None.
        """
        if isolated and isinstance(d, dict):
            if isolated_task_id:
                d["task_id"] = isolated_task_id
            if isolated_worktree:
                d["worktree_path"] = isolated_worktree
        return d

    def _with_spawn_fields(d: dict) -> dict:
        """Carry the spawn call's descriptive fields onto a result dict
        built elsewhere (the wait path's `_await_reply` result), so
        `wait=true` callers see the same `worktree_path` / `task_id` /
        `prompt_source` that `wait=false` callers get."""
        if isinstance(d, dict) and isinstance(spawn, dict):
            for key in ("worktree_path", "task_id", "prompt_source"):
                if spawn.get(key) is not None and d.get(key) is None:
                    d[key] = spawn[key]
        return d

    if not wait:
        spawn_uid = spawn.get("session_uid") if isinstance(spawn, dict) else None
        if (
            notify_on_done
            and spawn_uid
            # ux-5c: an auto-delivered task prompt is a turn the worker
            # will run and finish, so it deserves the same wake-me-up
            # monitor a caller-supplied prompt gets.
            and delivered_a_prompt
            and type != "bash"
        ):
            try:
                spawn["monitor"] = async_monitor.register_monitor(
                    [spawn_uid], mode="any", until=notify_until,
                    note=(
                        f"worker '{label}' reported done"
                        if notify_until in ("final", "task_done")
                        else f"worker '{label}' finished its initial prompt"
                    ),
                    source="auto",
                )
            except async_monitor.RegistrationError as e:
                spawn["monitor"] = {"error": e.code, "message": str(e)}
        return _with_isolation(spawn)

    session_uid = spawn.get("session_uid") if isinstance(spawn, dict) else None
    if not session_uid:
        # Nothing to wait on — hand back whatever spawn returned.
        return _with_isolation(spawn)

    deadline = time.monotonic() + max(1.0, min(timeout_s, 86400.0))
    interval = max(0.5, min(poll_interval_s, 30.0))
    grace = max(1.0, min(pending_idle_grace_s, 60.0))
    # Fresh session: no transcript bound yet, so anchor on None — every
    # assistant message it writes is the reply. `_await_reply` picks up the
    # transcript path the moment the detector binds it.
    res = await _await_reply(
        session_uid,
        engine="claude-code", transcript_path=None,
        anchor_cursor=None, generation=0,
        deadline=deadline, interval=interval, grace=grace,
    )
    res["session_uid"] = session_uid
    if schema is not None:
        res = await _settle_schema(
            session_uid, res, schema, schema_retries,
            deadline=deadline, interval=interval, grace=grace,
        )
    return _with_isolation(_with_spawn_fields(res))


@mcp.tool()
async def send_input(
    session_uid: str,
    text: str,
    submit: bool = True,
    notify_on_done: bool = True,
    notify_until: str = "turn_end",
) -> dict:
    """Deliver a prompt to a session you can see, and (by default) get
    woken asynchronously when that session finishes the turn.

    With `notify_on_done=true` (the default) this auto-registers an
    async monitor on the target: when it finishes the turn this prompt
    starts (edge-triggered against its transcript at send time, so a
    still-idle target can't instant-fire its stale previous reply), a
    `[cm-monitor ...]` message is delivered into YOUR session with its
    final reply — so END YOUR TURN after sending; don't poll and don't
    park in blocking `wait_*` calls. If you need the reply
    synchronously in this same tool-turn, use `send_input_and_wait`
    instead.

    Args:
        session_uid: Target session's stable UID (from list_sessions).
        text: Body to send.
        submit: True (default) appends Enter so the agent receives the
            input as a fresh keystroke; the body and Enter are separated
            in time by the TUI drainer to avoid the body being seen as
            a multi-line paste.
        notify_on_done: Register the self-waking monitor (default
            true). Set false for pure fire-and-forget (e.g. a nudge you
            don't care to hear back about, or when you're already
            watching the session another way).
        notify_until: What that monitor waits for — "turn_end" (default:
            the reply to THIS prompt) or "final" (only an exit or the
            target's own `report_done`, with interim turn ends re-arming
            the watch). Use "final" when the prompt kicks off work that
            will span several turns. See `monitor_sessions`.

    State your intent and ask the user before calling this tool.
    """
    res = await asyncio.to_thread(
        control_client.call,
        "send_input",
        {"session_uid": session_uid, "text": text, "submit": submit},
    )
    if notify_on_done and isinstance(res, dict) and "error" not in res:
        try:
            res["monitor"] = async_monitor.register_monitor(
                [session_uid], mode="any", until=notify_until,
                note=(
                    f"{session_uid} reported done"
                    if notify_until in ("final", "task_done")
                    else f"reply to your send_input to {session_uid}"
                ),
                source="auto",
            )
        except async_monitor.RegistrationError as e:
            res["monitor"] = {"error": e.code, "message": str(e)}
    return res


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

    The tombstone also records WHO killed it (you), so anything that
    later reports on that session — `list_sessions(include_exited=true)`,
    a monitor's wake-up message — says "killed by <your uid>" instead of
    quoting the half-finished transcript tail as if it were the agent's
    final report.

    Errors: killing a session that is already gone returns `not_found`
    with a message naming when it exited ("session '<uid>' already
    exited at <ts>") rather than implying a bad uid. That is a no-op,
    not a failure — the session you wanted closed is closed.

    Ask the user before calling. Killing a session is destructive —
    pending work in that agent stops.
    """
    return control_client.call("kill_session", {"session_uid": session_uid})


# Advisory attached whenever a read/wait comes back with no readable output
# because no transcript is bound. Turns the old silent null/[] — the bash
# read dead-end an agent hits and can't explain — into a clear next step.
_NO_TRANSCRIPT_NOTE = (
    "No transcript is bound for this session, so there is no readable output "
    "here. If this is a bash session it will NEVER bind one — a bash PTY's "
    "output is not exposed through the MCP; redirect the command to a file "
    "(e.g. `cmd > /tmp/out.txt 2>&1`) and read that, or spawn a "
    "type='claude-code'/'codex' agent for readable transcript output. If this "
    "is a freshly-spawned agent, its transcript may just not have bound yet — "
    "poll again."
)


# Outcome fields the daemon puts on a `resolve_authorized_session` reply:
# how the session ended (3b) and whether its agent said it was finished
# (4a). Copied verbatim onto the read tools' results so a reader never has
# to call `list_sessions(include_exited=True)` just to learn that the tail
# it is looking at is a SIGKILL fragment rather than a conclusion.
_OUTCOME_FIELDS = (
    "killed",
    "killed_by",
    "exited_at",
    "reported_done",
    "reported_done_at",
    "report_reason",
)


def _with_outcome(out: dict, resolved: dict) -> dict:
    """Carry the resolve payload's outcome fields onto a read result."""
    for key in _OUTCOME_FIELDS:
        if resolved.get(key) is not None:
            out[key] = resolved[key]
    return out


@mcp.tool()
def read_session_output(
    session_uid: str,
    since_cursor: str | None = None,
    max_messages: int = 20,
) -> dict:
    """Read transcript messages from a session you can see.

    Low-level primitive — pages FORWARD from a cursor. For the common
    "what did it just say?" question use `read_last_turn`; reach for this
    only when you actually need to walk history in order or resume a
    forward scan from a saved cursor.

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
        - status: "starting"|"working"|"awaiting_input"|"reported"|
          "exited" — the legible summary; "reported" means the agent
          called `report_done`.
        - When state="pending", messages is empty and you can poll again.
        - note: present only when no transcript is bound (messages empty).
          Explains the bash-session read dead-end and what to do instead.
        - Outcome fields, present when the daemon knows them:
          reported_done / reported_done_at / report_reason (the agent's
          own "I'm finished" signal) and, for a session that has ended,
          killed / killed_by / exited_at. Read them before trusting the
          tail: a killed session's last message is wherever the SIGKILL
          landed, not a conclusion.
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
    reported = bool(resolved.get("reported_done", False))

    if state == "pending" or transcript_path is None:
        out = {
            "messages": [],
            "cursor": None,
            "generation": generation,
            "state": state,
            "idle": idle,
            "status": _session_status(state, idle, reported),
        }
        if transcript_path is None:
            out["note"] = _NO_TRANSCRIPT_NOTE
        return _with_outcome(out, resolved)

    parser = _parser_for(engine)
    messages, cursor = parser.read_messages(
        transcript_path,
        generation,
        since_cursor,
        max_messages,
    )
    return _with_outcome({
        "messages": [m.to_dict() for m in messages],
        "cursor": cursor,
        "generation": generation,
        "state": state,
        "idle": idle,
        "status": _session_status(state, idle, reported),
    }, resolved)


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
        - status: "starting"|"working"|"awaiting_input"|"reported"|
          "exited". "reported" is the only one that means the agent
          considers the work DONE; "awaiting_input" just means it
          stopped talking.
        - cursor: end cursor — pass to `read_session_output(since_cursor=)`
          if you later want to resume forward reads from here.
        - note: present only when no transcript is bound (last_assistant is
          null). Explains the bash-session read dead-end and what to do.
        - Outcome fields, present when the daemon knows them:
          reported_done / reported_done_at / report_reason — the agent's
          own done signal and its one-line reason — and, once the session
          has ended, killed / killed_by / exited_at. The pair answers the
          question this tool is usually asked in service of: is the text
          below a conclusion, or wherever the process happened to stop?
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
    status = _session_status(
        state, idle, bool(resolved.get("reported_done", False))
    )

    if transcript_path is None:
        return _with_outcome({
            "last_assistant": None,
            "messages": [],
            "status": status,
            "state": state,
            "idle": idle,
            "generation": generation,
            "cursor": None,
            "note": _NO_TRANSCRIPT_NOTE,
        }, resolved)

    n = max(0, min(context_messages, 100))
    messages, cursor = _read_all_messages(engine, transcript_path, generation)
    tail = messages[-n:] if n else []
    return _with_outcome({
        "last_assistant": _last_assistant(messages),
        "messages": [m.to_dict() for m in tail],
        "status": status,
        "state": state,
        "idle": idle,
        "generation": generation,
        "cursor": cursor,
    }, resolved)


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

    Low-level primitive. For the common cases prefer the front door:
    `start_session(wait=true)` to spawn a worker and get its first reply,
    `send_input_and_wait` to drive an existing session and get the reply,
    or `wait_for_any_session_idle` to watch a fan-out of workers. Better
    still, don't block at all: `start_session`/`send_input` auto-register
    an async monitor (see `monitor_sessions`) that wakes you when the
    worker finishes — end your turn and let it fire. Use this bare wait
    only for a session you did NOT just prompt (no post-send race to
    close), e.g. polling one you handed off earlier.

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
    # Streak clock for the transcript-shape semantic-idle fallback
    # (ready + PTY-busy + transcript says turn complete).
    semantic_idle_since = None
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
        # The agent's own done signal, so this wait's `status` agrees with
        # what `list_sessions` says about the same session.
        reported = bool(resolved.get("reported_done", False))
        now = time.monotonic()
        if state == "exited":
            return {
                "idle": True, "timed_out": False, "state": state,
                "status": _session_status(state, idle, reported),
            }
        # A READY session (transcript bound) that's quiet is unambiguously
        # done with its turn — return immediately.
        if state == "ready" and idle:
            return {
                "idle": True, "timed_out": False, "state": state,
                "status": _session_status(state, idle, reported),
            }
        # READY but PTY-busy: a background task's spinner keeps the PTY
        # noisy while the agent is at the prompt (false-busy). Consult
        # the transcript shape, debounced — see monitor.py. A daemon
        # hook-confirmed `semantic_idle` skips the debounce.
        if state == "ready":
            if await asyncio.to_thread(
                transcript_turn_complete,
                resolved.get("engine", "claude-code"),
                resolved.get("transcript_path"),
            ):
                if semantic_idle_since is None:
                    semantic_idle_since = now
                if resolved.get("semantic_idle") is True or (
                    now - semantic_idle_since >= SEMANTIC_IDLE_GRACE_S
                ):
                    return {
                        "idle": True, "timed_out": False, "state": state,
                        "status": "reported" if reported else "awaiting_input",
                        "idle_source": "transcript",
                    }
            else:
                semantic_idle_since = None
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
                "status": _session_status(state, idle, reported),
            }
        else:
            pending_idle_since = None
        if now >= deadline:
            return {
                "idle": False, "timed_out": True, "state": state,
                "status": _session_status(state, idle, reported),
            }
        await asyncio.sleep(interval)


# ── Structured-output (schema=) helpers ────────────────────────────────
#
# The spawn-and-run / send-and-wait tools can take a `schema` (a JSON
# Schema). When set, the outgoing prompt is decorated with an instruction
# to reply with ONLY a JSON value matching it; the reply is then parsed and
# validated at the tool layer, and on mismatch the worker is re-prompted up
# to `schema_retries` times. This mirrors the Workflow `schema` option and
# turns a free-running Claude/Codex worker into a structured function for
# orchestration (no free-text parsing on the caller side).

_JSON_FENCE_RE = re.compile(r"```(?:json)?\s*(.*?)\s*```", re.DOTALL | re.IGNORECASE)


def _schema_instruction(schema: dict) -> str:
    """Suffix appended to a prompt to steer the worker to JSON-only output."""
    return (
        "\n\n---\nSTRUCTURED OUTPUT REQUIRED. End your turn with ONLY a "
        "single JSON value that matches this JSON Schema — no prose after "
        "it, no markdown fences:\n" + json.dumps(schema, indent=2)
    )


def _schema_correction(err: str, schema: dict) -> str:
    """Re-prompt sent when a reply failed schema validation."""
    return (
        f"Your previous reply did not satisfy the required JSON output: {err}. "
        "Reply again with ONLY a single JSON value matching this schema, "
        "nothing else:\n" + json.dumps(schema, indent=2)
    )


def _first_json_span(text: str) -> str | None:
    """Return the first balanced {...} or [...] span in `text`, or None.
    String-aware (braces inside quoted strings don't count), so it survives
    JSON embedded in surrounding prose."""
    start = None
    for i, ch in enumerate(text):
        if ch in "{[":
            start = i
            break
    if start is None:
        return None
    depth = 0
    in_str = False
    esc = False
    for j in range(start, len(text)):
        ch = text[j]
        if in_str:
            if esc:
                esc = False
            elif ch == "\\":
                esc = True
            elif ch == '"':
                in_str = False
            continue
        if ch == '"':
            in_str = True
        elif ch in "{[":
            depth += 1
        elif ch in "}]":
            depth -= 1
            if depth == 0:
                return text[start:j + 1]
    return None


def _extract_json(text: str) -> tuple[object, str | None]:
    """Best-effort pull of a single JSON value out of an assistant message.
    Tries, in order: fenced ```json blocks (last first — models often
    explain, then emit), the whole trimmed message, then the first balanced
    brace/bracket span. Returns (value, None) on success or (None, error)."""
    if not text or not text.strip():
        return None, "empty reply"
    candidates: list[str] = []
    candidates.extend(reversed(_JSON_FENCE_RE.findall(text)))
    candidates.append(text.strip())
    span = _first_json_span(text)
    if span is not None:
        candidates.append(span)
    for c in candidates:
        try:
            return json.loads(c), None
        except (ValueError, TypeError):
            continue
    return None, "no valid JSON found in reply"


def _minimal_validate(value: object, schema: dict) -> str | None:
    """Dependency-free fallback validator: checks top-level `type` and
    (for objects) `required`. Returns None if OK, else an error string.
    Used only when `jsonschema` is not installed."""
    t = schema.get("type")
    checks = {
        "object": (dict, "a JSON object"),
        "array": (list, "a JSON array"),
        "string": (str, "a string"),
        "boolean": (bool, "a boolean"),
    }
    if t in checks:
        py_t, label = checks[t]
        if not isinstance(value, py_t):
            return f"expected {label}, got {type(value).__name__}"
    elif t == "integer":
        if isinstance(value, bool) or not isinstance(value, int):
            return f"expected an integer, got {type(value).__name__}"
    elif t == "number":
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            return f"expected a number, got {type(value).__name__}"
    if isinstance(value, dict):
        for key in schema.get("required", []) or []:
            if key not in value:
                return f"missing required field: {key!r}"
    return None


def _validate_schema(value: object, schema: dict | None) -> str | None:
    """Return None if `value` conforms to `schema`, else a short error
    string. Uses `jsonschema` when available; otherwise `_minimal_validate`.
    A falsy/empty schema accepts any JSON value."""
    if not isinstance(schema, dict) or not schema:
        return None
    if _jsonschema is not None:
        try:
            _jsonschema.validate(value, schema)
            return None
        except _jsonschema.ValidationError as e:
            return f"schema validation failed: {e.message}"
        except _jsonschema.SchemaError as e:
            return f"invalid schema supplied: {e.message}"
    return _minimal_validate(value, schema)


# ── Shared spawn/send → await-reply core ───────────────────────────────


async def _await_reply(
    session_uid: str,
    *,
    engine: str,
    transcript_path: str | None,
    anchor_cursor: str | None,
    generation: int,
    deadline: float,
    interval: float,
    grace: float,
) -> dict:
    """Poll a session until its next reply lands and it goes quiet, or the
    deadline passes. Completion is anchored on transcript progress past
    `anchor_cursor` (a NEW assistant message must appear), so it can't
    return the turn that was already there when polling began.

    Shared by `send_input_and_wait` (anchor = the transcript end captured
    just before the send) and `start_session(wait=True)` (anchor = None on
    a fresh session whose transcript hasn't bound yet — everything it
    writes is new). Transcript-less sessions (bash, detector-less daemon)
    fall back to the bounded pending-idle wait with no message, exactly
    like `wait_for_session_idle`.

    Returns {completed, timed_out, status, state, idle, last_message}. The
    caller frames it (adds `delivered` / `session_uid` / schema keys)."""
    cursor = anchor_cursor
    saw_new_assistant = False
    last_message: dict | None = None
    pending_idle_since: float | None = None
    semantic_idle_since: float | None = None
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
            # `wait_for_session_idle`). Read the final reply from the
            # captured path — the file persists past exit.
            if resolved_once and getattr(e, "code", None) == "not_found":
                last_message = await asyncio.to_thread(_final_read)
                return {
                    "completed": True, "timed_out": False,
                    "status": "exited", "state": "exited", "idle": True,
                    "last_message": last_message,
                }
            raise
        resolved_once = True
        state = resolved.get("state", "pending")
        idle = bool(resolved.get("idle", False))
        # Same as the wait loop above: the agent's own done signal, so the
        # `status` this returns agrees with what a listing would say.
        reported = bool(resolved.get("reported_done", False))
        # A fresh session's transcript may bind only AFTER polling begins —
        # pick it up the moment it appears.
        if transcript_path is None and resolved.get("transcript_path"):
            transcript_path = resolved.get("transcript_path")
            engine = resolved.get("engine", engine)
            generation = int(resolved.get("generation", generation))
        now = time.monotonic()

        if state == "exited":
            last_message = await asyncio.to_thread(_final_read)
            out = {
                "completed": True, "timed_out": False,
                "status": "exited", "state": "exited", "idle": True,
                "last_message": last_message,
            }
            if last_message is None and transcript_path is None:
                out["note"] = _NO_TRANSCRIPT_NOTE
            return out

        if transcript_path is not None:
            # Transcript-anchored: pull new messages since the anchor, latch
            # the reply, complete only when a NEW assistant message exists
            # AND the session is quiet at the prompt.
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
                    "completed": True, "timed_out": False,
                    "status": _session_status(state, idle, reported),
                    "state": state, "idle": idle, "last_message": last_message,
                }
            # New assistant reply + PTY still noisy: a background
            # task's spinner can hold `idle` false forever. Fall back
            # to transcript shape (debounced) — see monitor.py. A
            # daemon hook-confirmed `semantic_idle` skips the debounce.
            if saw_new_assistant and state == "ready":
                if await asyncio.to_thread(
                    transcript_turn_complete, engine, transcript_path
                ):
                    if semantic_idle_since is None:
                        semantic_idle_since = now
                    if resolved.get("semantic_idle") is True or (
                        now - semantic_idle_since >= SEMANTIC_IDLE_GRACE_S
                    ):
                        return {
                            "completed": True, "timed_out": False,
                            "status": (
                                "reported" if reported else "awaiting_input"
                            ),
                            "state": state, "idle": idle,
                            "last_message": last_message,
                            "idle_source": "transcript",
                        }
                else:
                    semantic_idle_since = None
        else:
            # Transcript-less fallback == `wait_for_session_idle` semantics.
            if state == "ready" and idle:
                return {
                    "completed": True, "timed_out": False,
                    "status": _session_status(state, idle, reported),
                    "state": state, "idle": idle, "last_message": None,
                    "note": _NO_TRANSCRIPT_NOTE,
                }
            if state == "pending" and idle:
                if pending_idle_since is None:
                    pending_idle_since = now
                elif now - pending_idle_since >= grace:
                    return {
                        "completed": True, "timed_out": False,
                        "status": _session_status(state, idle, reported),
                        "state": state, "idle": idle, "last_message": None,
                        "note": _NO_TRANSCRIPT_NOTE,
                    }
            else:
                pending_idle_since = None

        if now >= deadline:
            return {
                "completed": False, "timed_out": True,
                "status": _session_status(state, idle, reported),
                "state": state, "idle": idle, "last_message": last_message,
            }
        await asyncio.sleep(interval)


async def _send_and_await(
    session_uid: str,
    text: str,
    submit: bool,
    *,
    deadline: float,
    interval: float,
    grace: float,
) -> dict:
    """Capture the transcript anchor, deliver `text`, then await the reply
    to THIS input via `_await_reply`. Returns the `_await_reply` result with
    `delivered: True` added, or {error, message} if the session won't
    resolve. The body of the canonical `send_input_and_wait`."""
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

    await asyncio.to_thread(
        control_client.call,
        "send_input",
        {"session_uid": session_uid, "text": text, "submit": submit},
    )
    res = await _await_reply(
        session_uid,
        engine=engine, transcript_path=transcript_path,
        anchor_cursor=anchor_cursor, generation=generation,
        deadline=deadline, interval=interval, grace=grace,
    )
    res["delivered"] = True
    return res


async def _settle_schema(
    session_uid: str,
    res: dict,
    schema: dict,
    retries: int,
    *,
    deadline: float,
    interval: float,
    grace: float,
) -> dict:
    """Given a completed reply `res`, extract + validate JSON against
    `schema`. On success, set `res["result"]` to the parsed value. On
    mismatch, re-prompt (up to `retries` times, budget permitting) and
    re-await; if it still fails — or the worker has exited / timed out /
    the deadline passed — set `result=None` and `schema_error` to the
    reason. Always returns a dict carrying both keys."""
    attempts_left = max(0, int(retries))
    while True:
        content = ((res.get("last_message") or {}).get("content") or "")
        value, err = _extract_json(content)
        if err is None:
            err = _validate_schema(value, schema)
        if err is None:
            res["result"] = value
            res["schema_error"] = None
            return res
        stuck = (
            attempts_left <= 0
            or res.get("state") == "exited"
            or res.get("timed_out")
            or time.monotonic() >= deadline
        )
        if stuck:
            res["result"] = None
            res["schema_error"] = err
            return res
        attempts_left -= 1
        nxt = await _send_and_await(
            session_uid, _schema_correction(err, schema), True,
            deadline=deadline, interval=interval, grace=grace,
        )
        if "error" in nxt:
            res["result"] = None
            res["schema_error"] = f"{err}; re-prompt failed: {nxt.get('message')}"
            return res
        res = nxt


@mcp.tool()
async def send_input_and_wait(
    session_uid: str,
    text: str,
    submit: bool = True,
    timeout_s: float = 600.0,
    poll_interval_s: float = 2.0,
    pending_idle_grace_s: float = 8.0,
    schema: dict | None = None,
    schema_retries: int = 1,
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
        schema: Optional JSON Schema. When set, the prompt is decorated with
            a "reply with ONLY JSON matching this schema" instruction, and
            the reply is parsed + validated at the tool layer. On a
            validation miss the worker is re-prompted up to `schema_retries`
            times. The parsed object comes back as `result` — no free-text
            parsing on your side. Requires a transcript-bound session (an
            agent, not bash).
        schema_retries: Max re-prompts on a schema miss before giving up.
            Default 1.

    Returns: {delivered, completed, timed_out, status, state, idle,
        last_message}.
        - completed=True: the agent produced a reply and went quiet;
          `last_message` is its final assistant message {role, content,
          ts} (null for transcript-less sessions even on success).
        - completed=False, timed_out=True: no completed reply before the
          deadline (agent may be stuck, or mid-tool-call on a very long
          turn) — inspect `status`/`state`, or `read_last_turn` to see
          partial progress.
        When `schema` is set, two more keys: `result` (the parsed+validated
        JSON value, or null if it never conformed) and `schema_error` (null
        on success, else why validation failed after all retries).

    State your intent and ask the user before calling — this delivers
    input to another session, same as `send_input`.
    """
    deadline = time.monotonic() + max(1.0, min(timeout_s, 86400.0))
    interval = max(0.5, min(poll_interval_s, 30.0))
    grace = max(1.0, min(pending_idle_grace_s, 60.0))

    body = text if schema is None else text + _schema_instruction(schema)
    res = await _send_and_await(
        session_uid, body, submit,
        deadline=deadline, interval=interval, grace=grace,
    )
    if "error" in res:
        return res
    if schema is not None:
        res = await _settle_schema(
            session_uid, res, schema, schema_retries,
            deadline=deadline, interval=interval, grace=grace,
        )
    return res


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
          An entry for an EXITED session also carries exited_at, killed
          and killed_by when the daemon still holds its tombstone: when
          killed is true, last_message is the fragment the transcript
          happened to end on when the kill landed — NOT a final report.
          `idle_source: "transcript"` means the turn boundary came from
          the transcript while the PTY stayed busy, so background work
          may still be running.
        - still_running: UIDs not yet finished — pass back to keep waiting.
        - timed_out: true if the deadline passed with nothing finished.

    Blocking. To monitor a fan-out WITHOUT parking this orchestrator —
    so you stay free to handle the user or other work while workers run —
    use `monitor_sessions` instead: it registers the same watch in the
    background and delivers a wake-up message into YOUR session when it
    fires. (Prompting workers via `send_input` / spawning via
    `start_session` already auto-registers one per worker.)
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
async def monitor_sessions(
    session_uids: list[str],
    mode: str = "any",
    until: str = "turn_end",
    note: str = "",
    timeout_s: float = 1800.0,
    edge: bool = True,
) -> dict:
    """Watch sessions in the BACKGROUND and get woken when they finish —
    the non-blocking front door for orchestrators.

    Registers an async monitor and returns immediately. When the watch
    completes (a watched session finishes its turn / exits — same idle
    rules as `wait_for_any_session_idle`, including transcript-shape
    idle for spinner-noisy sessions), a `[cm-monitor <id>]` message
    carrying each finished worker's final reply is delivered into YOUR
    OWN session, waking you exactly like a user message. So after
    registering: END YOUR TURN (or keep doing unrelated work). Don't
    poll, don't call blocking `wait_*` tools, don't read the worker's
    output in a loop.

    The wake-up message says HOW each session finished, so you don't
    mistake an interruption for a conclusion:
      - a killed session reads `- <uid> (killed by <who> at <ts>)`, and
        any transcript text is labelled "last transcript fragment before
        kill" instead of being quoted as its reply;
      - a fire off transcript-shape idle notes the PTY is still active
        (background work may still be running);
      - an idle-but-alive session is flagged as possibly an interim
        turn — the agent stopped talking, which is not the same as
        reporting done;
      - a session that called `report_done` reads `- <uid> (reported)`
        with its own reason, and does NOT get the interim-turn caveat.

    You rarely need to call this directly — `start_session` (with a
    prompt) and `send_input` auto-register a monitor per worker unless
    you pass `notify_on_done=false`. Call it explicitly to watch a
    fan-out as a single "all done" event (`mode="all"`), to wait for
    genuine completion rather than the next pause (`until="final"`), to
    watch sessions you didn't just prompt, or to re-arm after a
    `cancel_monitor`.

    Args:
        session_uids: Sessions to watch (UIDs from `list_sessions`).
        mode: "any" (default) fires when the FIRST watched session
            finishes; "all" fires once when EVERY one has.
        until: WHAT counts as finished for each session.
            "turn_end" (default) — the next completed turn. Right when
                you're driving a conversation turn by turn.
            "final" — only an EXIT or an explicit `report_done`. Every
                interim turn end silently re-arms the watch, so a worker
                that pauses mid-task, asks itself a question, or ends a
                turn with background subagents still running will not
                wake you; you're woken when it is actually done. Right
                for fan-outs you want to collect once. Use a generous
                `timeout_s`, and note that a worker which never calls
                `report_done` (or a bash session, which can't) will only
                complete this watch by exiting. A final watch also holds
                its monitor slot for as long as the work takes, so watch
                a fan-out as ONE `mode="all"` monitor rather than one per
                worker if you're arming many.
            `until="task_done"` is accepted as a synonym for "final",
            and `mode="final"` as shorthand for `mode="any",
            until="final"`.
        note: Short context echoed in the wake-up message (e.g. "wave 2
            workers") so future-you knows what fired.
        timeout_s: Watch budget, default 1800 (30 min). On timeout the
            wake-up message still arrives, flagged timed-out, listing
            who is still running.
        edge: True (default) arms against each session's CURRENT
            transcript state — the monitor fires only when a watched
            session completes a NEW turn (or exits) after registration.
            A session that is already idle does NOT instant-fire its
            stale last message; it comes back in `already_idle` so you
            can `read_last_turn` it right now instead. Set false only
            if you genuinely want "fire immediately if it's already
            idle" (level-triggered).

    Returns: {monitor_id, watching, mode, until, already_idle,
    already_reported, async_note} immediately. A non-empty
    `already_idle` means those sessions are at their prompt RIGHT NOW —
    read their output directly; do not re-arm monitors on them hoping
    for a notification. `already_reported` is the `until="final"`
    equivalent: those sessions had ALREADY reported done before this
    watch armed, so that report will not fire it. The eventual result is
    ALSO retained and readable via `list_monitors` (e.g. if the wake-up
    delivery could not be verified).

    Registering a monitor is read-only-plus-a-future-self-message — no
    pre-approval needed.
    """
    try:
        return async_monitor.register_monitor(
            session_uids, mode=mode, until=until, note=note,
            timeout_s=timeout_s, source="explicit", edge=edge,
        )
    except async_monitor.RegistrationError as e:
        return {"error": e.code, "message": str(e)}


@mcp.tool()
def list_monitors() -> dict:
    """List this session's async monitors (active and completed), with
    each completed monitor's retained result. Read-only.

    Use to check what you're still waiting on, or to pick up a result
    whose wake-up delivery failed (state="undelivered")."""
    return async_monitor.list_monitors()


@mcp.tool()
def cancel_monitor(monitor_id: str) -> dict:
    """Cancel an async monitor. Terminal from any state: the watch
    stops, an in-flight wake-up delivery is aborted, and any pending
    turn-boundary inbox message is purged — nothing arrives after a
    cancel. Already-retained results stay readable via `list_monitors`.

    Pass `monitor_id="all"` to cancel EVERY live monitor at once — the
    one-call off switch when the user asks to stop monitor
    notifications. (Remember `send_input` / `start_session` auto-register
    a fresh monitor per prompt; pass `notify_on_done=false` there to
    keep them off.)"""
    return async_monitor.cancel_monitor(monitor_id)


@mcp.tool()
def create_subtask(
    name: str,
    prompt: str = "",
    worktree_mode: str = "inherit",
    project: str | None = None,
    base: str | None = None,
) -> dict:
    """Create a subtask under your current task. Creates the task (and,
    in branch mode, its worktree) and STOPS — nothing is running until
    you `start_session` on it.

    The new task gets `parent_task_id` set to your task. You must have
    a bound task (workflow- or planning-launched session). Taskless
    callers (`A-n`) should use `propose_task` for top-level tasks.

    Args:
        name: Display name for the subtask. Slugified for the branch
            name (alphanumerics + dash, max 40 chars).
        prompt: Optional initial prompt for the subtask's worker.
            Recorded on the task row (that's where the planning view
            reads it from); since this call spawns nothing, pass the
            prompt to `start_session` too if you want it delivered.
        worktree_mode: "inherit" (default) — same worktree as parent;
            sessions spawn in the parent's worktree directory.
            "branch" — new worktree branched off the parent's
            wip_branch with name `cm-sub/<slug-chain>-<short_id>`.
            "in-place" — spawn directly in the parent's MAIN repo
            checkout: no new worktree, no new branch.
        base: Optional committish the new branch is cut from, REPLACING
            the default parent-wip-branch base. Anything git resolves:
            a sha, a tag, a local branch ("main"), or a remote-tracking
            ref ("origin/main") — resolved locally first, with one
            `git fetch origin <base>` if it doesn't resolve yet. Use it
            to fork a subtask off clean upstream instead of inheriting
            the parent's in-progress work. `worktree_mode="branch"`
            ONLY: passing it with "inherit"/"in-place" is an error
            (those modes create no branch, so there is nothing to cut).
            Unresolvable bases fail with
            `base '<x>' does not resolve to a commit`.
        project: Optional explicit project; defaults to the parent's
            project.

    Returns: {"task_id": "<uuid>", "worktree_path": "<absolute path>",
    "base_sha": "<commit sha the checkout sits on, or null>",
    "launched": false}. `launched` is always false — this tool never
    spawns a session; call `start_session(task_id=<task_id>, prompt=...)`
    next to actually put an agent on it.

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
    if base:
        params["base"] = base
    result = control_client.call("create_subtask", params)
    # Version skew: a daemon predating `base` deserializes params
    # leniently, so it would ignore the argument and cut from the
    # parent's branch WITHOUT saying so. `base_sha` ships in the same
    # change, so its absence is the tell — surface it rather than let
    # the caller believe it forked from where it asked.
    if base and isinstance(result, dict) and "base_sha" not in result:
        result["warning"] = (
            f"the cm host serving create_subtask predates the `base` parameter "
            f"and IGNORED base={base!r} — the subtask was cut from the parent's "
            "branch instead. Rebuild/restart the serving host (create_subtask is "
            "TUI-routed on workstation setups, daemon-routed headless) or check "
            "the branch by hand before relying on the base."
        )
    return result


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
def mark_subtask_done(
    task_id: str, close_worktree: bool = True, force: bool = False
) -> dict:
    """Mark a subtask done. Optionally tear down its worktree.

    Args:
        task_id: The subtask. Must be your task or a descendant.
        close_worktree: When true (default) AND the subtask used
            branch mode, tombstone its sessions, run `git worktree
            remove`, and mark its workspace closed. The branch ref
            stays so merge history is preserved — prune manually with
            `git branch -d cm-sub/<slug-chain>-<short_id>` once
            you're confident.
        force: Discard an uncommitted worktree instead of refusing.
            Default false — if the subtask's branch worktree has
            uncommitted or untracked changes, this call is REFUSED with
            a clear error and nothing is torn down (the `--force` git
            remove would otherwise silently destroy that work). Merge or
            commit first (the branch ref survives regardless), or pass
            `force=true` to accept the loss. Only the working tree is at
            risk; committed work on the branch is always preserved.

    Returns: {"ok": true, "worktree_removed": bool}. On a dirty worktree
    without `force`, returns an error (`{"error": ..., "message": ...}`)
    and leaves everything intact — sessions still alive so you can go
    merge.

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
        {"task_id": task_id, "close_worktree": close_worktree, "force": force},
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
def enqueue(
    queue: str,
    payload: dict,
    dedupe_key: str = "",
    source: str = "",
) -> dict:
    """Buffer an item into a named queue for a queue-fed continuous task.

    Queues are the async input surface of Consumer-scheduled continuous tasks
    (Continuous Tasks Phase 4): producers enqueue free-form JSON payloads; the
    consuming task's scheduler claims them in batches (when the queue is deep
    enough, or a batching window has elapsed) and delivers them to the
    orchestrator as a staged batch file. Example: pushing a scraper-creation
    proposal into `scraper-creation-proposals`.

    The payload schema is a soft convention between you and the consuming
    task's prompt — the daemon never parses it. Keep items small (proposals /
    pointers, not blobs; 64 KiB cap).

    Args:
        queue: Queue name ([A-Za-z0-9_-], ≤128 chars). Enqueueing to a queue
            no task consumes is allowed (items wait).
        payload: Free-form JSON object for the consumer.
        dedupe_key: Optional coalescing key — if an item with the same key is
            already pending/claimed in this queue, this enqueue is dropped as
            a duplicate ({"deduped": true}). After the earlier item is
            consumed, the same key may enqueue again.
        source: Optional provenance label; defaults to your session identity.

    Returns:
        {"enqueued": bool, "deduped": bool, "id": str | None, "depth": int}
        where depth is the queue's pending count after the call.

    Enqueueing only buffers data — it does not spawn anything itself, so you
    do NOT need to ask the user first (the consuming task's own schedule
    governs when work runs).
    """
    params: dict = {"queue": queue, "payload": payload}
    if dedupe_key:
        params["dedupe_key"] = dedupe_key
    if source:
        params["source"] = source
    return control_client.call("enqueue", params)


@mcp.tool()
def queue_depth(queue: str) -> dict:
    """Read a named queue's depth (Continuous Tasks Phase 4). Read-only.

    Args:
        queue: Queue name ([A-Za-z0-9_-], ≤128 chars).

    Returns:
        {"queue": str, "pending": int, "claimed": int,
         "oldest_pending_at": str | None}. `pending` items await a batch
        claim; `claimed` items are in (or stranded from) an in-flight batch.
    """
    return control_client.call("queue.stats", {"queue": queue})


@mcp.tool()
def report_done(reason: str | None = None) -> dict:
    """Signal that YOUR work is finished — call this when you're done.

    Any session can call it, and every session should: ending your turn only
    says you stopped talking. Whoever is watching you (an orchestrator, the
    continuous scheduler) cannot tell "delivered my final report" from "paused
    mid-task" or "ended a turn with background work still running" — they all
    look like `awaiting_input`. This is how you say which one it is. There is no
    session_uid argument; the daemon resolves you from your own session.

    Two effects, and you get whichever apply to you:

      1. Your session reports `status="reported"` to `list_sessions` /
         `read_last_turn` instead of the ambiguous `awaiting_input`, and any
         `monitor_sessions(..., until="final")` watching you FIRES. This is what
         lets an orchestrator arm one watch and be woken once, when you're
         actually done, instead of on every pause.
      2. If you're a continuous-task tick, it ALSO flips your active run
         Running → Done (recording finished_at), clearing the watchdog's
         stuck-detection clock so a slow-but-real run is never misread as
         wedged. That flip is scoped to the run you own: if you've been
         superseded it's a SOFT no-op with a clear message, not an error.

    The mark is superseded automatically when someone sends you new input — so
    if an orchestrator gives you follow-up work, report again when THAT is done.

    Args:
        reason: Optional one-line summary of what you finished. Worth passing:
            it is quoted in the watcher's wake-up message, so it's the first
            thing your orchestrator reads.

    Returns:
        {"ok": true, "reported": true, "reported_at": float,
         "status": "reported", "done": bool, "task_id": str|null,
         "message": str}. `done`/`task_id` are the CONTINUOUS-run flip: false /
        null when you aren't a continuous tick (nothing went wrong — effect 1
        still applied).

    This only marks your own session, so — like notify_user — you do NOT need
    to ask the user first. Just call it when your work is genuinely done.
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


# ---------------------------------------------------------------------------
# Cloud auto-backtest (submit + results)
# ---------------------------------------------------------------------------

_BACKTEST_VM_DEFAULTS = {
    "project": "prediction-market-scalper",
    "zone": "us-east4-a",
    "machine_type": "c2-standard-4",
    "image_family": "cm-backtest-worker",
    "image_project": "prediction-market-scalper",
}
# GCE instance metadata caps values at 256KB; the whole backtest payload rides
# one metadata attribute, so keep inline configs well under that.
_BACKTEST_CONFIG_MAX_BYTES = 32 * 1024
_BACKTEST_TERMINAL_STATUSES = ("done", "blocked", "archived")
_BACKTEST_DEFAULT_SCRIPT = "analysis.backtests.backtest_actrader_grid"


def _caller_task_id() -> str | None:
    """The caller's bound task UUID, or None (unbound / unreachable).

    Mirrors get_current_task's two branches. Failure to resolve identity
    must NOT block a backtest submission — the row just lands top-level.
    """
    try:
        route = control_client.resolve_socket_route()
        if route.chose_daemon:
            pong = control_client.call("ping", {}, socket_path=route.path)
            return pong.get("task_id") or None
        ctx = control_client.call("get_caller_task")
        return ctx.get("task_id") or None
    except Exception:
        return None


@mcp.tool()
def submit_backtest(
    branch: str,
    config: str,
    script: str = _BACKTEST_DEFAULT_SCRIPT,
    label: str = "",
    baseline_ref: str = "",
    notify: bool = False,
    regression: bool = False,
    repo_url: str = "",
    project: str = "predictionTrading",
    machine_type: str = "",
    zone: str = "",
) -> dict:
    """Submit a backtest to run on an ephemeral cloud spot VM.

    Creates a `kind="backtest"` task that the claude-manager dispatch
    daemon picks up: it provisions a spot worker in the same GCP
    project/zone as the market data (fast intra-zone window download),
    runs the backtest, publishes bulk results to GCS, attaches a compact
    metrics summary to the task, and tears the VM down. Read results back
    with `get_backtest_result(task_id)`.

    NOTE: this provisions a cloud VM (cheap spot instance, auto-deleted) —
    state your intent before calling. Re-submitting is NOT idempotent:
    every call creates a new task + run.

    Args:
        branch: Git ref of predictionTrading to check out and run.
        config: EITHER a repo-relative path to a run config (e.g.
            "analysis/backtests/configs/rbpf_targeted_t1.yaml") OR an
            inline YAML string (window, market_ids, params/grid). Passed
            through verbatim — the repo-side resolver disambiguates.
        script: Module path of the runner. Defaults to the canonical grid
            runner. "__cm_stub__" runs a CM-infrastructure smoke test with
            fabricated results (no repo deps).
        label: Short human label; becomes part of the run_key.
        baseline_ref: Optional stored-baseline ref for an A/B delta.
        notify: Ask for a notification on completion (delivery is owned by
            the repo-side tooling; stored on the task).
        regression: Deterministic regression mode (forces speed_factor=0).
        repo_url: Repo to clone. Defaults to the `project`'s repo URL.
        project: Planning project the task files under (board visibility;
            also the repo-URL lookup key). Must be non-empty.
        machine_type: Override the default c2-standard-4.
        zone: Override the default us-east4-a.

    Returns: {task_id, run_key, status} — run_key is minted server-side
        and keys the GCS results prefix (backtests/<run_key>/).
    """
    if not branch or not branch.strip():
        raise ValueError("submit_backtest: `branch` is required")
    if not config or not config.strip():
        raise ValueError("submit_backtest: `config` is required")
    if not project or not project.strip():
        raise ValueError("submit_backtest: `project` must be non-empty")
    if len(config.encode("utf-8")) > _BACKTEST_CONFIG_MAX_BYTES:
        raise ValueError(
            f"submit_backtest: `config` exceeds {_BACKTEST_CONFIG_MAX_BYTES} "
            "bytes — commit it to the repo and pass its path instead"
        )
    _check_parameter_confusion("label", label)
    _check_parameter_confusion("config", config)

    parent_task_id = _caller_task_id()

    try:
        client = PlanningClient()
    except Exception:
        # Headless deployment (daemon-spawned agent on cm-manager): the
        # daemon proxies the submission with its own planning creds and
        # resolves the repo_url default there.
        params: dict = {
            "branch": branch,
            "config": config,
            "script": script,
            "label": label,
            "baseline_ref": baseline_ref,
            "notify": notify,
            "regression": regression,
            "project": project,
        }
        if repo_url:
            params["repo_url"] = repo_url
        if machine_type:
            params["machine_type"] = machine_type
        if zone:
            params["zone"] = zone
        if parent_task_id:
            params["parent_task_id"] = parent_task_id
        return control_client.call("backtest.submit", params)

    if not repo_url:
        projects = {p["name"]: p["repo_url"] for p in client.list_projects()}
        repo_url = projects.get(project) or ""
        if not repo_url:
            raise ValueError(
                f"submit_backtest: unknown project {project!r} — pass "
                "repo_url explicitly"
            )

    vm = dict(_BACKTEST_VM_DEFAULTS)
    if machine_type:
        vm["machine_type"] = machine_type
    if zone:
        vm["zone"] = zone
    backtest_meta = {
        "branch": branch,
        "config": config,
        "script": script,
        "label": label,
        "baseline_ref": baseline_ref,
        "notify": notify,
        "regression": regression,
    }
    name = f"backtest: {label or script.rsplit('.', 1)[-1]} @ {branch}"
    prompt = f"Cloud backtest {script} on {branch}" + (
        f" (label: {label})" if label else ""
    )
    body = {
        "repo_url": repo_url,
        "repo_branch": branch,
        "name": name,
        "project": project,
        "prompt": prompt,
        "source": "claude",
        "is_cloud": True,
        "kind": "backtest",
        "status": "backlog",
        "metadata": {"backtest": backtest_meta, "vm": vm},
    }
    if parent_task_id:
        body["parent_task_id"] = parent_task_id
    task = client.create_task(body)
    run_key = ((task.get("metadata") or {}).get("backtest") or {}).get("run_key")
    return {"task_id": task["id"], "run_key": run_key, "status": task["status"]}


def _read_backtest_result(task_id: str) -> dict:
    """One synchronous read of task + artifacts, composed into the
    get_backtest_result shape (module-level for testability)."""
    try:
        client = PlanningClient()
        task = client.get_task(task_id)
        artifacts = client.get_task_artifacts(task_id)
    except (RuntimeError, ModuleNotFoundError):
        res = control_client.call("backtest.result", {"task_id": task_id})
        task = res["task"]
        artifacts = res["artifacts"]

    latest = artifacts[0] if artifacts else None
    task_status = task.get("status")
    if latest is None:
        status = "no_result" if task_status in _BACKTEST_TERMINAL_STATUSES else "pending"
    else:
        status = "partial" if latest.get("partial") else "complete"
    return {
        "status": status,
        "partial": bool(latest and latest.get("partial")),
        "summary": latest.get("summary") if latest else None,
        "gcs_prefix": latest.get("gcs_prefix") if latest else None,
        "artifacts": [
            {
                "id": a.get("id"),
                "kind": a.get("kind"),
                "summary": a.get("summary"),
                "gcs_prefix": a.get("gcs_prefix"),
                "partial": a.get("partial"),
                "created_at": a.get("created_at"),
            }
            for a in artifacts
        ],
        "task_status": task_status,
    }


@mcp.tool()
async def get_backtest_result(
    task_id: str,
    wait: bool = False,
    timeout_s: float = 600.0,
    poll_interval_s: float = 15.0,
) -> dict:
    """Read the structured results of a backtest submitted via `submit_backtest`.

    Returns the newest artifact attached to the task (compact metrics
    summary + a GCS pointer to the bulk results dir), plus the full
    artifact list (a run may attach a partial artifact on spot preemption
    and a final one after resume).

    `status` semantics:
      - "complete": a final (non-partial) result is available.
      - "partial": the newest result was published from an interrupted run
        (preemption / pipeline failure) — you can re-call with wait=True to
        wait for a final one if the task was re-queued.
      - "pending": no result yet, task still queued/running.
      - "no_result": task reached a terminal status (done/blocked/archived)
        without ever attaching an artifact — inspect the task / ttyd.

    Args:
        task_id: The backtest task UUID from `submit_backtest`.
        wait: Block until any artifact appears or the task goes terminal.
        timeout_s: Max seconds to wait (clamped to [1, 86400]).
        poll_interval_s: Seconds between polls (clamped to [5, 120]).

    Returns: {status, partial, summary, gcs_prefix, artifacts, task_status}
        (+ timed_out: true when a wait hit the deadline).
    """
    if not wait:
        return await asyncio.to_thread(_read_backtest_result, task_id)
    deadline = time.monotonic() + max(1.0, min(timeout_s, 86400.0))
    interval = max(5.0, min(poll_interval_s, 120.0))
    while True:
        result = await asyncio.to_thread(_read_backtest_result, task_id)
        if result["artifacts"] or result["task_status"] in _BACKTEST_TERMINAL_STATUSES:
            return result
        if time.monotonic() >= deadline:
            result["timed_out"] = True
            return result
        await asyncio.sleep(interval)


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
