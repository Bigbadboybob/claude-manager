"""MCP server for Claude instances to propose tasks to the backlog."""

import asyncio
import json
import os
import sys
import time
import uuid
from pathlib import Path

# Add project root to path so cli.planning_client is importable
sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from mcp.server.fastmcp import FastMCP

from cli.planning_client import PlanningClient
from mcp_server import control_client
from mcp_server.transcripts import claude_code as transcripts_claude
from mcp_server.transcripts import codex as transcripts_codex

mcp = FastMCP("claude-manager")


def _workflow_run_dir() -> Path:
    """Return the directory for the current workflow run, creating it if needed.

    Raises RuntimeError if CM_WORKFLOW_RUN_ID is not set (tool called outside a workflow).
    """
    run_id = os.environ.get("CM_WORKFLOW_RUN_ID", "").strip()
    if not run_id:
        raise RuntimeError(
            "CM_WORKFLOW_RUN_ID is not set — workflow tools are only usable "
            "inside a workflow-participant session."
        )
    base = Path(os.path.expanduser("~/.cm/workflow-runs")) / run_id
    base.mkdir(parents=True, exist_ok=True)
    return base


def _append_event(tool: str, args: dict) -> dict:
    """Append an MCP tool-call event to the active workflow run's events.jsonl."""
    role = os.environ.get("CM_ROLE", "").strip() or "unknown"
    run_dir = _workflow_run_dir()
    event = {
        "id": uuid.uuid4().hex,
        "ts": time.time(),
        "run_id": os.environ["CM_WORKFLOW_RUN_ID"],
        "role": role,
        "tool": tool,
        "args": args,
        # 10d-2c-1 review round-1 (P1 #1/#2): tag events written
        # through the file-write path (TUI-local SpawnTarget) so
        # the TUI tail observer knows state has NOT been mutated
        # by the daemon — it must still call
        # fire_transition / finish_run locally to apply the state
        # change. Daemon-routed events get `source: "daemon"`
        # (set by daemon's workflow_transition / workflow_done
        # handlers).
        "source": "tui-mcp",
    }
    # Append atomically: open with O_APPEND, write one line.
    events_path = run_dir / "events.jsonl"
    with events_path.open("a", encoding="utf-8") as f:
        f.write(json.dumps(event) + "\n")
        f.flush()
    return event


_BRIEF_FIELDS = (
    "id", "slug", "project", "name", "status", "source",
    "priority", "difficulty", "is_cloud",
)
_FULL_EXTRA_FIELDS = (
    "description", "prompt", "depends", "repo_url", "repo_branch",
    "wip_branch", "session_id", "ttyd_url", "worker_vm",
    "blocked_at", "created_at", "updated_at",
    "parent_task_id", "worktree_mode",
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
        from cli.planning_client import _detect_repo_url
        repo_url = _detect_repo_url()
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
    client = PlanningClient()
    return _shape_task(client.get_task(task_id), full=True)


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
) -> dict:
    """Edit a task's planning fields.

    This tool can modify ANY task — including tasks created by the human
    owner (source="user"). That power cuts both ways: be conservative when
    touching user tasks. Don't rewrite their prompt, change scope, or move
    them between statuses unless the user explicitly asked you to. Agent-
    proposed tasks (source="claude") are fair game for self-revision.

    The returned object includes the `source` field so you can confirm
    what you just modified.

    Only the fields you pass (non-None) will be updated.

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
    """
    fields = {
        k: v for k, v in {
            "name": name,
            "description": description,
            "prompt": prompt,
            "status": status,
            "priority": priority,
            "difficulty": difficulty,
            "project": project,
            "depends": depends,
        }.items() if v is not None
    }
    if not fields:
        raise ValueError("No fields to update — pass at least one field.")
    client = PlanningClient()
    return _shape_task(client.update_task(task_id, **fields), full=True)


@mcp.tool()
def ping() -> dict:
    """Check connectivity to the host TUI's control socket.

    Returns the TUI's pong response, including the `session_uid` it sees
    you as. Useful as a smoke test that the agent's MCP env was wired up
    correctly (CM_TUI_SESSION_ID + CM_TUI_SOCKET must be set by the TUI
    at spawn time).
    """
    try:
        return control_client.call("ping")
    except control_client.ControlError as e:
        return {"ok": False, "error": str(e)}
    except control_client.TransportError as e:
        return {"ok": False, "transport_error": str(e)}


@mcp.tool()
def list_sessions(
    task_id: str | None = None,
    include_exited: bool = False,
) -> list[dict]:
    """List sessions visible to you (your own task tree, or your whole
    workspace if you have no task).

    Args:
        task_id: Optional explicit task scope. Leave unset to use the
            default (your task, or whole workspace if taskless). A taskless
            caller passing `task_id` gets `unauthorized`.
        include_exited: When true, also include closed sessions
            (state=`exited`). Default false. Read-after-exit still
            works via `read_session_output` regardless of this flag.

    Returns: list of {session_uid, label, type, state, idle, managed_by_uid}.
        state ∈ {"ready", "pending", "exited"}.
    """
    params: dict = {"include_exited": include_exited}
    if task_id:
        params["task_id"] = task_id
    return control_client.call("list_sessions", params)


@mcp.tool()
def start_session(
    type: str,
    label: str,
    prompt: str = "",
    task_id: str | None = None,
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
            any) or no task. Cross-task binding is rejected.

    Returns: {"session_uid": "<uid>"} for the freshly-spawned session.

    State your intent in plain language and ask the user to confirm
    before calling this tool. The user expects to be in the loop on
    every spawn.
    """
    params: dict = {"type": type, "label": label}
    if prompt:
        params["prompt"] = prompt
    if task_id:
        params["task_id"] = task_id
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

    Returns: {messages, cursor, generation, state, idle}.
        - messages: list of {role, content, ts}
        - cursor: opaque, pass back on next call
        - state: "ready" | "pending" | "exited"
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
        }

    parser = transcripts_codex if engine == "codex" else transcripts_claude
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
    }


@mcp.tool()
def start_workflow(
    task_id: str,
    workflow_name: str,
    goal: str = "",
    role_sessions: dict[str, str] | None = None,
) -> dict:
    """Launch a workflow on a task you have authority over.

    The caller is the orchestrator — NOT a participant. By default,
    workflow participants are spawned as fresh sessions with their
    TOML-declared engine. Pass `role_sessions` to bind specific roles
    to existing sessions instead (only valid for `persistent`-context
    roles; `fresh` roles must always spawn anew). Use
    `get_workflow_state` and `read_session_output` to observe progress;
    the existing `workflow_transition` / `workflow_done` tools are for
    participants, not orchestrators.

    Args:
        task_id: Target task. Must be the caller's own task or a
            descendant in the parent_task_id tree.
        workflow_name: Workflow definition name (e.g. "feedback").
        goal: Optional initial goal string passed to the worker's
            activation prompt template ({{ goal }}).
        role_sessions: Optional map of role name → existing
            `session_uid`. Each referenced session must live in the
            target task's workspace and be in the caller's auth scope.
            The role must be `persistent`-context and its declared
            engine must match the session's engine. Roles not in this
            map are spawned fresh as usual.

    Returns: {"run_id": "<id>"}.

    State your intent and ask the user to confirm before calling.
    """
    params: dict = {"task_id": task_id, "workflow_name": workflow_name}
    if goal:
        params["goal"] = goal
    if role_sessions:
        params["role_sessions"] = role_sessions
    return control_client.call("start_workflow", params)


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

    Args:
        session_uid: Target session's stable UID.
        timeout_s: Max seconds to wait. Default 600 (10 min).
        poll_interval_s: Seconds between polls. Default 2.

    Returns: {"idle": bool, "timed_out": bool, "state":
        "ready"|"pending"|"exited"}.
        - idle=True, state="ready": agent finished its turn.
        - idle=True, state="exited": session terminated.
        - idle=False, timed_out=True: deadline reached while still busy.

    Raises ControlError on auth failure or unknown session_uid.
    """
    deadline = time.monotonic() + max(1.0, min(timeout_s, 86400.0))
    interval = max(0.5, min(poll_interval_s, 30.0))
    while True:
        resolved = await asyncio.to_thread(
            control_client.call,
            "resolve_authorized_session",
            {"session_uid": session_uid},
        )
        state = resolved.get("state", "pending")
        if state == "exited":
            return {"idle": True, "timed_out": False, "state": state}
        if bool(resolved.get("idle", False)):
            return {"idle": True, "timed_out": False, "state": state}
        if time.monotonic() >= deadline:
            return {"idle": False, "timed_out": True, "state": state}
        await asyncio.sleep(interval)


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
def workflow_transition(to: str, prompt: str) -> str:
    """Hand control to another role in the current workflow.

    Use this to end your turn and activate a different role with a specific prompt.
    The TUI delivers `prompt` to that role's session when it next activates.

    Args:
        to: Name of the role to transition to (must be declared in the workflow).
        prompt: The message to send to that role when it activates.
    """
    # 10d-2b: flip from `_append_event` (direct file write) to
    # `control_client.call` ONLY when a daemon is pinned. Under
    # TUI-local spawn (SpawnTarget::TuiLocal), the agent's env
    # has `CM_DAEMON_SOCKET=""` and no daemon serves
    # workflow_transition — the per-method resolver would fall
    # back to the TUI socket, but the TUI dispatcher has no
    # handler for these methods, so the call would round-trip
    # `UnknownMethod`. The `daemon_socket_pinned()` signal here
    # matches the resolver's `chose_daemon` decision exactly
    # (both read the same env shape), so file-write and
    # daemon-routed paths can never drift.
    if control_client.daemon_socket_pinned():
        run_id = os.environ["CM_WORKFLOW_RUN_ID"]
        role = os.environ.get("CM_ROLE", "").strip() or "unknown"
        result = control_client.call(
            "workflow_transition",
            {"to": to, "prompt": prompt, "run_id": run_id, "role": role},
        )
        return f"Queued transition to '{to}' (event {result['event_id']})."
    else:
        # TUI-local fallback: preserve pre-10d-2b behavior so
        # A-f-launched workflow participants keep working. The
        # TUI's controller tails events.jsonl exactly as today.
        event = _append_event("workflow_transition", {"to": to, "prompt": prompt})
        return f"Queued transition to '{to}' (event {event['id']})."


@mcp.tool()
def workflow_done(reason: str) -> str:
    """End the current workflow run.

    Use this when the workflow's goal is achieved and no further iteration is needed.
    All participant sessions remain open in the TUI but the workflow stops firing
    transitions.

    Args:
        reason: Short explanation of why the workflow is complete.
    """
    # 10d-2b: see `workflow_transition` above — same daemon-vs-
    # TUI-local branching rationale.
    if control_client.daemon_socket_pinned():
        run_id = os.environ["CM_WORKFLOW_RUN_ID"]
        role = os.environ.get("CM_ROLE", "").strip() or "unknown"
        result = control_client.call(
            "workflow_done",
            {"reason": reason, "run_id": run_id, "role": role},
        )
        return f"Workflow marked done (event {result['event_id']})."
    else:
        event = _append_event("workflow_done", {"reason": reason})
        return f"Workflow marked done (event {event['id']})."


if __name__ == "__main__":
    mcp.run()
