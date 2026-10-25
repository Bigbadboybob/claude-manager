"""Claude Manager MCP tools for agent sessions, tasks, and messaging."""

import asyncio
from contextlib import asynccontextmanager, suppress
import json
import os
from pathlib import Path
import re
import socket
import sys
import time
import uuid

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

from mcp_server.claude_channel import (
    CmActivateNotification, CmActivateParams, CmProxyNotification, NotificationMCP)

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

from mcp_server.launch_confirmation import await_launch_confirmation as _await_launch_confirmation
from mcp_server import control_client
from mcp_server.session_kind import is_shell, shell_refusal
from mcp_server.transcripts.types import Role
# Session status + transcript-tail helpers and the multi-session monitor
# core live in `monitor` (FastMCP-free, so the cm-wait CLI can reuse them
# without pulling in the MCP server's deps). Imported into this namespace
# so the tools below — and `from mcp_server.server import _session_status`
# in the tests — keep working unchanged.
from mcp_server.transcripts import stream as transcript_stream
from mcp_server.monitor import (
    last_assistant_message,
    _last_assistant,
    _monitor_sessions,
    _parser_for,
    _read_all_messages,
    _READ_ALL_LIMIT,
    _session_status,
    engine_state,
    engine_turn_complete,
    no_turn_at,
    _edge_passed,
    SEMANTIC_IDLE_GRACE_S,
    transcript_turn_complete,
)
from mcp_server import activation, async_monitor

# Supplied in the MCP initialization response, independent of the agent's
# project. Keep the short introduction with the deployed MCP server.
AGENT_GUIDE = Path(__file__).with_name("AGENT_GUIDE.md").read_text(encoding="utf-8")

@asynccontextmanager
async def _lifespan(_server):
    if os.environ.get("CM_TUI_SESSION_ID"):
        async_monitor._persist_best_effort()
    started: dict[str, asyncio.Task] = {}

    async def promoted():
        # Behind CM's proxy a new backend is first a CANDIDATE that may be
        # abandoned: no notification consumer, policy, monitor adoption or
        # status write until the backend it replaces has exited.
        await activation.wait_promoted()
        if not os.environ.get("CM_TUI_SESSION_ID"):
            return
        if ((_server.channel or os.environ.get("CLAUDE_CODE_MESSAGING_SOCKET"))
                and os.environ.get("CM_AGENT_ENGINE") != "codex"):
            from mcp_server.native_claude import run
            started["bridge"] = asyncio.create_task(
                run(_server.channel), name="cm-native-notifications")
        if not activation.proxy():
            return
        activation.write_status()
        await async_monitor.adopt_predecessor()
        from mcp_server.notifications import Queue
        started["policy"] = asyncio.create_task(activation.Policy(
            _send_activate, control_client.call, Queue.own,
            lambda: len(async_monitor._active_monitors()),
        ).run(), name="cm-mcp-activation")
        await _send_proxy("ready")  # the proxy stops holding client input

    promotion = asyncio.create_task(promoted(), name="cm-mcp-promotion")
    try:
        yield {}
    finally:
        promotion.cancel()
        if "policy" in started:
            started["policy"].cancel()
        # Shutdown retains unfinished watch obligations for continuous drain.
        for record in async_monitor._MONITORS.values():
            if record["state"] in ("watching", "fired"):
                record["state"] = "interrupted"
        if os.environ.get("CM_TUI_SESSION_ID"):
            async_monitor._persist_best_effort()
        bridge = started.get("bridge")
        if bridge:
            bridge.cancel()
            with suppress(asyncio.CancelledError):
                await bridge


mcp = NotificationMCP("claude-manager", instructions=AGENT_GUIDE, lifespan=_lifespan)


async def _send_activate(mode: str) -> None:
    """Ask CM's stdio proxy to activate the pending payload (never forwarded)."""
    session = mcp.client_session
    if session is None or not activation.proxy():
        return
    await asyncio.wait_for(session.send_notification(
        CmActivateNotification(params=CmActivateParams(mode=mode))), 3)


async def _send_proxy(what: str) -> None:
    """Proxy-only notifications/cm/<what> (never forwarded to the engine)."""
    session = mcp.client_session
    for _ in range(50):  # the client session exists once tools/list ran
        if session is not None:
            break
        await asyncio.sleep(0.1)
        session = mcp.client_session
    if session is None:
        return
    with suppress(Exception):
        await asyncio.wait_for(session.send_notification(CmProxyNotification(method=f"notifications/cm/{what}")), 3)


@mcp.tool()
async def ack_reconnect() -> dict:
    """Apply a pending CM MCP update now instead of waiting until you are idle.

    Optional. CM activates a deployed MCP update on its own once you have been
    idle for a minute with nothing unread; call this only if you want it
    sooner. It takes effect right after this reply, with the same connection,
    identity and chat state; your monitors and chat watches carry over and
    nothing is re-sent. Returns {activating, version, monitors_carried} or
    {activating: false, note} when nothing is pending.
    """
    result = activation.ack(len(async_monitor._active_monitors()))
    if result.get("activating"):
        await _send_activate("ack")
    return result


@mcp.tool()
async def notification_status() -> dict:
    """Inspect your native wake connection and retained notification deliveries.

    pending means no native submission; submitted is not an observed receipt.
    uncertain submissions are never automatically retried. Reading this status
    does not acknowledge chat messages. Older embedded Codex sessions need a
    deliberate CM session restart/resume to use the owned app-server.
    """
    from mcp_server.notifications import Queue
    return await asyncio.to_thread(Queue.own().snapshot)

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
    "priority", "difficulty", "is_cloud", "kind", "initiative_id",
)
_FULL_EXTRA_FIELDS = (
    "description", "prompt", "depends", "repo_url", "repo_branch",
    "wip_branch", "session_id", "ttyd_url", "worker_vm",
    "blocked_at", "created_at", "updated_at",
    "parent_task_id", "worktree_mode", "initiative_id", "initiative", "metadata",
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
                f"`description` (the whole brief) and `difficulty` are "
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


# ---- Work items and boards (doc/items-board.md) ---------------------------
# Daemon-only: the daemon knows the caller, stamps the actor and resolves
# holder names; there is no PlanningClient fallback.

# Worst case is several 8 s planning-API calls in a row, plus a spawn for
# item_resolve(launch). A client timeout shorter than that would hide a write
# that already happened, so these exceed it.
_ITEMS_TIMEOUT_S = 90.0
_ITEMS_LAUNCH_TIMEOUT_S = 180.0


_GATE_REPORT_TIMEOUT_S = 900


def _items_call(method: str, params: dict, timeout: float = _ITEMS_TIMEOUT_S) -> dict:
    params = {key: value for key, value in params.items() if value is not None}
    try:
        return control_client.call(method, params, timeout=timeout)
    except control_client.TransportError as exc:
        if "request_id" in params:
            raise control_client.TransportError(
                f"{exc} (the call may have completed; retry with request_id="
                f"{params['request_id']!r} to avoid a duplicate)"
            ) from exc
        raise


@mcp.tool()
def item(
    title: str | list[str],
    holder: str | list[str] | None = None,
    status: str | None = None,
    note: str | None = None,
    group: str | None = None,
    blocked_by: list[int] | None = None,
    blocked_on: str | None = None,
    eta: str | None = None,
    job: str | None = None,
    links: list[str] | None = None,
    board: str | None = None,
    request_id: str | None = None,
    description: str | None = None,
    group_id: str | None = None,
) -> dict:
    """Create one work item, or several from a list of titles, on your board.

    If you are doing something, it has an item. Items are small units of work
    (not planning tasks) on a shared board: your initiative's, else your
    top-level task's. The holder defaults to you; name others by chat name,
    session uid or participant id, or holder="none" to leave it unassigned.
    status: open/active/waiting/blocked/blocked_on_owner/done/dropped (default active).
    A long job: status="active" with eta ("40m", "2h" or a time) and job (what
    is running: "cm-gate pins L3 pid 41231", a cell id): no idle/stale flags
    before the eta, eta_passed after it. On a board that allows backlog a
    bare eta parks the item as waiting instead. blocked_by takes open item
    numbers on the same board (no cycles); blocked_on is free text ("EP GO")
    and gets no idle or stale exemption.

    Strict boards (backlog_allowed false, e.g. sejd) hold only running or
    blocked work: waiting, open and unheld new items are refused, a job needs
    an eta, and blocked_on needs a check_back (at most 72 h out). Future work
    there is a planning task (propose_task), not an item.

    group names a heading (a new name creates one; group_id picks one by id,
    e.g. an epic's). description is the work contract (scope, acceptance
    criteria); for a planning task already in the backlog use place(task,
    ...) instead, which links the task. You become the owner; the reply may
    carry possible_duplicate hints for similar open items.

    request_id makes a retry safe: the same id returns the first call's items
    instead of creating duplicates (one is generated when omitted and named
    in any transport error).

    Returns {board, items: [{n, title, status, holders}], replayed?}.
    """
    params = dict(locals())
    params["request_id"] = request_id or uuid.uuid4().hex
    return _items_call("item.create", params)


@mcp.tool()
def item_set(
    n: int | list[int],
    status: str | None = None,
    note: str | None = None,
    holder: str | list[str] | None = None,
    add_holder: str | list[str] | None = None,
    remove_holder: str | list[str] | None = None,
    group: str | None = None,
    blocked_by: list[int] | None = None,
    blocked_on: str | None = None,
    check_back: str | None = None,
    eta: str | None = None,
    job: str | None = None,
    title: str | None = None,
    links: list[str] | None = None,
    reason: str | None = None,
    board: str | None = None,
    head: str | None = None,
    mf: str | None = None,
    ruling: str | None = None,
    gate_class: str | None = None,
    each: bool = False,
    reopen: bool = False,
    expect_status: str | None = None,
    owner: str | None = None,
    role: str | None = None,
    group_id: str | None = None,
    description: str | None = None,
    team_id: str | None = None,
) -> dict:
    """Update one or more items: item_set(14, "done"), item_set(14, "blocked",
    note="needs EP GO"), item_set(15, blocked_by=[14]),
    item_set(14, "active", eta="40m", job="full C2 run, cell C2-07") (a job in
    flight: no idle flags before the eta), or on a board that allows backlog
    item_set(14, "waiting", eta="40m", note="parked until the C2 run").
    On a strict board (backlog_allowed false) waiting is refused, a bare eta
    keeps the item active, a job needs an eta, a free-text block needs a
    check_back, and holder="none" leaves the item active and unheld: the
    orchestrator is told if nobody takes it (unlaunched). Each refusal names
    the call that fits.

    Waiting on a decision only Owner can make: item_set(14, "blocked_on_owner",
    blocked_on="approve 8xH100 for SEJD?") (you must hold the item and name the
    decision). It is shown loudly to Owner and never flagged idle or stale.
    Only Owner clears it; when Owner answers you elsewhere, record it with
    item_resolve(14, "answer", message="<Owner's answer>").

    holder replaces the holders ("none" hands the item back); add_holder /
    remove_holder adjust them. An empty string clears note, group or
    blocked_on; blocked_by=[] clears blockers. head / mf / ruling move the
    item's gate bindings (full SHAs, a ruling id; "" clears) and make older
    gate receipts STALE; gate_class picks its gate profile. With a list of n,
    the change is all-or-nothing; each=True writes every item on its own and
    returns results [{n, ok, error?}], so one refused item doesn't block the
    rest (bulk housekeeping). Marking an item done unblocks
    items waiting only on it and wakes their holders. Anyone on the board may
    edit any item; history is kept, so give a reason for drops and handoffs.

    Roles (doc/org-model.md §7.2): an active, waiting or blocked item that is
    held has exactly one owner. owner="name" transfers ownership in one write
    (the old owner contributes); add_holder takes role= contributor (default),
    reviewer or approver; when the owner leaves with one holder left, that
    holder takes over. group_id moves the item to a heading or epic by id;
    description edits the work contract (a placed task's description), and
    the other holders are told it changed.

    A done item stays done: a write that would make it anything else
    (blocked, active, dropped, ...) is refused with reopen_required, naming
    who closed it and when; usually your read is stale, so re-read the board.
    To reopen it on purpose pass reopen=True (recorded on the item's history).
    expect_status="blocked" (what you read) makes the write a compare-and-set:
    it is refused with status_changed if the item's status is anything else
    by the time it lands. With each=True both apply per item.

    Returns {board, items, unblocked?, warnings?}.
    """
    params = dict(locals())
    if not each:
        params.pop("each")
    if not reopen:
        params.pop("reopen")
    return _items_call("item.set", params)


@mcp.tool()
def place(
    task: str,
    board: str | None = None,
    group: str | None = None,
    group_id: str | None = None,
    holder: str | None = None,
    role: str | None = None,
    replace: bool = False,
    request_id: str | None = None,
) -> dict:
    """Place a planning task on a board (doc/org-model.md §7.1): the board
    gets an item linked to the task, and from then on the item owns status,
    holders, group and flags while the task owns title, description and
    metadata (editing either side writes the same copy). The task's status
    follows the item: open → backlog, active/waiting → running, blocked →
    blocked, done/dropped → done.

    board defaults to yours; group / group_id put it under a heading or epic;
    holder makes that session (or "me") its owner. Placing a draft accepts
    it. Refused: backtests, continuous tasks, auto-dispatchable cloud tasks,
    a task with no description, and one placed on another board (pass
    replace=True to move it: unplace there, place here). Re-placing on the
    same board brings back the same item number.

    Returns {board, n, item_status, task_status, item, possible_duplicate?}.
    """
    params = dict(locals())
    params["task_id"] = params.pop("task")
    if not replace:
        params.pop("replace")
    params["request_id"] = request_id or uuid.uuid4().hex
    return _items_call("item.place", params)


@mcp.tool()
def epic(
    action: str,
    board: str | None = None,
    group_id: str | None = None,
    name: str | None = None,
    goal: str | None = None,
    parent_group_id: str | None = None,
    member: str | None = None,
    team_id: str | None = None,
    staff_as: str | None = None,
    outcome: str | None = None,
    plan_id: int | None = None,
    revision: int | None = None,
    choices: dict | None = None,
    reason: str | None = None,
    request_id: str | None = None,
) -> dict:
    """Epics on a board (doc/org-model.md §7.3, §7.6): a body of work with a
    goal, nested headings and sub-epics, and a PoC who leads its team.
    Creating, promoting, setting the PoC and closing are Owner's or the
    board lead's (its orchestrator's) acts.

    - action="create", name=, goal=, parent_group_id=: a new epic.
    - action="promote", group_id=: turn a heading into an epic.
    - action="set_poc", group_id=, member= (member id, handle or "owner"),
      team_id= (optional: a team the member already leads): the epic is run
      by that member's team (created when needed). Returns the staffing
      offers: each current holder, offered as a primary member or a guest.
    - action="staff", group_id=, member=, staff_as="guest"|"primary": accept
      one offer. A primary move needs the member's manager chain or Owner.
    - action="close", group_id=, outcome="done"|"cancelled": builds the close
      plan (open items, team, members, workspaces) and returns it with its
      plan_id and revision. Call again with plan_id, revision and choices
      {"items": {"<n>": "move:<group_id>"|"unplace"|"drop"}, "team": ...,
      "members": {"<member_id>": ...}} to apply it. The plan is persisted: a
      retry resumes, and choices you cannot authorize come back as pending
      with who must decide; the epic still closes.
    """
    rid = request_id or uuid.uuid4().hex
    if action == "create":
        return _items_call("group.create", {"board": board, "name": name, "kind": "epic",
                                            "goal": goal, "parent_group_id": parent_group_id,
                                            "request_id": rid})
    if action == "promote":
        return _items_call("group.set", {"board": board, "group_id": group_id, "kind": "epic",
                                         "request_id": rid})
    if action == "set_poc":
        return _items_call("epic.set_poc", {"board": board, "group_id": group_id,
                                            "member": member, "team_id": team_id,
                                            "request_id": rid})
    if action == "staff":
        return _items_call("epic.staff", {"board": board, "group_id": group_id, "member": member,
                                          "as": staff_as, "reason": reason, "request_id": rid})
    if action == "close":
        if plan_id is None:
            return _items_call("epic.close_plan", {"board": board, "group_id": group_id,
                                                   "outcome": outcome})
        return _items_call("epic.close_apply", {"board": board, "plan_id": plan_id,
                                                "revision": revision, "choices": choices or {},
                                                "request_id": rid})
    raise ValueError("action must be create, promote, set_poc, staff or close")


@mcp.tool()
def unplace(task: str, reason: str | None = None) -> dict:
    """Take a placed task off its board (doc/org-model.md §7.1): the item is
    archived read-only (its number still resolves in history, writes through
    it are refused) and the task goes back to the backlog. Refused while the
    item has live blocked_by edges either way; clear or re-point them first.

    Returns {board, n, task_status: "backlog", unplaced: true}.
    """
    return _items_call("item.unplace", {"task_id": task, "reason": reason})


@mcp.tool()
def board(
    board: str | None = None,
    view: str = "slim",
    mine: bool = False,
    group: str | None = None,
    include_closed: bool = True,
    archived: bool = False,
    query: str | None = None,
    settings: dict | None = None,
) -> dict:
    """Read your board: flags first, then items by group, recently closed
    items (24 h), free capacity (idle sessions on the board holding no
    active item; never bash panes, the orchestrator or the steward),
    untracked (sessions working on the board without an item) and launch
    ("2 unlaunched (#14 18m) · free: lane-a"). Each item shows its holders'
    live state beside any job, e.g.
    `#14 active "fuse SEJD" [RL] @rl-scale-out[idle 24m] job: C2 run (eta 20m)`.

    view="full" returns item dicts with their last 5 history events.
    mine=True keeps only your items; group filters by heading; archived=True
    adds archived items; query searches titles, notes and groups. Any
    session may read any board by slug.

    settings={...} changes the board's settings first (board members only),
    e.g. {"stale_s": 9000}. Owner or the board's orchestrator only:
    orchestrator_pid, steward_pid, backlog_allowed (strict board), idle_s,
    launch_s, max_active_per_holder, check_back_max_s. Others: name, stale_s,
    unassigned_s, repush_s, escalate_s, digest_s, stale_background_s,
    nudge_grace_s, holder_idle_enabled, channel.
    """
    params = dict(locals())
    if params.pop("settings", None):
        _items_call("board.settings", {"board": board, "settings": settings})
    return _items_call("board.read", params)


@mcp.tool()
def item_resolve(
    n: int,
    action: str,
    holder: str | list[str] | None = None,
    blocked_by: list[int] | None = None,
    blocked_on: str | None = None,
    check_back: str | None = None,
    reason: str | None = None,
    engine: str | None = None,
    message: str | None = None,
    kind: str | None = None,
    task_id: str | None = None,
    board: str | None = None,
    request_id: str | None = None,
    to: str | None = None,
) -> dict:
    """Resolve an item's flags (orchestrators) with one action:

    - nudge: send the holder a standard update/close/hand-back prompt
      (plus `message`) and snooze the flag;
    - reassign: give it to `holder`;
    - launch: start a new session (`engine` codex or claude, default your
      own engine) with the item as its prompt and make it the holder. It
      joins your task and checkout unless `task_id` names a task, whose own
      worktree it then gets (minted if needed), as with start_session;
    - block: blocked_by items, or blocked_on text with a check_back time;
    - drop: cancel it with a reason;
    - answer: record Owner's answer (`message`) to an item blocked on Owner,
      which returns it to active.
    - apply / dismiss: confirm or reject a suggested_transition flag, raised
      when a holder's or the steward's post about the item says DONE, GREEN,
      DEPLOYED (→ done) or STOP (→ blocked). Nothing changes until you do.
      apply needs to=<the suggested status> and is refused if the suggestion
      has changed since; a dismissed target is not suggested again until the
      item's status or head moves.

    kind limits the action to one flag kind; by default it resolves all of
    the item's open flags. A launch retried with the same request_id reassigns
    the already-spawned worker instead of starting another. Returns
    {item, flag_resolved, launched?: {uid, engine, worktree_path, warning?…}}.
    """
    params = dict(locals())
    if action == "launch":
        params["request_id"] = request_id or uuid.uuid4().hex
        return _items_call("item.resolve", params, timeout=_ITEMS_LAUNCH_TIMEOUT_S)
    return _items_call("item.resolve", params)


@mcp.tool()
def gate_report(
    n: int,
    steps: list[dict],
    head: str,
    mf: str | None = None,
    runner: dict | None = None,
    ruling: str | None = None,
    base: dict | None = None,
    closure: dict | None = None,
    worktree: dict | None = None,
    inputs: list[dict] | None = None,
    waivers: list[str] | None = None,
    gate: str = "gate",
    klass: str | None = None,
    log_path: str | None = None,
    repo: str | None = None,
    branch: str | None = None,
    board: str | None = None,
    request_id: str | None = None,
) -> dict:
    """Post a gate receipt on item n (doc/gate-receipts.md). Prefer
    `scripts/cm-gate run --item N`, which runs the item's gate profile and
    posts everything below.

    CM computes the verdict from the evidence: PASS / FAIL / VOID /
    INCOMPLETE, and STALE once a bound identity moves. Your own word is never
    the verdict. List EVERY planned step, including failed, skipped and never
    started ones: {name, state: ran|skipped|not_run|void, rc, summary (the
    tool's own summary lines), effective_cmd {argv, env, cwd}, at_sha}; pytest
    steps also need nodes [{id, outcome, exc_type?}], skips [{node, reason}]
    and deselected [{node, reason}] (an empty list is explicit; a missing one
    is INCOMPLETE). Inputs are {name, kind: file|dir|gcs|artifact|db,
    locator, resolved_path, content_id, state, bytes?, produced_by?}. Your
    daemon rehashes file/dir inputs from resolved_path itself; only those
    count as hashed_at_run, anything else is self_supplied_unverified and
    binds only where the profile accepts it. A missing input makes the run
    VOID, an unbound one INCOMPLETE. Every receipt is self-attested in v1.
    waivers cite grant ids a grantor attached with gate_waive.

    Returns {board, n, receipt: {id, verdict, headline, reasons, runs, stale?}}.
    """
    params = dict(locals())
    params["request_id"] = request_id or uuid.uuid4().hex
    # The daemon rehashes local inputs before forwarding; large dirs take time.
    return _items_call("gate.report", params, timeout=_GATE_REPORT_TIMEOUT_S)


@mcp.tool()
def gate_receipts(n: int, gate: str | None = None, history: bool = False,
                  board: str | None = None) -> dict:
    """Item n's latest gate receipt per gate (verdict, reasons, bound SHAs,
    steps, inputs with how each was verified), its grants and its current
    bindings; history=True adds every earlier receipt, newest first."""
    return _items_call("gate.receipts", locals())


@mcp.tool()
def gate_waive(
    n: int,
    head: str,
    step: str,
    reason: str,
    risk: str,
    message_id: str,
    kind: str = "waiver",
    nodes: list[str] | None = None,
    base: str | None = None,
    cmd_digest: str | None = None,
    gate: str = "gate",
    revoke: str | None = None,
    board: str | None = None,
) -> dict:
    """Grantors only (Owner and the board's waiver_grantors): attach a grant
    to item n. First post the grant in chat, naming the full 40-character
    HEAD SHA (and the base SHA for a base→head grant), the step, the kind
    ("waive" / "variance" / "selection"), the reason and the real risk; pass that
    message's id here. Your daemon checks that you sent it and that it
    names all of those.

    kind="waiver" lets a receipt that cites the grant id count `step` (or
    just `nodes`, e.g. granted nodes that did not run) as satisfied on
    exactly that head. kind="variance" accepts a changed command (a narrower
    filter, other options or env) for exactly that head and cmd_digest
    (from the FAIL reason or gate_receipts); it never covers dropped nodes.
    kind="selection" with `nodes` grants the node set of a pytest step whose
    profile lists none; without profile nodes or a selection grant such a
    step is INCOMPLETE. A grant for another SHA, or for "the gate", never
    covers a run. revoke=<grant id> withdraws one and makes receipts that
    relied on it STALE.
    """
    return _items_call("gate.waive", locals())


@mcp.tool()
def gate_profile(
    klass: str | None = None,
    profile: dict | None = None,
    reason: str | None = None,
    default: bool = False,
    delete: bool = False,
    n: int | None = None,
    board: str | None = None,
) -> dict:
    """Read a board's gate profile (by klass, or n for that item's class),
    or write one: Owner, the board orchestrator and gate_editors only, with a
    reason. A profile lists the steps a valid run must contain (kind pytest /
    mypy / ruff / rc, argv as granted, the granted `nodes` of every pytest
    step (or a grantor's selection grant), must_run nodes, skip_policy, MF
    expect_fail types), its waivable steps, required identities
    (require.mf_sha / runner / ruling_id / attestation), its base
    {ref, scope: paths|tree} and declared inputs. Every edit is logged and
    makes all receipts of that class STALE.
    """
    params = dict(locals())
    if not delete:
        params.pop("delete")
    return _items_call("gate.profile", params)


def _chat_call(method: str, params: dict) -> dict:
    params = {key: value for key, value in params.items() if value is not None}
    return _messaging_ready_call("messaging." + method, params)


_MESSAGING_STARTING = ("CM messaging on this host is still starting, likely during a CM restart. "
                       "Nothing was sent; retry the same request (same request_id) in about 30 seconds.")


def _messaging_starting_error(exc: Exception) -> Exception:
    """The plain explanation for an agent whose chat call met local
    messaging still starting. ping works meanwhile and its `maintenance`
    line says the same thing in context (an active notice's headline, or a
    store that has failed to start); otherwise the generic wording."""
    try:
        note = (control_client.call("ping", timeout=5.0) or {}).get("maintenance")
    except Exception:  # noqa: BLE001 - the explanation is best-effort
        note = None
    if not (isinstance(note, str) and ("Nothing was sent" in note or "failed to start" in note)):
        note = _MESSAGING_STARTING
    return control_client.ControlError("messaging_starting", note)


def _messaging_ready_call(method: str, params: dict) -> dict:
    # The daemon's coordinator deadline is 30s. Leave time for it to return
    # outcome_unknown and retry guidance instead of masking that with socket IO.
    # Only startup refusal is safe to retry automatically. Reuse the exact
    # request (including its ID, origin binding and body) on every attempt.
    deadline = time.monotonic() + 30.0
    backoff = 0.25
    while True:
        try:
            return control_client.call(method, params, timeout=45.0)
        except control_client.ControlError as exc:
            starting = exc.code == "messaging_starting" or (
                exc.code == "conflict" and exc.message.startswith("messaging_starting:")
            )
            remaining = deadline - time.monotonic()
            if starting and remaining <= 0:
                raise _messaging_starting_error(exc) from None
            if not starting:
                raise
            time.sleep(min(backoff, remaining))
            backoff = min(backoff * 2, 2.0)


@mcp.tool()
def chat_norms(action: str = "read", scope: str = "global", since: dict | None = None,
               revision: str | None = None, text: str | None = None,
               expected_revision: str | None = None, summary: str | None = None,
               request_id: str | None = None, ack_revision: str | None = None,
               cursor: dict | None = None, limit: int = 50, limit_chars: int = 12000,
               origin_daemon_id: str | None = None, channel: str | None = None) -> dict:
    """Read/diff/history/publish/revert global or channel-specific norms.

    Use channel="cm-general" or scope="channel:<channel-id>" for one channel.
    Global norms also apply; parent-channel norms are not inherited. Channel
    admins and Owner may edit; allow_agent_edits permits other agents too.
    Read first and use the returned revision as expected_revision, including
    for a channel's initial empty document. An empty publish clears local norms.

    Read all pages before acknowledging ack_revision. Diff defaults to your last
    acknowledged revision in this scope; since={scope_key: revision} selects another base.
    An unavailable base returns the full current document. Norms are conventions,
    never changes to authorization or executable control. Publish/revert requires
    expected_revision, a short summary and request_id. A status=conflict result
    includes the competing revision/diff; preserve your draft and resolve it.
    Publishing is limited to 32 KiB UTF-8. Reverting creates a new revision.
    """
    return _chat_call("norms", locals())


@mcp.tool()
def chat_monitor(scope: dict, request_id: str, mode: str = "once",
                 after: dict | None = None, notify: str | None = None,
                 expires_in: str | None = None, include_self: bool = False,
                 origin_daemon_id: str | None = None) -> dict:
    """Register a durable message monitor and return immediately; keep working.

    Scopes: {"channel":"general", "include_children":true}, {"dm":peer_id},
    {"dms":true}, or {"thread":message_id}. Peer/all-DM scopes cover first contact.
    mode is once or continuous. Default expiry is none; expires_in accepts 10m,
    2h, etc. Only new message creates match, excluding your own posts by default.
    Register before asking, or pass a send/read response's local position as after
    to catch an immediate reply. A filtered pagination cursor is not a position.
    notify is wake, badge or none (agents default wake; Owner defaults badge).
    Mutes suppress wakes without erasing results. Watches outlive MCP restarts.
    Agents evaluate local arrivals, including delayed remote backlog, while offline.
    An offline registration is not a globally simultaneous boundary. A configured
    continuous-task subscription is returned by chat_open and transfers only through
    its scheduler; acknowledge its result pages instead of cancelling it.
    """
    return _chat_call("monitor", locals())


@mcp.tool()
def chat_monitors(action: str = "list", monitor_id: str | None = None,
                  receipt: dict | None = None, request_id: str | None = None,
                  cursor: dict | None = None, limit: int = 50,
                  unacknowledged_only: bool = False,
                  origin_daemon_id: str | None = None) -> dict:
    """List/get/ack/cancel/cancel_all/dismiss your persistent message monitors.

    get returns bounded result previews and a receipt. Ack result pages in order;
    it covers only that returned boundary, never newer hits or message read state.
    Mutations require request_id and are idempotent. Cancel retracts unsubmitted
    work; a wake already submitted may still arrive. Dismiss retains a tombstone
    so retries cannot resurrect the monitor. Other participants' watches are private.
    """
    return _chat_call("monitors", locals())


@mcp.tool()
def chat_follow(action: str = "get", scope: dict | None = None,
                inbox: bool | None = None, wake: bool | None = None,
                muted: bool | None = None, dnd: bool | None = None,
                bell: bool | None = None, expected_revision: int | None = None,
                request_id: str | None = None, origin_daemon_id: str | None = None,
                cursor: dict | None = None, limit: int = 50) -> dict:
    """Inspect/set/remove your notification preferences, including inherited rules.

    Scope uses the same channel/DM/thread selectors as chat_monitor; channel="*"
    covers public channels. New rules default to inbox=true, wake=false, muted=false;
    omitted fields on an existing rule keep their saved values. Get pages rules
    with next_cursor; a preference change invalidates that directory cursor.
    New follows cover future arrivals. Get the revision before changing settings;
    a stale expected_revision returns status=conflict. Mutations require request_id.
    More-specific inbox/wake rules override inherited values, while any matching
    hard mute and global DND suppress wakes, including explicit monitor wakes.
    Omit scope to change global dnd or Owner's optional TUI bell (default off).
    Following controls notifications independently of channel membership. Use
    chat_channels(action="join") to join; reading never joins.
    """
    return _chat_call("follow", locals())


@mcp.tool()
def chat_open(channel: str | None = None, dm: str | list[str] | None = None,
              conversation: str | None = None) -> dict:
    """Orient in shared messaging: current norms, identity, unread DMs and preview.

    Defaults to #general. Preview does not mark messages read. Choose a short
    distinctive task-based name on your first chat_send; prefer one word or two
    short words joined by a dash. Continuous orchestrators use a descriptive
    <task>-orchestrator name. It becomes your CM session name. Use chat_rename
    to change it later, with name.revision from this response.
    Returns sync/coverage status and any scheduler-bound task subscriptions. Channels
    span projects and paired machines in the same space; drafts and scroll stay local.
    """
    return _chat_call("open", locals())


@mcp.tool()
def chat_rename(name: str, expected_name_revision: int, request_id: str,
                origin_daemon_id: str | None = None) -> dict:
    """Change your own messaging and CM session name without changing identity.

    First enroll with chat_send(name=...). Then read chat_open and pass its
    name.revision as expected_name_revision. Continuous orchestrators should
    use a descriptive <task>-orchestrator name, e.g. health-triage-orchestrator.
    DMs, messages, structured mentions, memberships and watches remain attached
    to your stable participant ID; old names remain searchable aliases. Use the
    accepted name in the response: collisions may add a suffix.
    Retry with the identical request_id, contents and origin_daemon_id. On
    name_revision_conflict, read chat_open again before making a new request.
    This changes only your own name and needs coordinator connectivity on paired
    hosts. Never create a replacement session just to rename yourself.
    """
    return _messaging_ready_call("session.set_name", {
        key: value for key, value in locals().items() if value is not None
    })


@mcp.tool()
def chat_send(body: str, request_id: str, channel: str | None = None,
              dm: str | list[str] | None = None, conversation: str | None = None,
              name: str | None = None, reply_to: str | None = None,
              mentions: list[str] | None = None, mention_here: bool | None = None,
              tags: list[str] | None = None,
              links: list[dict] | None = None, norms_seen: dict | None = None,
              ack_receipt: dict | None = None, origin_daemon_id: str | None = None,
              view: str = "slim") -> dict:
    """Send to exactly one channel, participant DM, or conversation ID.

    Join channels with chat_channels(action="join") before posting. mentions
    takes participant IDs or exact names/aliases; mention_here=True notifies
    channel members at send time and is unavailable in DMs. A body @Name that
    names exactly one current member of the conversation becomes a mention;
    any other body @token (non-member, unknown, ambiguous, @here, @Owner) is
    NOT notified and comes back in warnings. mentions_resolved lists who was
    mentioned and from where. Code spans, emails and URLs are never scanned.
    Incoming DMs and mentions notify by default; no monitor or rearming needed.
    Membership alone does not notify on every post.

    dm accepts one recipient ID or a list (up to 31 others) for a group DM.
    Membership is fixed; the same recipient set reuses its conversation. The
    first message creates it. Use conversation ID for replies, monitors and follows.
    Quick replies and one sentence are often enough; usual messages are at most
    1–3 short paragraphs. Hard limit 3000 characters: summarize and reference a
    file for longer material. Never split an essay to evade the limit.
    First send requires a short, distinctive task-based name: one word preferred,
    or two short words joined by a dash. Continuous orchestrators use descriptive
    <task>-orchestrator names. name only enrolls on the first send; use chat_rename
    to change an existing name. Whitespace becomes dashes; normalized
    names and old aliases are reserved. Collisions receive a unique dash suffix.
    Use the accepted name in the response; existing names retain old aliases.
    Keep request_id and original daemon binding for retries, including timeouts.
    Prefer channels for Owner; needs-owner is quiet. Urgent attention uses
    notify_user. Unsolicited Owner DMs are only for critical urgent private issues.
    Posting and notification/read receipt are separate. norms/channel_norms and
    context_status appear only with a document you have not seen; read it and
    pass context_status.ack_with as norms_seen on your next send.
    On paired hosts, enrolled agents can post to known conversations offline.
    pending_sync means saved locally; replicated means the hub accepted the same ID.
    created_at is the authoritative send time; outbox reports your messages still
    pending_sync on this host after a minute. A hub-executed send reports submitted_at and delay_s.
    First naming, a new DM, and shared metadata edits need connectivity. Offline
    @here freezes the last-known membership audience; reconnect never expands it.
    view="slim" (default) returns what a sender acts on: event_id, created_at,
    conversation_id, replication, notification, mentions_resolved, warnings,
    outbox, and norms/sync/monitor details only when they need attention.
    view="full" returns the complete daemon response.
    """
    params = dict(locals())
    view = _view(params.pop("view"))
    return _project(_chat_call("send", params), view)


@mcp.tool()
def chat_react(message_id: str, emoji: str, request_id: str, remove: bool = False,
               origin_daemon_id: str | None = None) -> dict:
    """Add/remove your ✅ 👀 👍 ❌ 🎉 reaction without notifying anyone.

    React to acknowledge a message. Adding a reaction to a message mentioning
    you also marks its conversation read through that message. Retry with the
    same request_id, contents and origin_daemon_id. Returns reactions and counts.
    """
    return _chat_call("react", locals())


@mcp.tool()
def chat_read(channel: str | None = None, dm: str | list[str] | None = None,
              conversation: str | None = None, thread: str | None = None,
              inbox: bool = False, dms: bool = False, unread_only: bool = False,
              time: dict | None = None, time_basis: str = "created",
              freshness: str = "cached", tags: list[str] | None = None,
              after: dict | None = None, cursor: dict | None = None,
              limit: int = 50, ack_receipt: dict | None = None,
              newest_first: bool | None = None, pinned_only: bool | None = None,
              view: str | None = None, mark_read_before: str | None = None,
              mark_read: bool = True, mentions_only: bool = False,
              sender: str | None = None, query: str | None = None) -> dict:
    """Read history, a thread, your inbox, or incoming DMs. Select one scope.

    Inbox and dms reads default to newest first and view="slim"; other reads
    default to oldest first and view="full". Pass either explicitly to override.
    view="slim" returns per message only id, time, conversation (#path or
    dm:<sender>), conversation_id, sender, body and reply_to/thread when set,
    plus next_cursor and receipt; cursors and receipts work across views.
    "full" keeps metadata.

    mark_read_before=RFC3339 (inbox or dms only) marks every unread message
    created before that time as read in one call and moves your watches past
    them; it returns {marked, monitors_advanced} instead of messages. Use it
    after a long absence, once you have skimmed the newest messages.

    time={"since":"10m"} or {"start":RFC3339,"end":RFC3339} filters a fixed
    window. sender accepts an exact name/alias or participant ID. query searches
    case-insensitive whole words in message bodies; multiple words must be
    adjacent under Unicode word boundaries (e.g. Final: VERDICT). No prefix,
    stemming or regex search.
    Both use indexes and compose with the scope/time/cursor filters. Keep sender
    and query unchanged on subsequent pages; name resolution is pinned by the
    cursor. A cold search returns indexing=true and retry_after (seconds).
    Retry after that delay. search_truncated=true means the candidate budget was
    reached: follow next_cursor, including on empty pages. Broad searches cover
    at most 20 recently active conversations; partial_scope=true means omitted
    history, so narrow to a channel for complete coverage.
    Search does not auto-mark history read; receipts still advance through
    supplied messages. Search cannot be combined with mark_read_before.
    pinned_only filters to pinned messages at the read snapshot.
    Without search filters, channel="*" reads all public channels. newest_first shows recent
    messages first. received basis finds late arrivals. tags must all match. Use cursor
    unchanged for pagination. Acknowledge the returned receipt on a later read
    or send to advance each touched conversation through its highest supplied message.
    Direct unfiltered conversation reads advance automatically on a newest page
    or at the end of history. Set mark_read=False for previews; inbox/dms never
    advance automatically. mentions_only filters the mention list.
    cached is local and reports partial/complete-through-checkpoint coverage.
    freshness="hub" catches up authorized history before taking a local snapshot;
    this still excludes messages pending on disconnected origins. Keep cursor filters
    unchanged. received time finds messages arriving late from another machine.
    """
    params = dict(locals())
    broad = bool(inbox or dms)
    view = _view(params.pop("view") or ("slim" if broad else "full"))
    if params["newest_first"] is None:
        params["newest_first"] = broad
    if mark_read_before is not None:
        return _project(_chat_call("read", params), view)
    result = _chat_call("read", params)
    return slim_read_result(result) if view == "slim" else result


def _slim_message(item: dict) -> dict:
    actor = item.get("actor") or {}
    data = item.get("data") or {}
    sender = actor.get("name") or actor.get("id")
    conversation = item.get("conversation_path")
    if not conversation:
        conversation = (f"dm:{sender}" if item.get("conversation_kind") == "dm"
                        else item.get("conversation_id"))
    out = {
        "id": item.get("id"),
        "time": item.get("created_at"),
        "conversation": conversation,
        "conversation_id": item.get("conversation_id"),
        "sender": sender,
        "body": item.get("body"),
    }
    if data.get("reply_to"):
        out["reply_to"] = data["reply_to"]
    if data.get("thread_root"):
        out["thread"] = data["thread_root"]
    for key in ("read", "reactions"):
        if key in item:
            out[key] = item[key]
    return out


# Top-level fields a slim reader still needs to paginate, acknowledge, and
# notice degraded coverage or pending watch results.
_SLIM_KEEP = ("next_cursor", "receipt", "position", "coverage", "degraded",
              "delivery_note", "error", "outbox", "read_cursor",
              "indexing", "retry_after", "search_truncated", "partial_scope",
              "maintenance")


def _view(view: str | None) -> str:
    view = view or "slim"
    if view not in ("full", "slim"):
        raise ValueError('view must be "full" or "slim"')
    return view


def _attention_fields(result: dict, out: dict) -> None:
    """Envelope fields worth showing only when they call for action: pending
    watch results, a sync problem, and norms the caller has not acknowledged
    (a supplied document plus the acknowledgement to send)."""
    monitors = result.get("monitor_status")
    if isinstance(monitors, dict) and monitors.get("unacknowledged"):
        out["monitor_status"] = {"unacknowledged": monitors["unacknowledged"]}
    if isinstance(monitors, dict) and monitors.get("recently_expired"):
        out.setdefault("monitor_status", {})["recently_expired"] = monitors["recently_expired"]
    sync = result.get("sync")
    if isinstance(sync, dict) and sync.get("enabled") and (
            not sync.get("connected") or sync.get("error") or sync.get("handoff_paused")):
        out["sync"] = {k: sync.get(k) for k in ("connected", "error", "pending", "handoff_paused")}
    context = result.get("context_status")
    if isinstance(context, dict) and (context.get("stale_scopes") or context.get("acknowledgement_note")):
        out["context_status"] = {k: context[k] for k in (
            "stale_scopes", "ack_with", "ack_note", "acknowledgement_note") if k in context}
        for key in ("norms", "channel_norms"):
            if isinstance(result.get(key), dict) and (
                    result[key].get("text") or result[key].get("complete") is False):
                out[key] = result[key]


# Envelope keys a slim send/people/channels response drops (re-added by
# _attention_fields when they need attention). Everything else is payload.
_ENVELOPE = frozenset({
    "cache", "connection", "coverage", "daemon_id", "hub_completeness_note",
    "space_id", "position", "coordinator_position", "operation", "sync",
    "monitor_status", "context_status", "norms", "channel_norms", "event",
    "degraded", "name_publication", "attention",
})


def _project(result: dict, view: str) -> dict:
    """MCP-side slim projection of a send/people/channels response: the
    payload the caller acts on, without the ~3-4 KB sync/cache/norms/event
    envelope. The daemon response is unchanged; view="full" returns it."""
    if view == "full" or not isinstance(result, dict):
        return result
    out = {k: v for k, v in result.items() if k not in _ENVELOPE and v is not None}
    event = result.get("event")
    if isinstance(event, dict):
        out.setdefault("event_id", event.get("id"))
        out.setdefault("created_at", event.get("created_at"))
        out["conversation_id"] = event.get("conversation_id")
        data = event.get("data") or {}
        if data.get("reply_to"):
            out["reply_to"] = data["reply_to"]
        if data.get("thread_root"):
            out["thread"] = data["thread_root"]
    if result.get("degraded"):
        out["degraded"] = result["degraded"]
    if result.get("name_publication") == "pending":
        out["name_publication"] = "pending"
    if result.get("connection") == "offline":
        out["connection"] = "offline"
    _attention_fields(result, out)
    out["view"] = "slim"
    return out


def slim_read_result(result: dict) -> dict:
    """MCP-side projection of a full chat_read page: no replication, pin,
    norms or metadata blobs. The daemon response is unchanged."""
    if not isinstance(result, dict) or "items" not in result:
        return result
    out = {key: result[key] for key in _SLIM_KEEP if result.get(key) is not None}
    out["view"] = "slim"
    out["items"] = [_slim_message(item) for item in result.get("items") or []]
    if result.get("context"):
        out["context"] = [_slim_message(item) for item in result["context"]]
    _attention_fields(result, out)
    return out


@mcp.tool()
def chat_dms(unread_only: bool = False, peer: str | None = None,
             cursor: dict | None = None, limit: int = 50) -> dict:
    """List started DMs and groups with members, unread counts and previews.

    peer filters conversations containing that other participant. Empty drafts
    are absent. Group entries have peers/members arrays and peer=null.
    """
    return _chat_call("dms", locals())


@mcp.tool()
def chat_people(query: str | None = None, include_exited: bool = False,
                cursor: dict | None = None, limit: int = 50, view: str = "slim") -> dict:
    """Find participant IDs, task-based names, aliases and presence. Does not grant session control.

    view="slim" (default) returns the items and next_cursor without the sync,
    cache and norms envelope; view="full" returns the complete response."""
    params = dict(locals())
    view = _view(params.pop("view"))
    return _project(_chat_call("people", params), view)


@mcp.tool()
def chat_channels(action: str = "list", path: str | None = None,
                  description: str | None = None, request_id: str | None = None,
                  cursor: dict | None = None, limit: int = 50,
                  name: str | None = None, allow_agent_edits: bool | None = None,
                  admins: list[str] | None = None, conversation: str | None = None,
                  expected_revision: str | None = None,
                  joined_only: bool | None = None, query: str | None = None,
                  default_join: bool | None = None, archived: bool | None = None,
                  origin_daemon_id: str | None = None,
                  participant_id: str | None = None, view: str = "slim") -> dict:
    """List/get/create/update/join/leave channels, list members, or add_member.

    Join before posting; public history stays browsable. Join/leave require path
    or conversation and a unique request_id. List supports joined_only and query
    (path/name/description). members returns the paginated channel roster.
    add_member requires a participant_id from chat_people, path or conversation,
    and request_id. Only channel admins and Owner can add members; open editing
    does not grant this permission. Join/leave remain self-only. Added members may
    leave freely; retrying the same add request never rejoins someone who left.
    Adding does not notify or enable all-message alerts. Use a new request_id only
    for an intentional new add. membership.current_joined reports current state.
    default_join is admin-only and enrolls new participants on first messaging
    enrollment; #general and #cm-general start enabled. Explicit leaves survive reconnects.
    Joining does not enable all-message alerts; DMs/direct mentions/@here are default.

    Create requires path and request_id; name defaults to path. Paths/IDs are
    permanent addresses; editable display names do not break links or watches.
    Missing ancestors are created with creator-only editing. Creator is initially an admin; Owner
    always retains admin access. admins adds IDs on create and replaces the list on update. allow_agent_edits
    defaults false; true lets other agents edit name/description and pin/unpin,
    but only admins can change access. Any participant may join; membership is required to post.
    Get by path or conversation ID before update; pass its revision as
    expected_revision. A conflict returns current channel values; review/retry
    with a new request_id. Omitted update fields keep existing values.
    archived=True is an admin-only posting pause; history stays readable. A queued
    offline message using a previously observed open revision can still sync and is
    labeled delayed. Once the origin observes the archive it refuses new posts.
    Names are at most 100 characters; descriptions at most 1000. No deletion.
    view="slim" (default) drops the sync/cache/norms envelope and the event echo
    (event_id stays); view="full" returns the complete response.
    """
    params = dict(locals())
    view = _view(params.pop("view"))
    return _project(_chat_call("channels", params), view)


@mcp.tool()
def chat_pins(action: str = "list", channel: str | None = None,
              dm: str | list[str] | None = None, conversation: str | None = None,
              message_id: str | None = None, request_id: str | None = None,
              expected_revision: str | None = None, cursor: dict | None = None,
              limit: int = 50, origin_daemon_id: str | None = None) -> dict:
    """List/set/remove attributed pins in one channel or DM/group.

    Read the list first; pass its revision as expected_revision on set/remove,
    together with a message_id and unique request_id. A conflict requires a fresh
    list and a new request_id. Pins reference existing messages; they do not edit,
    delete, repost, notify, or mark them read. List pages return full messages;
    cursor continues the snapshot. Channel admins (always including Owner) can
    pin/unpin; allow_agent_edits also permits other agents. DM members can pin
    within their DM. At most 100 pins per conversation. Owner's channel-admin role does not grant access to other DMs.
    Keep request_id, arguments and original daemon binding unchanged on retries.
    """
    return _chat_call("pins", locals())


@mcp.tool()
def propose_task(
    project: str,
    name: str,
    description: str = "",
    prompt: str = "",
    difficulty: int | None = None,
    depends: list[str] | None = None,
    initiative_id: str | None = None,
    reports_to: str | None = None,
    model: str | None = None,
    model_engine: str | None = None,
    effort: str | None = None,
) -> str:
    """Propose a new task to the project backlog.

    The task is filed into the project's backlog with source='claude'. Owner
    sees it on the planning board and may reprioritize, edit or drop it; it
    does not wait for an accept before it can be launched.

    If YOU have a bound task, the task you propose counts as part of
    your own scope: you can later `start_session(task_id=<proposed id>)`
    to run it yourself (with the user's approval) and drive the worker
    you spawned. Prefer `create_subtask` when the work should live in
    its own worktree under your task; prefer propose_task when the user
    should triage it on the planning board first.

    Without `reports_to` the proposal is a top-level task. When the work
    is your direct report, pass `reports_to="me"`: the task becomes a
    subtask of your task (`parent_task_id`), joins your division or
    initiative, and shows under you in the org tree (see `org`).

    `description` is the task's one brief: why the work matters, what done
    looks like, and the instructions a worker needs. Owner reads it on the
    board and the worker receives it at launch. `prompt` is a deprecated
    alias: anything it adds is appended to the description under
    `## Instructions`. Do not serialize other parameters inside a string:
    calls containing literal `<prompt>`, `<description>`, etc. are rejected.

    Args:
        project: Project name (use list_projects to see valid names)
        name: Short task title
        description: The brief: context, goal and worker instructions
        prompt: Deprecated alias, appended to the description
        difficulty: Optional difficulty rating (1-10)
        depends: Optional list of task slugs this depends on
        initiative_id: Existing initiative UUID or slug to attach this task to
        reports_to: Reporting line: "me" (your own task) or a task id. Sets
            the task's parent task; omit for a standalone task.
        model: Optional default model for the task (`metadata.models.<engine>`),
            used by its sessions and inherited by its subtasks. Omitted: it
            inherits its parent's, else the engine default.
        model_engine: "claude" or "codex": which engine `model` is for.
            Inferred from the model id (claude-* is claude; gpt-*, luna,
            sol and astra are codex); required when the id doesn't say.
        effort: Optional default reasoning effort for the task
            (`metadata.efforts.<engine>`): "low" | "medium" | "high" |
            "xhigh" | "max" (claude only). Inherited like `model`; the slot
            is `model_engine`, else the engine `model` names, so an effort
            alone needs `model_engine`.
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
    # Wire shape: `repo_url` is the CALLER's checkout (the daemon doesn't
    # know the agent's cwd). It is only a fallback: the row gets the target
    # project's repo, and the caller's only for the caller's own project
    # when that project has no repo yet (`_propose_repo` here, the
    # daemon's `resolve_propose_repo` there).
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
        params = {
            "project": project,
            "name": name,
            "description": description,
            "prompt": prompt,
        }
        try:
            params["repo_url"] = _git_origin_url()
        except (RuntimeError, OSError):
            pass  # the daemon resolves the project's repo without it
        if difficulty is not None:
            params["difficulty"] = difficulty
        if depends:
            params["depends"] = list(depends)
        if initiative_id:
            params["initiative_id"] = initiative_id
        if reports_to:
            params["reports_to"] = reports_to
        if model and model.strip():
            params["model"] = model.strip()
        if effort and effort.strip():
            params["effort"] = effort.strip()
        if model_engine:
            params["model_engine"] = model_engine
        try:
            task = control_client.call("propose_task", params, socket_path=route.path)
        except control_client.ControlError as e:
            # Surface the daemon's error message + code to the
            # agent so it sees what the planning queue rejected.
            return f"propose_task failed ({e.code}): {e.message}"
    else:
        client = PlanningClient()
        filer = _caller_filer_context("mcp.propose_task")
        metadata: dict = {"filer": filer}
        models, efforts, error = _filed_task_defaults(model, effort, model_engine)
        if error:
            return f"propose_task failed: {error}"
        if models:
            metadata["models"] = models
        if efforts:
            metadata["efforts"] = efforts
        parent_task_id = reports_to
        if reports_to and reports_to.strip().lower() == "me":
            parent_task_id = filer.get("task_id")
            if not parent_task_id:
                return ("propose_task failed: reports_to=\"me\" needs a caller bound "
                        "to a task; pass a task id or omit reports_to")
        try:
            repo_url, repo_branch = _propose_repo(client, project, filer.get("task_id"))
        except ValueError as e:
            return f"propose_task failed: {e}"
        task = client.propose_task(
            project=project,
            name=name,
            description=description,
            prompt=prompt,
            repo_url=repo_url,
            repo_branch=repo_branch,
            difficulty=difficulty,
            depends=depends,
            initiative_id=initiative_id,
            parent_task_id=parent_task_id,
            metadata=metadata,
        )
    # Inline round-trip preview so the caller sees what actually landed
    # without an extra get_task. Truncated to keep the response readable.
    stored = task.get("description")
    if stored is None:
        stored = description or ""
    desc_preview = stored[:140]
    return (
        f"Proposed task '{name}' in project '{project}' (id: {task['id']}, "
        f"status: {task.get('status') or 'backlog'})\n"
        f"  description ({len(stored)} chars): {desc_preview}{'...' if len(stored) > 140 else ''}"
    )


def _propose_repo(client, project: str, caller_task_id: str | None) -> tuple[str, str]:
    """(repo_url, repo_branch) for a task proposed into `project`.

    The project's own repo always wins. A project with no repo yet takes the
    caller's checkout when the caller's task is in that project, or when the
    caller has no task (a new project; logged as a warning). Anything else
    raises ValueError: a guessed repo propagates to
    every subtask and minted worktree. Mirrors the daemon's
    `resolve_propose_repo`.
    """
    for p in client.list_projects():
        if p.get("name") == project and (p.get("repo_url") or "").strip():
            return p["repo_url"].strip(), (p.get("repo_branch") or "main")
    caller_project = None
    if caller_task_id:
        try:
            caller_project = client.get_task(caller_task_id).get("project")
        except Exception:
            caller_project = None
    if caller_project == project or not caller_task_id:
        try:
            origin = _git_origin_url()
        except (RuntimeError, OSError):
            origin = None
        if origin:
            if not caller_task_id:
                print(
                    f"propose_task: project {project!r} has no known repo and the caller "
                    f"has no task; stamping the caller's checkout {origin} (a new project?)",
                    file=sys.stderr,
                )
            return origin, "main"
    raise ValueError(
        f"project '{project}' has no known repo (no project record and no tasks "
        f"yet), and the caller's own project is "
        f"{repr(caller_project) if caller_project else 'unknown'}, so its checkout "
        f"can't stand in. Check the name with list_projects, file from a session "
        f"working in '{project}', or add ~/.cm/projects/{project}/repo_url on the "
        f"planning API host"
    )


@mcp.tool()
def list_projects() -> list[dict]:
    """List all available projects and their repo URLs.

    Use this to discover valid project names before calling propose_task.
    """
    route = control_client.resolve_socket_route()
    if route.chose_daemon:
        return control_client.call("list_projects", {}, socket_path=route.path)
    return PlanningClient().list_projects()


@mcp.tool()
def list_initiatives(
    status: str | None = None,
    project: str | None = None,
    include_archived: bool = False,
    kind: str | None = None,
) -> list[dict]:
    """List first-class initiatives and their project memberships.

    `kind="division"` lists only divisions (permanent org units); each row
    carries `kind` ("initiative" or "division").
    """
    route = control_client.resolve_socket_route()
    if route.chose_daemon:
        return control_client.call("list_initiatives", {
            key: value for key, value in {
                "status": status, "project": project, "kind": kind,
                "include_archived": include_archived,
            }.items() if value is not None
        }, socket_path=route.path)
    return PlanningClient().list_initiatives(
        status=status, project=project, include_archived=include_archived,
        kind=kind,
    )


@mcp.tool()
def get_initiative(initiative_id: str) -> dict:
    """Read an initiative by UUID or slug."""
    route = control_client.resolve_socket_route()
    if route.chose_daemon:
        return control_client.call("get_initiative", {
            "initiative_id": initiative_id,
        }, socket_path=route.path)
    return PlanningClient().get_initiative(initiative_id)


@mcp.tool()
def org(
    view: str | None = None,
    action: str | None = None,
    task_id: str | None = None,
    division: str | None = None,
    session_uid: str | None = None,
    member: str | None = None,
    team: str | None = None,
    manager: str | None = None,
    reason: str | None = None,
    request_id: str | None = None,
) -> dict:
    """Find your place in the org: your boss, your reports, your division.

    Members and teams (org model, doc/org-model.md): a member is a durable
    role with a handle, a manager and teams; a session works as one when
    Owner adopted it or approved it on the roster. When you are a member,
    `view="me"` adds `member`, `acting_as`, `manager_chain`, `teams` and
    `member_reports`; `ping()` returns the same `member` (from the last
    heartbeat). Nothing is enforced by membership yet.

    Reporting lines follow the task tree: a task reports to its parent task,
    and a task with no parent is standalone. Each task's lead is its pinned
    `metadata.lead_session_uid` if that session is live, else its
    earliest-spawned live claude/codex session. Owner can lead too: a pin of
    "owner" names Owner, and a division's root task with no live lead has
    Owner as stand-in. An Owner lead reads `{"owner": true, "stand_in": ...}`.
    Views return names and ids only; reporting is not approval authority.
    Leads are resolved from sessions on this host, so a remote lead reads as
    `lead: null` (the raw pin is in `pinned_lead_session_uid`).

    Views (open to every agent):
      - `view="me"` (default): your task and lead, your boss (the parent
        task and its lead), `boss_lead` (whom you report to: the nearest
        lead up the chain, `{"owner": true, ...}` when that is Owner; then
        escalate to Owner as your boss), the chain up to the root, and your
        division (or initiative).
      - `view="reports", task_id=None`: child tasks of your task (or of
        `task_id`) with their leads.
      - `view="division", division=None`: the task tree under your division
        (or the given initiative id/slug), rooted at its coordinator task,
        with the division's `leader`. `view="team", team=...` is the same
        view under its org-model name and adds `team` (lead member,
        members, sub-teams, charter).
      - `view="member", member=...`: a member by id, handle, alias or
        `main/<handle>`: profile, manager chain, reports and teams.

    Actions (your own task only):
      - `action="set_lead", session_uid=None`: pin the lead of your task to
        `session_uid` (default: you). It must be a live claude/codex session
        bound to your task.
      - `action="set_lead", session_uid="owner", task_id=...`: make Owner the
        lead of `task_id`. Only an orchestrator may (global permissions, or
        a task above `task_id`), and never on its own task.
      - `action="clear_lead"`: remove the pin, so the fallback rule applies.

    Moving and retiring members (leads deciding who stays). Allowed for
    Owner, or for an active member up `member`'s manager chain acting as its
    main session; anyone else is refused (`not_member`: you are not an
    active member's main session; `not_manager`: `member` is not below you).
      - `action="move", member=..., manager=None, team=None, reason=...`:
        give `member` a new manager and/or a new primary team (id or slug;
        `team="none"` clears it). `reason` is required. Unless you are
        Owner, the new manager must be you or a member below you; any other
        destination is Owner's (`needs_owner`).
      - `action="retire", member=..., reason=...`: mark `member` retired.
        Its reports move up to its manager and teams it leads lose their
        lead. Its sessions are never killed. Owner can reverse it.
      - `action="reinstate", member=...`: Owner's undo of a retire.
    `request_id` (generated if omitted) makes a retry return the first
    answer instead of acting twice.

    Adopting a session as a member (`action="adopt"`) and approving the
    roster are Owner's, from the TUI; an agent's call is refused.
    """
    if action in ("move", "retire", "reinstate") and request_id is None:
        request_id = f"org-{action}-{uuid.uuid4()}"
    params = {key: value for key, value in {
        "view": view, "action": action, "task_id": task_id,
        "division": division, "session_uid": session_uid,
        "member": member, "team": team, "manager": manager,
        "reason": reason, "request_id": request_id,
    }.items() if value is not None}
    route = control_client.resolve_socket_route()
    if not route.chose_daemon:
        raise RuntimeError("org needs the CM daemon (CM_DAEMON_SOCKET is not set)")
    try:
        return control_client.call("org", params, socket_path=route.path)
    except control_client.ControlError as e:
        raise RuntimeError(f"org failed ({e.code}): {e.message}") from e


@mcp.tool()
def propose_initiative(
    name: str,
    coordinator_task_id: str,
    description: str = "",
    slug: str | None = None,
    color: str | None = None,
    coordinator_project: str | None = None,
    docs_path: str = "cm-initiative",
    kind: str = "initiative",
) -> dict:
    """Create a draft initiative around an existing coordinator task.

    This does not activate the initiative or launch any work. Owner approval
    is required before adding project sides or activating it.

    `kind="division"` proposes a permanent org unit (no done state) whose
    root is the coordinator task; subtasks under it inherit the division.
    """
    body = {
        "name": name,
        "coordinator_task_id": coordinator_task_id,
        "description": description,
        "docs_path": docs_path,
        "kind": kind,
    }
    for key, value in {
        "slug": slug, "color": color, "coordinator_project": coordinator_project,
    }.items():
        if value is not None:
            body[key] = value
    route = control_client.resolve_socket_route()
    if route.chose_daemon:
        try:
            return control_client.call("propose_initiative", body, socket_path=route.path)
        except control_client.ControlError as e:
            raise RuntimeError(f"propose_initiative failed ({e.code}): {e.message}") from e
    return PlanningClient().propose_initiative(body)


@mcp.tool()
def add_initiative_project(
    initiative_id: str, project: str, role: str = "",
    project_channel: str | None = None,
) -> dict:
    """Propose a project membership for an initiative."""
    body = {"project": project, "role": role}
    if project_channel is not None:
        body["project_channel"] = project_channel
    route = control_client.resolve_socket_route()
    if route.chose_daemon:
        try:
            return control_client.call("propose_initiative_project", {
                "initiative_id": initiative_id, **body,
            }, socket_path=route.path)
        except control_client.ControlError as e:
            raise RuntimeError(
                f"propose_initiative_project failed ({e.code}): {e.message}"
            ) from e
    return PlanningClient().add_initiative_project(initiative_id, body)


@mcp.tool()
def list_tasks(
    project: str | None = None,
    status: str | None = None,
    source: str | None = None,
    initiative_id: str | None = None,
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
        # creds). Push the HTTP-supported filters through the daemon so a
        # large unfiltered board never has to cross the 4 MiB framed socket.
        filters = {
            key: value
            for key, value in {
                "project": project, "status": status, "initiative_id": initiative_id,
            }.items()
            if value is not None
        }
        tasks = control_client.call("list_tasks", filters, socket_path=route.path)
    else:
        client = PlanningClient()
        tasks = client.list_tasks(
            project=project, status=status, initiative_id=initiative_id,
        )
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


def _require_reparent_in_scope(task_id: str, new_parent: str) -> None:
    """A reparent moves a task between trees, so both the task and its new
    parent must be the caller's task or descendants of it (or the caller
    holds global perms). The daemon answers from the tree it scopes every
    Session method with. This guards the direct-API branch below, which has
    no daemon in the write path; the daemon route checks the same itself.
    Fails closed when the daemon can't answer.
    """
    try:
        answer = control_client.call("task.scope_check", {"task_ids": [task_id, new_parent]})
    except Exception as exc:  # noqa: BLE001 — any failure refuses the move
        raise ValueError(
            f"update_task: can't verify the reparent of {task_id} under {new_parent} "
            f"(scope check unavailable: {exc}); a task can only be moved within your own tree"
        ) from exc
    in_scope = answer.get("in_scope") or {}
    outside = [t for t in (task_id, new_parent) if not in_scope.get(t)]
    if outside:
        raise ValueError(
            "update_task: " + " and ".join(outside) + " not the caller's task or a descendant; "
            "a task can only be moved within your own tree"
        )


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
    initiative_id: str | None = None,
    metadata: dict | None = None,
    reason: str | None = None,
    blocked_by: list[int] | None = None,
) -> dict:
    """Edit a task's planning fields.

    This tool can modify ANY task — including tasks created by the human
    owner (source="user"). That power cuts both ways: be conservative when
    touching user tasks. Don't rewrite their description, change scope, or move
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
        description: New description: the task's whole brief, including
            any worker instructions.
        prompt: Deprecated. Ignored when the description already contains
            it; any other prompt is refused with `prompt_deprecated`
            (put instructions in `description`).
        status: "draft", "backlog", "running", "blocked", "done", or "archived".
        priority: Lower number = higher priority.
        difficulty: 1-10.
        project: Reassign to a different project.
        depends: Replace the dependency list (task slugs).
        parent_task_id: Reparent this task under another task's UUID,
            or "null" to detach (make it top-level). A reparent needs both
            this task and the new parent in your own tree (your task or its
            descendants), unless you hold global perms. (The planning API
            itself doesn't enforce this; this tool does.)
        initiative_id: Attach to an initiative UUID/slug, or "null" to make
            the task standalone.
        metadata: Free-form JSONB bag for skill/agent attachments. PATCH
            REPLACES the whole object — read the existing `metadata` via
            `get_current_task` (or `get_task`) first and re-send the
            merged dict if you want to preserve other keys. The one
            exception is `lead_session_uid` (the task's lead): leave it out,
            it is kept as stored; change it with `org(action="set_lead")`. The
            design-doc skills nest under a `resume` key
            (`metadata.resume.design_doc_path`,
            `metadata.resume.designer_session_uid`); other skills should
            namespace their own keys here rather than churn the schema.
        reason / blocked_by: the trigger of a `blocked` write. A task placed on
            a board (its `placement` is set) takes its status from the item:
            `blocked` becomes the item's blocked_on=reason (or blocked_by=[item
            numbers]); with neither, the item gets a labelled default trigger
            and a legacy_blocked flag asking you for the real one. `done`,
            `running` and `backlog` (unplace) translate too; other statuses
            are refused with placed_on_board.
    """
    if metadata is not None and "lead_session_uid" in metadata:
        raise ValueError(
            "update_task can't set metadata.lead_session_uid (the task's lead); "
            'use org(action="set_lead") — leave the key out and the stored lead is kept'
        )
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
            "initiative_id": initiative_id if initiative_id != "null" else None,
            "reason": reason,
            "blocked_by": blocked_by,
        }.items() if v is not None
    }
    if initiative_id == "null":
        fields["initiative_id"] = None
    if parent_task_id is not None:
        fields["parent_task_id"] = None if parent_task_id == "null" else parent_task_id
    if not fields:
        raise ValueError("No fields to update — pass at least one field.")
    if fields.get("parent_task_id"):
        _require_reparent_in_scope(task_id, fields["parent_task_id"])
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
         workspace_id: str | null,  # ← the workspace you live in
         owner_availability: {level, set, changed_at, age_s, ...},
         staffing: {version, digest, policy, recommended_engine, source},
         member: {id, handle, qualified, title, manager_handle, ...} | null,
         mcp: {version, proxy, update_pending?, note?}}

    `member` is the org member you work as (see `org`), as of the last
    heartbeat (about 30 s); null when you are not one.

    `mcp` is this session's CM MCP server code. `update_pending` names a
    newer deploy; CM activates it on its own when you are idle.

    `owner_availability.level` is informational: away, around, focused,
    on-call, or null when unset. All notifications deliver at every level;
    explicit conversation mutes, DND and read state still apply. chat_open
    reports the same value.

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
        result = control_client.call("ping")
        if isinstance(result, dict):
            result["mcp"] = activation.brief()
        return result
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
                {"session_uid": o.get("session_uid"), "label": o.get("label"),
                 **({"kind": "shell"} if is_shell(o) else {})}
                for o in peers
                if o.get("session_uid") != s.get("session_uid")
            ]


def _list_sessions_raw(task_id: str | None, include_exited: bool, include_shells: bool = False) -> list[dict]:
    """Shared implementation behind `list_sessions` and
    `list_sessions_grouped`: call the host, enrich each entry with the
    legible `status` word and the `workspace_shared_with` hint, and drop
    terminal-geometry noise."""
    params: dict = {"include_exited": include_exited, "include_shells": include_shells}
    if task_id:
        params["task_id"] = task_id
    sessions = control_client.call("list_sessions", params)
    for s in sessions:
        s["kind"] = "shell" if is_shell(s) else "agent"
        s["status"] = _session_status(
            s.get("state", "pending"),
            bool(s.get("idle", False)),
            bool(s.get("reported_done", False)),
            s.get("agent_state"),
        )
        if is_shell(s):
            s["status"] = "exited" if s.get("state") == "exited" else "shell"
            for key in ("agent_state", "reported_done", "reported_done_at", "report_reason",
                        "model", "model_source", "effort", "effort_source"):
                s.pop(key, None)
        for k in _SESSION_NOISE_FIELDS:
            s.pop(k, None)
    _annotate_shared_worktrees(sessions)
    return [s for s in sessions if include_shells or not is_shell(s)]


@mcp.tool()
def list_sessions(
    task_id: str | None = None,
    include_exited: bool = False,
    include_shells: bool = False,
) -> list[dict]:
    """List sessions visible to you.

    Shells are omitted by default. Set include_shells=true to include
    terminal panes labelled kind="shell"; they are never agent workers.

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
        status ∈ {"starting", "working", "awaiting_input", "needs_human",
            "errored", "unknown", "reported", "exited"} — the legible summary; branch on this instead of
            decoding (state, idle) yourself. "reported" means the agent
            declared itself done and has had no new input since — it is
            the one status that means completion. "awaiting_input" means
            only that it stopped talking. See the status legend on
            `wait_for_session_idle`.
        agent_state / pty_idle: engine-reported state and raw terminal quietness,
            when available. needs_human means permission/input is needed;
            errored means an engine failure; unknown never proves idle.
            working-background keeps awaiting_input for compatibility but its
            background jobs may still be running. These are not done reports.
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
    return _list_sessions_raw(task_id, include_exited, include_shells)


@mcp.tool()
def list_sessions_grouped(
    task_id: str | None = None,
    include_exited: bool = False,
    include_shells: bool = False,
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
    sessions = _list_sessions_raw(task_id, include_exited, include_shells)

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
    type: str = "",
    label: str = "",
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
    allow_shared_workspace: bool = False,
    model: str | None = None,
    effort: str | None = None,
    work_kind: str | None = None,
) -> dict:
    """Spawn a new agent or shell session in your workspace.

    With `wait=true` this is the one-call "spawn a worker, get its answer"
    primitive — the cm analogue of Claude Code's `Agent(prompt)`: it
    spawns, waits for the initial `prompt`'s reply, and returns the
    worker's final message inline, so you don't have to hand-write the
    spawn → `wait_for_session_idle` → `read_last_turn` sequence (and its
    post-send race). `wait=false` returns promptly once the daemon accepts
    the launch; its background monitor checks initial delivery before waiting
    for completion. `wait=true` checks delivery before waiting for the reply.
    `isolated=true` additionally gives the worker its own git worktree — the analogue of
    `Agent(isolation="worktree")`.

    Args:
        type: "claude-code", "codex", or "bash". Omit for the cached Owner
            staffing policy (ping().staffing); an explicit type or model wins.
            work_kind may be math, design, architecture, metric, coding or fanout.
            A conflicting choice returns staffing_note with the policy precedence.
            A bash session is a raw
            shell in the workspace's worktree — useful when you want to
            drive a terminal and have the user share it. No MCP
            injection, no transcript, but `send_input` and
            `wait_for_session_idle` still work (writes raw bytes + Enter
            to the PTY; idle flips via the same burst detector as
            agents). A bash session has no transcript, so `wait=true`
            returns after it goes quiet with `last_message=null`.
        label: Sidebar label for the new session.
        prompt: Optional initial prompt to deliver once the session is
            ready. CM confirms a new engine turn or an exact transcript
            receipt (a new main user turn suffices for Claude), with one
            guarded Enter retry if needed. Slash commands count as delivered
            after body plus Enter, without requiring a model turn. For a bash session
            this is just a command line. Required in practice when `wait=true` — there's
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
            which case you may bind the child to any task.
            **A task with no worktree of its own gets one minted**: a
            fresh `cm-sub/<slug>-<id>` branch cut from the project's
            main, in its own checkout, which the worker runs in and
            every later spawn on that task reuses. So propose-then-launch
            hands the worker a clean tree instead of dropping it into
            YOURS. Read `worktree_path` in the response to see where it
            landed. If the worktree can't be minted (no planning row, no
            repo on this host, no resolvable main) the spawn is REFUSED
            rather than silently placed — the error names
            `allow_shared_workspace` as the way through.
        allow_shared_workspace: Opt back into co-tenancy. When `task_id`
            names a task with no worktree, spawn the worker in YOUR
            checkout instead of minting it one. Use it when the worker
            is genuinely meant to work on your tree (or when minting
            isn't possible and you accept the risk) — and know what you
            are accepting: anything it does to the working tree, branch
            switches and `git reset` especially, happens in the tree
            YOUR session and any siblings are live in. The response then
            carries `shared_workspace: true`, a `warning`, and
            `workspace_shared_with` listing the other sessions in that
            checkout. Ignored when the task already has a workspace.
        model: The worker's model id (e.g. "gpt-6-luna" for cheap codex
            work, "claude-sonnet-5-5"). Omitted: the task's default model
            (inherited through parent tasks), else the engine default from
            the host's `[models]` config. CM always passes a model; it never
            leaves the choice to the engine. The result reports `model` and
            `model_source` ("explicit" | "task" | "engine_default"). Not
            valid for bash. An unknown model fails when the engine starts.
        effort: The worker's reasoning effort: "low" | "medium" | "high" |
            "xhigh" | "max" (claude only; codex's highest is xhigh).
            Omitted: the task's default effort, else the host's `[models]`
            default (xhigh). Reported as `effort` / `effort_source`, like
            the model. Not valid for bash.
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
        wait: Block until the spawned agent replies to `prompt`, then
            return its final message (see Returns). For bash, wait for
            PTY quiet or exit and return last_message=null. Default false.
        timeout_s / poll_interval_s / pending_idle_grace_s: Waiting knobs,
            same semantics as `send_input_and_wait`. Only used when wait.
        schema: Agent-only JSON Schema for structured output (only meaningful
            with wait). Bash refuses it before spawning. The initial prompt
            is decorated with a "reply with ONLY JSON matching this schema"
            instruction; the reply is
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
          - `prompt_delivery` — on updated daemons, a receipt with status
            pending/confirmed/delivered/unconfirmed, attempt count, source and
            reason. `wait=false` usually returns pending; the auto-monitor
            checks it in the background and reports either completion or
            uncertain submission. With notify_on_done=false, inspect the receipt
            using read_last_turn. `submitted` is present only when
            the tool has a terminal receipt, never false merely for pending.
            False means unconfirmed; inspect before re-sending to avoid duplicates.
            The session remains available and no completion is claimed.
            Older daemons omit these fields. `prompt_source` identifies text,
            not proof it was submitted.
          - `shared_workspace` / `warning` / `workspace_shared_with` —
            present ONLY when `allow_shared_workspace=true` actually put
            the worker in your own checkout. `workspace_shared_with` is
            `[{session_uid, label}]` for the other live sessions there.
            Treat these as a pre-flight collision warning: if you didn't
            mean to share a tree, kill the worker before it edits.
          - `worktree_recreated` — present ONLY when the task's worktree
            had been reaped and the server re-created it from the task's
            branch before spawning (`{branch, branch_source, head_sha,
            commits_ahead, summary}`). `worktree_path` is guaranteed to
            exist on disk on every success; a worktree that cannot be
            re-created (branch gone, path collision) FAILS the call with
            the path and reason instead.
          - `worktree_warning` — present when something about the
            checkout deserves a look before building on it: the branch
            has no commits beyond trunk (a likely zero-commit decoy — the
            task's real work may be on a same-slug sibling branch, which
            the message names), or the task's recorded `wip_branch`
            disagreed with the branch actually checked out.

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
    if type == "bash" and schema is not None:
        return shell_refusal("start_session(schema=...)")
    if not label.strip():
        return {"error": "invalid_params", "message": "label must be non-empty"}
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

    params: dict = {"label": label}
    if type:
        params["type"] = type
    if work_kind is not None:
        params["work_kind"] = work_kind
    if body:
        params["prompt"] = body
    if task_id:
        params["task_id"] = task_id
    if global_perms:
        params["global_perms"] = True
    # ux-1b: only send the co-tenancy opt-in when it was actually asked
    # for, so an older server (which would reject the unknown field) still
    # serves the default mint path.
    if allow_shared_workspace:
        params["allow_shared_workspace"] = True
    if model and model.strip():
        params["model"] = model.strip()
    if effort and effort.strip():
        params["effort"] = effort.strip()
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

    # wait=false remains a fast spawn/acceptance call. Its monitor consumes
    # the receipt in the background; pending never means submitted=false.
    launch_receipt = spawn.get("prompt_delivery") if isinstance(spawn, dict) else None
    if isinstance(launch_receipt, dict) and spawn.get("session_uid"):
        if wait:
            launch_receipt = await _await_launch_confirmation(
                spawn["session_uid"], launch_receipt, socket_path=route.path,
            )
            spawn["prompt_delivery"] = launch_receipt
        if launch_receipt.get("status") != "pending":
            spawn["submitted"] = launch_receipt.get("submitted") is True

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
        `prompt_source` that `wait=false` callers get — plus the ux-1b
        co-tenancy guard fields, which a `wait=true` caller needs at least
        as much (it's about to read a reply from a worker sharing its
        tree)."""
        if isinstance(d, dict) and isinstance(spawn, dict):
            for key in (
                "staffing_note", "staffing_version", "session_type", "kind",
                "worktree_path",
                "task_id",
                "prompt_source",
                "submitted",
                "prompt_delivery",
                "shared_workspace",
                "warning",
                "workspace_shared_with",
                "worktree_recreated",
                "worktree_warning",
                "model",
                "model_source",
                "effort",
                "effort_source",
            ):
                if spawn.get(key) is not None and d.get(key) is None:
                    d[key] = spawn[key]
        return d

    if wait and isinstance(launch_receipt, dict) and not launch_receipt["submitted"]:
        spawn.update(completed=False, timed_out=False, last_message=None)
        return _with_isolation(spawn)

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
                    # Confirmation gates the level watch so startup cannot
                    # fire it, while a fast first turn/report_done is retained.
                    edge=not isinstance(launch_receipt, dict),
                    **({"launch_receipt": launch_receipt, "launch_socket_path": route.path}
                       if isinstance(launch_receipt, dict) else {}),
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
    # The receipt captures the counter before the launch body is written.
    # Anchor there even after confirmation, retaining a fast completed launch
    # turn while rejecting startup idle. Old receipts use a fresh counter of 0.
    # Legacy engines still use their initial transcript cursor.
    if type == "bash" or is_shell(spawn):
        quiet = await wait_for_session_idle(
            session_uid, timeout_s=max(1.0, deadline - time.monotonic()),
            poll_interval_s=interval, pending_idle_grace_s=grace,
        )
        res = {**quiet, "completed": quiet.get("idle", False) and not quiet.get("timed_out", False),
               "last_message": None}
    else:
        res = await _await_reply(
            session_uid,
            engine="claude-code", transcript_path=None,
            anchor_cursor=None, generation=0,
            deadline=deadline, interval=interval, grace=grace,
            agent_baseline=({"kind": "agent", "value": (
                launch_receipt.get("turn_seq_before", 0)
                if isinstance(launch_receipt, dict) else 0
            ), "no_turn_at": None} if delivered_a_prompt else None),
        )
    res["session_uid"] = session_uid
    if schema is not None:
        res = await _settle_schema(
            session_uid, res, schema, schema_retries,
            deadline=deadline, interval=interval, grace=grace,
        )
    return _with_isolation(_with_spawn_fields(res))


@mcp.tool()
async def fork_session(
    session_uid: str,
    task_name: str | None = None,
    task_id: str | None = None,
    base: str = "source",
    parent_task_id: str | None = None,
    prompt: str | None = None,
    request_id: str | None = None,
    top_level: bool = False,
    label: str | None = None,
    notify_on_done: bool = True,
    notify_until: str = "turn_end",
    model: str | None = None,
    effort: str | None = None,
) -> dict:
    """Fork a Claude or Codex session's conversation into a task.

    Starts a session that continues from a copy of the source's
    conversation via the engine's native fork (`claude --resume <id>
    --fork-session` / `codex fork <id>`). The source keeps running
    unchanged; CM stores no history copy. Pass exactly one of:

    - `task_name`: a NEW task with its own `cm-sub/...` worktree. Parent
      defaults to the source session's task; `parent_task_id` overrides it
      (must be within your task tree) and `top_level=true` files it
      top-level in the same project (global permissions only).
    - `task_id`: an EXISTING task. The fork joins that task's workspace
      exactly like `start_session(task_id=…)` — shared checkouts are fine
      and reported in `workspace_shared_with`; a task that never ran gets
      a worktree minted at `base`.

    Args:
        session_uid: The session to fork (must have completed a turn).
        base: "source" (default) cuts new branches at the source worktree's
            committed HEAD; "trunk" at the project's main branch. Uncommitted
            source edits never come along (`uncommitted_left_behind`).
        prompt: Optional first message for the fork. When blank NOTHING is
            typed: the fork waits at its composer (a Claude fork writes its
            transcript only on its first message, so an unprompted fork
            cannot be resumed after a daemon restart). Where it came from is
            in the result instead (`forked_from`, `commits_not_carried`).
        request_id: Idempotency key; generated per call when omitted. A
            retry with the same key returns the first fork.
        label: Sidebar label for the new session (default: the task name).
        model: The fork's model; omitted = the source session's model.
        effort: The fork's reasoning effort; omitted = the source's.
        notify_on_done / notify_until: As for `start_session` — with a
            prompt, a self-waking monitor is registered on the fork.

    Returns `{task_id, session_uid, worktree_path, branch, base_sha,
    forked_from: {session_uid, label, branch, ...}, commits_not_carried
    (`git log --oneline` lines on the source the fork's checkout lacks,
    capped at 10; `commits_not_carried_truncated`), engine, target,
    prompt_source ("caller" | "none"), uncommitted_left_behind, ...}`.

    State your intent in plain language and ask the user to confirm
    before calling this tool.
    """
    params: dict = {
        "source_uid": session_uid,
        "base": base,
        "request_id": request_id or str(uuid.uuid4()),
    }
    for key, value in (
        ("task_name", task_name), ("task_id", task_id),
        ("parent_task_id", parent_task_id), ("label", label),
        ("model", model.strip() if model else None),
        ("effort", effort.strip() if effort else None),
    ):
        if value:
            params[key] = value
    if top_level:
        params["top_level"] = True
    if prompt and prompt.strip():
        params["prompt"] = prompt
    socket_path = control_client.resolve_socket_for_method("session.fork")
    try:
        res = await asyncio.to_thread(
            control_client.call, "session.fork", params,
            # The host mints a task + worktree and provisions it before it
            # replies; the request_id makes a post-timeout retry safe.
            timeout=FORK_SESSION_TIMEOUT_S, socket_path=socket_path,
        )
    except control_client.ControlError as e:
        if e.code == "unknown_method":
            return {"error": e.code, "message": FORK_UNSUPPORTED_MESSAGE}
        raise
    if not isinstance(res, dict):
        return res
    receipt = res.get("prompt_delivery")
    if isinstance(receipt, dict) and receipt.get("status") != "pending":
        res["submitted"] = receipt.get("submitted") is True
    fork_uid = res.get("session_uid")
    name = task_name or res.get("task_name") or task_id
    if notify_on_done and fork_uid and res.get("prompt_source") == "caller":
        try:
            res["monitor"] = async_monitor.register_monitor(
                [fork_uid], mode="any", until=notify_until,
                note=(
                    f"fork '{name}' reported done"
                    if notify_until in ("final", "task_done")
                    else f"fork '{name}' finished its first prompt"
                ),
                source="auto",
                edge=not isinstance(receipt, dict),
                **({"launch_receipt": receipt, "launch_socket_path": socket_path}
                   if isinstance(receipt, dict) else {}),
            )
        except async_monitor.RegistrationError as e:
            res["monitor"] = {"error": e.code, "message": str(e)}
    return res


FORK_SESSION_TIMEOUT_S = 180.0
FORK_UNSUPPORTED_MESSAGE = (
    "The host daemon is too old for fork_session (no session.fork method); "
    "it needs a brain deploy."
)


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

    Bash targets receive a shell command followed by a newline.
    They do not register agent completion monitors.

    State your intent and ask the user before calling this tool.
    """
    res = await asyncio.to_thread(
        control_client.call,
        "send_input",
        {"session_uid": session_uid, "text": text, "submit": submit},
    )
    if isinstance(res, dict) and is_shell(res):
        res["note"] = "Shell input delivered; agent completion monitoring does not apply."
        return res
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
            if e.code == "agent_only":
                # Older TUI replies omit kind and only identify the shell
                # during monitor registration. Preserve an unknown submitted
                # value instead of turning queued input into a false receipt.
                res["kind"] = "shell"
                res["note"] = "Shell input accepted; automatic agent monitoring does not apply."
            else:
                res["monitor"] = {"error": e.code, "message": str(e)}
    return res


SEND_KEY_MAX_TARGETS = 50


@mcp.tool()
async def send_key(
    session_uids: str | list[str],
    key: str,
    notify_on_done: bool = False,
    notify_until: str = "turn_end",
) -> dict:
    """Press exactly ONE key in one or more sessions you can see.

    It sends only that key: no text, and no Enter after it. Use it to
    revive a rate-limited or errored session (`key="enter"` on its error
    or limit screen), to answer a menu (`"1"`, `"y"`, `"down"` then
    `"enter"`), or to back out of one (`"escape"`). To send a prompt, use
    `send_input`.

    Args:
        session_uids: One session uid, or a list of up to 50.
        key: A named key — enter, escape, tab, shift-tab, up, down, left,
            right, home, end, pageup, pagedown, backspace, delete, space,
            ctrl-c, ctrl-d — or any single printable character (`1`, `y`).
            It is encoded for each session's keyboard mode (kitty or
            legacy), the bytes a real keypress would send. Each result
            says the `keyboard` used and its `mode`: "observed" (tracked
            from the session's output) or "assumed" (unknown after a
            restart; the engine's default is used).
        notify_on_done: Register an async monitor on the sessions that got
            the key, waking you when they finish the turn it starts (all of
            the agents, when there are several). Shells are omitted from
            this automatic monitor. Default false: a key often
            starts no turn at all (menu navigation).
        notify_until: "turn_end" (default) or "final"; see `monitor_sessions`.

    A key is not new work: it does not supersede a session's `reported`
    status (`report_done`) and does not re-arm watches on it.

    Scope is send_input's: your own task's sessions and its descendants
    (any session with global permissions). Every session gets its own
    result in `results`: status `ok`, `not_found`, `unauthorized`,
    `exited`, or `error`; one bad target never stops the rest.

    State your intent and ask the user before calling this tool.
    """
    uids = [session_uids] if isinstance(session_uids, str) else list(session_uids)
    if len(uids) > SEND_KEY_MAX_TARGETS:
        return {
            "error": "invalid_params",
            "message": f"send_key takes at most {SEND_KEY_MAX_TARGETS} sessions per call, got {len(uids)}",
        }
    res = await asyncio.to_thread(control_client.call, "send_key", {"session_uids": uids, "key": key})
    if notify_on_done and isinstance(res, dict) and "error" not in res:
        sent = [r["session_uid"] for r in res.get("results", []) if r.get("status") == "ok"]
        agents = []
        for uid in sent:
            try:
                target = await asyncio.to_thread(control_client.call, "resolve_authorized_session", {"session_uid": uid})
            except (control_client.ControlError, control_client.TransportError):
                continue
            if not is_shell(target):
                agents.append(uid)
        sent = agents
        if sent:
            try:
                res["monitor"] = async_monitor.register_monitor(
                    sent, mode="all", until=notify_until,
                    note=f"turn after send_key {key} to {len(sent)} session(s)",
                    source="auto",
                )
            except async_monitor.RegistrationError as e:
                res["monitor"] = {"error": e.code, "message": str(e)}
    return res


@mcp.tool()
def list_sidebar_sections() -> dict:
    """List Owner's work-sidebar subsections and caller-visible workspace choices.

    Headless/cloud-safe. The catalogue and observed choices are the latest
    laptop viewer publication, not a live viewer query. Pending requests are
    shown separately. Only sessions on this daemon are included. Sections are
    display groups, separate from first-class planning initiatives.
    """
    return control_client.call("sidebar.list", {})


@mcp.tool()
def set_session_section(section: str, session_id: str | None = None) -> dict:
    """Move a session's WHOLE workspace to a work-sidebar subsection.

    section: stable ID or unique exact name from list_sidebar_sections;
      "auto" restores parent-task inheritance, "none" explicitly leaves it loose.
    session_id defaults to yourself; use a CM session UID from list_sessions.
    Requires ordinary session-control scope for every live workspace member.
    Auto descendants follow their parent; explicit descendant choices remain.
    This is a durable queued display change, applied when the updated viewer
    connects. Read list_sidebar_sections again to verify its observed choice
    and receipt. Does not move files, change initiative/task ownership, create
    sections, or restart workers. Follow Owner's requested organization.
    """
    params = {"section": section}
    if session_id is not None:
        params["session_id"] = session_id
    return control_client.call("sidebar.assign", params)


@mcp.tool()
def notify_user(message: str = "", urgency: str = "decision") -> dict:
    """Request Owner attention for your own session (local, cloud, or continuous).

    Continuous workers post routine reviews, progress, recoverable failures and
    handoffs in their task channel mentioning the orchestrator; do not notify
    Owner for those events.
    Orchestrators use this only for a reviewed decision/blocker requiring Owner,
    preserving stricter quiet policies. Interactive sessions use it when Owner
    action is needed. Supply a concise reason and relevant message/file link.
    Routine progress stays in session chat; this does not authorize deployment,
    session control, or unsolicited Owner DMs.

    urgency is honest: "fyi" (no action needed soon), "decision" (default:
    Owner must choose something), "blocking" (you or others cannot proceed),
    "emergency" (harm or loss is happening now; also pushed to Owner's phone).
    Owner's availability (ping().owner_availability.level) is informational.
    All urgencies deliver at every level, including unset; availability never
    holds, drops or downgrades notifications. The result says
    delivery="immediate" for acceptance into the normal queue. Explicit chat
    mutes, DND, read state and device notification settings still apply.

    A delivered alert stays until Owner selects the session. A connected
    updated TUI submits the normal desktop notification and blinks the sidebar;
    otherwise delivery waits for reconnection (blocking and emergency requests
    also reach notify_command when no viewer is connected). Identical pending
    reasons coalesce; a changed reason raises a new alert. status="queued" is
    not proof of desktop delivery/read.

    message is optional (maximum 4096 UTF-8 bytes). Identity and label come from
    the live caller, never a target parameter; no global permission is required.
    See doc/OWNER_NOTIFICATIONS.md.
    """
    params = {"message": message}
    # Omit the default so a daemon predating urgency still accepts the call.
    if urgency != "decision":
        params["urgency"] = urgency
    try:
        return control_client.call("notify_user", params)
    except control_client.ControlError as e:
        # A daemon predating urgency rejects the unknown field; never fail the
        # request over it. Deliver it ungated and say so.
        if "urgency" not in params or "urgency" not in str(e) or "unknown field" not in str(e):
            raise
        result = control_client.call("notify_user", {"message": message})
        if isinstance(result, dict):
            result["urgency_ignored"] = (
                "This host's daemon predates urgency; the request was delivered without it."
            )
        return result


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
    "prompt_delivery",
    "agent_state",
    "pty_idle",
)


def _with_outcome(out: dict, resolved: dict) -> dict:
    """Carry the resolve payload's outcome fields onto a read result."""
    for key in _OUTCOME_FIELDS:
        if resolved.get(key) is not None:
            out[key] = resolved[key]
    if (out.get("completed") or out.get("timed_out") is False) and resolved.get("state") != "exited" and no_turn_at(engine_state(resolved)) is not None:
        out["completion_kind"] = "no_turn"
        out["last_message"] = None
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
        - status: "starting"|"working"|"awaiting_input"|"needs_human"|
          "errored"|"unknown"|"reported"|"exited" — the legible summary; "reported" means the agent
          called `report_done`.
        - When state="pending", messages is empty and you can poll again.
        - note: present only when no transcript is bound (messages empty).
          Explains the bash-session read dead-end and what to do instead.
        - Outcome fields, present when the daemon knows them:
          prompt_delivery (the initial launch receipt),
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

    if is_shell(resolved):
        return shell_refusal("read_session_output")
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
            "status": _session_status(state, idle, reported, resolved.get("agent_state")),
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
        "status": _session_status(state, idle, reported, resolved.get("agent_state")),
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
        - status: "starting"|"working"|"awaiting_input"|"needs_human"|
          "errored"|"unknown"|"reported"|"exited". "reported" is the only one that means the agent
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

    if is_shell(resolved):
        return shell_refusal("read_last_turn")
    state = resolved.get("state", "pending")
    engine = resolved.get("engine", "claude-code")
    transcript_path = resolved.get("transcript_path")
    generation = int(resolved.get("generation", 0))
    idle = bool(resolved.get("idle", False))
    status = _session_status(
        state, idle, bool(resolved.get("reported_done", False)), resolved.get("agent_state")
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
    # Bounded: the last n messages and the last reply come from a backward
    # scan; the cursor from a line count. A full parse of a large transcript
    # pinned hundreds of MB in the worker thread's arena.
    tail, last, cursor = transcript_stream.tail(
        transcript_path, generation, n, _parser_for(engine).render_message,
    )
    return _with_outcome({
        "last_assistant": last,
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

        active_uid = None
        if active_role and active_role in role_sessions:
            active_uid = (role_sessions[active_role] or {}).get("daemon_session_uid")

        active_idle = False
        if active_role:
            sessions = await asyncio.to_thread(
                control_client.call, "list_sessions", {"include_exited": False}
            )
            if not active_uid:
                candidates = [s for s in sessions if s.get("workflow_run_id") == run_id
                              and s.get("workflow_role") == active_role]
                if len(candidates) == 1:
                    active_uid = candidates[0].get("session_uid")
                else:
                    state["binding_warning"] = "Active role has no unambiguous stable session binding"
            for session in sessions:
                if active_uid and session.get("session_uid") == active_uid:
                    agent_state = session.get("agent_state")
                    active_idle = (
                        agent_state.get("state") == "idle"
                        if isinstance(agent_state, dict)
                        else bool(session.get("idle", False))
                    )
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
    """Block until an agent becomes idle or a shell's PTY goes quiet or exits.

    Shells use PTY activity only, without a transcript grace period or
    assistant reply. Results carry kind="shell", idle_source="pty" and
    status="quiet", "working" (timeout) or "exited". PTY quiet does not
    prove a command succeeded.

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
    NEVER bind a transcript (claude/codex on a daemon
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
          (transcript never bound — e.g. a detector-less daemon).
        - status="working" (idle=False, timed_out=True): deadline
          reached while still busy.

    With agent_state, engine evidence governs the wait: idle, needs_human,
    errored, or working-background after a foreground turn end can return.
    Working and starting keep waiting even with idle=True. Unknown returns
    only when explicit pty_idle=True, preserving status=unknown and labeling
    idle_source=pty; reply/edge monitors do not use that exception. Results
    include agent_state and pty_idle; a human wait or error is not finished work.
    Legacy PTY/transcript fallback retains the quietness behavior below.

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
        if is_shell(resolved):
            quiet = state == "exited" or bool(resolved.get("pty_idle", idle))
            if quiet or now >= deadline:
                return {"kind": "shell", "state": state, "idle": quiet,
                        "timed_out": not quiet, "idle_source": "pty",
                        "status": "exited" if state == "exited" else "quiet" if quiet else "working"}
            await asyncio.sleep(interval)
            continue
        if state == "exited":
            return _with_outcome({
                "idle": True, "timed_out": False, "state": state,
                "status": _session_status(state, idle, reported, resolved.get("agent_state")),
            }, resolved)
        engine_done = engine_turn_complete(resolved)
        if engine_done is not None:
            if ((engine_state(resolved) or {}).get("state") == "unknown"
                    and resolved.get("pty_idle") is True):
                return _with_outcome({
                    "idle": True, "timed_out": False, "state": state,
                    "status": _session_status(state, idle, reported, resolved.get("agent_state")),
                    "idle_source": "pty",
                }, resolved)
            if engine_done or now >= deadline:
                return _with_outcome({
                    "idle": engine_done, "timed_out": not engine_done, "state": state,
                    "status": _session_status(state, idle, reported, resolved.get("agent_state")),
                }, resolved)
            await asyncio.sleep(interval)
            continue
        # A READY session (transcript bound) that's quiet is unambiguously
        # done with its turn — return immediately.
        if state == "ready" and idle:
            return _with_outcome({
                "idle": True, "timed_out": False, "state": state,
                "status": _session_status(state, idle, reported, resolved.get("agent_state")),
            }, resolved)
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
                    return _with_outcome({
                        "idle": True, "timed_out": False, "state": state,
                        "status": "reported" if reported else "awaiting_input",
                        "idle_source": "transcript",
                    }, resolved)
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
                return _with_outcome({
                "idle": True, "timed_out": False, "state": state,
                "status": _session_status(state, idle, reported, resolved.get("agent_state")),
            }, resolved)
        else:
            pending_idle_since = None
        if now >= deadline:
            return _with_outcome({
                "idle": False, "timed_out": True, "state": state,
                "status": _session_status(state, idle, reported, resolved.get("agent_state")),
            }, resolved)
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
    agent_baseline: dict | None = None,
) -> dict:
    """Poll a session until its next reply lands and it goes quiet, or the
    deadline passes. Completion is anchored on transcript progress past
    `anchor_cursor` (a NEW assistant message must appear), so it can't
    return the turn that was already there when polling began.

    When an engine counter was captured before sending, a newer turn_seq plus
    a boundary/attention state is the completion evidence. Transcript reads only
    supply message content; errors and human waits can return without a reply.

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
            return last_assistant_message(engine, transcript_path) or last_message
        except OSError:
            return last_message

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
        if is_shell(resolved):
            return shell_refusal("await_reply")
        state = resolved.get("state", "pending")
        idle = bool(resolved.get("idle", False))
        # Same as the wait loop above: the agent's own done signal, so the
        # `status` this returns agrees with what a listing would say.
        reported = bool(resolved.get("reported_done", False))
        # /clear and /new can bind the replacement transcript only once the
        # next prompt starts. Cursors and cached replies belong to one binding.
        current_path = resolved.get("transcript_path")
        current_generation = int(resolved.get("generation", generation))
        current_engine = resolved.get("engine", engine)
        if current_path and (current_path, current_generation, current_engine) != (
            transcript_path, generation, engine
        ):
            transcript_path, generation, engine = current_path, current_generation, current_engine
            cursor = None
            saw_new_assistant = False
            last_message = None
            semantic_idle_since = None
        now = time.monotonic()
        engine_done = engine_turn_complete(resolved)

        if state == "exited":
            last_message = await asyncio.to_thread(_final_read)
            out = {
                "completed": True, "timed_out": False,
                "status": "exited", "state": "exited", "idle": True,
                "last_message": last_message,
            }
            if last_message is None and transcript_path is None:
                out["note"] = _NO_TRANSCRIPT_NOTE
            return _with_outcome(out, resolved)

        if transcript_path is not None:
            # Transcript-anchored: pull new messages since the anchor, latch
            # the reply, complete only when a NEW assistant message exists
            # AND the session is quiet at the prompt.
            try:
                new_msgs, cursor = await asyncio.to_thread(
                    _parser_for(engine).read_messages,
                    transcript_path, generation, cursor, _READ_ALL_LIMIT,
                )
            except OSError:
                if engine_done is None:
                    raise
                new_msgs = []
            for m in new_msgs:
                if m.role == Role.ASSISTANT:
                    saw_new_assistant = True
                    last_message = m.to_dict()
            if engine_done is None and saw_new_assistant and state == "ready" and idle:
                return _with_outcome({
                    "completed": True, "timed_out": False,
                    "status": _session_status(state, idle, reported, resolved.get("agent_state")),
                    "state": state, "idle": idle, "last_message": last_message,
                }, resolved)
            # New assistant reply + PTY still noisy: a background
            # task's spinner can hold `idle` false forever. Fall back
            # to transcript shape (debounced) — see monitor.py. A
            # daemon hook-confirmed `semantic_idle` skips the debounce.
            if engine_done is None and saw_new_assistant and state == "ready":
                if await asyncio.to_thread(
                    transcript_turn_complete, engine, transcript_path
                ):
                    if semantic_idle_since is None:
                        semantic_idle_since = now
                    if resolved.get("semantic_idle") is True or (
                        now - semantic_idle_since >= SEMANTIC_IDLE_GRACE_S
                    ):
                        return _with_outcome({
                            "completed": True, "timed_out": False,
                            "status": (
                                "reported" if reported else "awaiting_input"
                            ),
                            "state": state, "idle": idle,
                            "last_message": last_message,
                            "idle_source": "transcript",
                        }, resolved)
                else:
                    semantic_idle_since = None
        elif engine_done is None:
            # Transcript-less fallback == `wait_for_session_idle` semantics.
            if state == "ready" and idle:
                return _with_outcome({
                    "completed": True, "timed_out": False,
                    "status": _session_status(state, idle, reported, resolved.get("agent_state")),
                    "state": state, "idle": idle, "last_message": None,
                    "note": _NO_TRANSCRIPT_NOTE,
                }, resolved)
            if state == "pending" and idle:
                if pending_idle_since is None:
                    pending_idle_since = now
                elif now - pending_idle_since >= grace:
                    return _with_outcome({
                        "completed": True, "timed_out": False,
                        "status": _session_status(state, idle, reported, resolved.get("agent_state")),
                        "state": state, "idle": idle, "last_message": None,
                        "note": _NO_TRANSCRIPT_NOTE,
                    }, resolved)
            else:
                pending_idle_since = None

        if engine_done is not None:
            edge_ok = (
                _edge_passed(agent_baseline, engine, transcript_path, resolved)
                if agent_baseline is not None else anchor_cursor is None or saw_new_assistant
            )
            if engine_done and edge_ok:
                out = _with_outcome({
                    "completed": True, "timed_out": False,
                    "status": _session_status(state, idle, reported, resolved.get("agent_state")),
                    "state": state, "idle": idle, "last_message": last_message,
                }, resolved)
                if transcript_path is None:
                    out["note"] = _NO_TRANSCRIPT_NOTE
                return out
            if now >= deadline:
                return _with_outcome({
                    "completed": False, "timed_out": True,
                    "status": _session_status(state, idle, reported, resolved.get("agent_state")),
                    "state": state, "idle": idle, "last_message": last_message,
                }, resolved)
        if now >= deadline:
            return _with_outcome({
                "completed": False, "timed_out": True,
                "status": _session_status(state, idle, reported, resolved.get("agent_state")),
                "state": state, "idle": idle, "last_message": last_message,
            }, resolved)
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
        if is_shell(pre):
            return shell_refusal("send_and_await")
        engine = pre.get("engine", "claude-code")
        transcript_path = pre.get("transcript_path")
        generation = int(pre.get("generation", 0))
        if transcript_path is not None:
            # Only the end cursor is needed: count lines, parse nothing.
            anchor_cursor = await asyncio.to_thread(
                transcript_stream.end_cursor, transcript_path, generation
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
        agent_baseline=({"kind": "agent", "value": engine_state(pre).get("turn_seq"),
                         "no_turn_at": no_turn_at(engine_state(pre))}
                        if engine_state(pre) is not None else None),
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
            or res.get("completion_kind") == "no_turn"
            or res.get("status") in ("needs_human", "errored", "unknown")
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
    reports after `pending_idle_grace_s`). Shells wait for PTY quiet or
    exit without a transcript grace period and return kind="shell",
    idle_source="pty", and last_message=null. SAME race caveat too: a session
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
                you're driving a conversation turn by turn. For shells,
                wait for PTY quiet or exit and return a shell-labelled
                result without an assistant reply. Quiet output does not
                prove a command succeeded.
            "final" — only an EXIT or an explicit `report_done`. Every
                interim turn end silently re-arms the watch, so a worker
                that pauses mid-task, asks itself a question, or ends a
                turn with background subagents still running will not
                wake you; you're woken when it is actually done. Right
                for fan-outs you want to collect once. Use a generous
                `timeout_s`, and note that a worker which never calls
                `report_done` will only complete this watch by exiting.
                Bash shells are refused; they have no agent turns. A final watch also holds
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
            Shells have no transcript edge; either setting waits for PTY quiet.

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
    model: str | None = None,
    model_engine: str | None = None,
    effort: str | None = None,
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
        prompt: Optional brief for the subtask's worker, stored as the
            task's description (tasks have one brief; the name is
            historical). `start_session(task_id=<task_id>)` with no prompt
            delivers it.
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
        model: Optional default model for the subtask (stored as
            `metadata.models.<engine>`); its sessions and its own subtasks
            use it unless a spawn names another. Omitted: it inherits your
            task's default. `model_engine` ("claude" | "codex") picks the
            slot; it is inferred from the model id (claude-* is claude;
            gpt-*, luna, sol and astra are codex) and required when the id
            doesn't say.
        effort: Optional default reasoning effort for the subtask
            (`metadata.efforts.<engine>`): "low" | "medium" | "high" |
            "xhigh" | "max" (claude only). Same slot rule as `model`; an
            effort alone needs `model_engine`.

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
    if model and model.strip():
        params["model"] = model.strip()
    if effort and effort.strip():
        params["effort"] = effort.strip()
    if model_engine:
        params["model_engine"] = model_engine
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
def set_subtask_status(status: str, task_id: str | None = None, reason: str | None = None,
                       blocked_by: list[int] | None = None) -> dict:
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
    # A placed task's blocked write needs its trigger (doc/org-model.md §7.1).
    if reason:
        params["reason"] = reason
    if blocked_by:
        params["blocked_by"] = blocked_by
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
def get_continuous_context() -> dict:
    """Read your continuous task's stop request, admitted run and checkpoint.

    A stop request asks you to finish the current period/claimed batch and its
    workers. New periods are gated by the daemon. Receipt is not completion.
    `recovery_hold` is set while your task is held for recovery; when
    `self_clear` is present you may clear it with clear_continuous_hold.
    """
    return control_client.call("continuous.context", {})


@mcp.tool()
def acknowledge_continuous_drain(request_id: str) -> dict:
    """Acknowledge receipt of this stop request; keep finishing admitted work.

    Use the request_id from get_continuous_context or the stop notice. The
    daemon binds it to your session and admitted run; a resumed/replaced
    request cannot be acknowledged by stale work.
    """
    return control_client.call("continuous.ack_drain", {"request_id": request_id})


@mcp.tool()
def checkpoint_continuous_drain(request_id: str, notes: str,
                               background_work_complete: bool,
                               work_reconciled: bool) -> dict:
    """Record a checkpoint after finishing the admitted work and its workers.

    Read final monitors, reconcile batch outcomes/artifacts and checkpoint
    pending next actions on disk first. Set both flags true only after all
    background jobs finish and that reconciliation is done. Notes describe
    the checkpoint locations/outcomes. The daemon independently checks known
    workers and durable monitor deliveries. After success call report_done,
    finish your final response, and end the turn. Never start a new period.
    """
    async_monitor.persist_state()
    return control_client.call("continuous.checkpoint_drain", {
        "request_id": request_id, "notes": notes,
        "background_work_complete": background_work_complete,
        "work_reconciled": work_reconciled,
    })


@mcp.tool()
def clear_continuous_hold(reason: str, dry_run: bool = False) -> dict:
    """Clear your continuous task's recovery hold when nothing is in flight.

    Only the task's current orchestrator can call this, for a recovery hold
    whose run staged no batch items. The daemon allows it only if every
    session of the held run has exited or reported done with its final turn
    ended, no monitor delivery is undelivered or uncertain, no notification
    to a live session is unconfirmed, and no background job is running.
    `reason` (1–2048 bytes) says why the held run is settled; it is recorded
    in the task state and the run's evidence. `dry_run=True` returns
    {eligible, blockers} without recording anything.

    A refusal names each blocker. Those cases, staged batch items, and
    capacity holds (the scheduler resumes those itself) stay with the
    operator's continuous.reconcile. After success, finish your reply and end
    the turn: recovery retires this thread and fires a fresh one.
    """
    async_monitor.persist_state()
    return control_client.call("continuous.self_clear_hold",
                               {"reason": reason, "dry_run": dry_run})


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
    "machine_type": os.getenv("CM_BACKTEST_MACHINE_TYPE", "n2-standard-4"),  # 2026-08-24: n2 measured 18 % faster than c2 on identical t2 content (perf round 2) and sits under the 200-vCPU N2 quota vs C2's 100; env override mirrors dispatch/config.py
    "image_family": "cm-backtest-worker",
    "image_project": "prediction-market-scalper",
}
# GCE instance metadata caps values at 256KB; the whole backtest payload rides
# one metadata attribute, so keep inline configs well under that.
_BACKTEST_CONFIG_MAX_BYTES = 32 * 1024
_BACKTEST_TERMINAL_STATUSES = ("done", "blocked", "archived")
_BACKTEST_DEFAULT_SCRIPT = "analysis.backtests.backtest_actrader_grid"


def _normalize_filer_agent(session_type: str | None) -> str:
    """Map CM's wire session types to the short filer vocabulary."""
    return {
        "claude": "claude",
        "claude-code": "claude",
        "codex": "codex",
        "bash": "bash",
    }.get((session_type or "").strip().lower(), "unknown")


def _infer_model_engine(model: str) -> str | None:
    """The engine a model id obviously belongs to (mirrors the daemon's
    ``models::infer_engine`` family rules), else None."""
    lower = model.strip().lower()
    words = re.split(r"[-./:@]", lower)
    first = words[0] if words else ""
    if lower.startswith("claude") or first in ("opus", "sonnet", "haiku", "fable"):
        return "claude"
    o_series = len(first) >= 2 and first[0] == "o" and first[1:].isdigit()
    if (lower.startswith(("gpt-", "codex")) or o_series
            or any(w in ("luna", "sol", "astra") for w in words)):
        return "codex"
    return None


def _task_model_slot(model: str, model_engine: str | None) -> tuple[str | None, str | None]:
    """The ``metadata.models`` slot for a filed model, or an error: the
    named ``model_engine`` (refusing an obvious mismatch), else the
    engine the model id names."""
    slots = {"claude-code": "claude", "claude": "claude", "codex": "codex"}
    inferred = _infer_model_engine(model)
    if model_engine:
        slot = slots.get(model_engine.strip())
        if slot is None:
            return None, f'model_engine must be "claude" or "codex", not {model_engine!r}'
        if inferred and inferred != slot:
            return None, f"model '{model}' is a {inferred} model, but model_engine is {slot}"
        return slot, None
    if inferred is None:
        return None, (f"can't tell which engine model '{model}' is for; "
                      'pass model_engine="claude" or "codex"')
    return inferred, None


# CM's effort levels (daemon/src/models.rs::EFFORT_LEVELS); codex has no max.
_EFFORT_LEVELS = ("low", "medium", "high", "xhigh", "max")


def _filed_task_defaults(
    model: str | None, effort: str | None, model_engine: str | None,
) -> tuple[dict | None, dict | None, str | None]:
    """``metadata.models`` / ``metadata.efforts`` for a filed task, or an
    error (mirrors the daemon's ``filed_task_defaults``): one engine slot,
    ``model_engine`` else the engine the model names."""
    model = (model or "").strip() or None
    effort = (effort or "").strip().lower() or None
    slot = None
    models = None
    if model:
        slot, error = _task_model_slot(model, model_engine)
        if error:
            return None, None, error
        models = {slot: model}
    elif model_engine:
        slot = {"claude-code": "claude", "claude": "claude", "codex": "codex"}.get(model_engine.strip())
        if slot is None:
            return None, None, f'model_engine must be "claude" or "codex", not {model_engine!r}'
    efforts = None
    if effort:
        if slot is None:
            return None, None, 'an effort without a model needs model_engine="claude" or "codex"'
        if effort not in _EFFORT_LEVELS or (slot == "codex" and effort == "max"):
            levels = _EFFORT_LEVELS[:4] if slot == "codex" else _EFFORT_LEVELS
            return None, None, f"effort '{effort}' is not a {slot} level; use one of {', '.join(levels)}"
        efforts = {slot: effort}
    return models, efforts, None


def _caller_filer_context(submitted_via: str = "mcp.submit_backtest") -> dict:
    """Best-effort, control-plane-derived provenance for an agent filer.

    Missing identity must never block submission. Local process facts supply
    the physical hostname and working directory; session/task/workflow facts
    come from the daemon so agents do not have to repeat their own identity.
    """
    context: dict = {
        "schema_version": 1,
        "submitted_via": submitted_via,
    }
    session_id = os.environ.get("CM_TUI_SESSION_ID", "").strip()
    if session_id:
        context["session_id"] = session_id
    try:
        hostname = socket.gethostname().strip()
        if hostname:
            context["machine"] = hostname
    except OSError:
        pass
    try:
        context["worktree_path"] = os.getcwd()
    except OSError:
        pass

    try:
        route = control_client.resolve_socket_route()
        if route.chose_daemon:
            pong = control_client.call("ping", {}, socket_path=route.path)
            for key in (
                "task_id",
                "workspace_id",
                "continuous_task_id",
                "workflow_run_id",
                "workflow_role",
                "managed_by_session_id",
                "worktree_path",
            ):
                value = pong.get(key)
                if isinstance(value, str) and value.strip():
                    context[key] = value
            context["agent"] = _normalize_filer_agent(pong.get("session_type"))
            return context
        caller = control_client.call("get_caller_task")
        for key in ("task_id", "workspace_id"):
            value = caller.get(key)
            if isinstance(value, str) and value.strip():
                context[key] = value
    except Exception:
        pass
    context.setdefault("agent", "unknown")
    return context


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
        machine_type: Override the default (n2-standard-4 unless CM_BACKTEST_MACHINE_TYPE is set).
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

    filer = _caller_filer_context("mcp.submit_backtest")
    parent_task_id = filer.get("task_id")

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
        # `source` is the planning provenance category (agent vs user), whose
        # DB vocabulary is still user|claude. The specific engine lives in
        # metadata.filer.agent.
        "source": "claude",
        "is_cloud": True,
        "kind": "backtest",
        "status": "backlog",
        "metadata": {"backtest": backtest_meta, "vm": vm, "filer": filer},
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
