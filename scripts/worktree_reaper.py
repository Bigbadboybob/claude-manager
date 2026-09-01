#!/usr/bin/env python3
"""Safely age out claude-manager worktrees.

The command is dry-run by default.  ``--apply`` removes only worktree
directories; it deliberately preserves every local and remote branch ref.

Policy:

* hard-protect live sessions/process references, pinned workspaces, and
  continuous tasks;
* require every associated planning task to be ``done`` or ``archived``;
* require seven days without task, session, transcript, checkout, reflog, or
  commit activity;
* preserve ordinary tracked/untracked changes in a local WIP commit before
  removing the checkout, while refusing conflicts, likely secrets, oversized
  untracked files, and meaningful ignored artifacts;
* preserve every branch ref.  A detached checkout gets a named rescue branch
  before its WIP commit, so removing the checkout cannot orphan its HEAD.

Every apply decision is re-evaluated immediately before removal, serialized
with a host-local flock, and appended to an audit ledger.
"""

from __future__ import annotations

import argparse
import dataclasses
import datetime as dt
import fcntl
import json
import os
import re
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import tomllib
import urllib.parse
import urllib.request
from collections import defaultdict
from pathlib import Path, PurePosixPath
from typing import Any, Iterable, Sequence


DAY = 24 * 60 * 60
TERMINAL_TASK_STATUSES = {"done", "archived"}
PROTECTED_TASK_STATUSES = {"running", "blocked"}
SAFE_IGNORED_DIRS = {
    ".cache",
    ".mypy_cache",
    ".nox",
    ".pytest_cache",
    ".ruff_cache",
    ".tox",
    ".venv",
    "__pycache__",
    "build",
    "dist",
    "node_modules",
    "target",
}
SAFE_IGNORED_FILES = {
    ".claude/scheduled_tasks.lock",
    ".claude/settings.local.json",
    ".coverage",
}
SENSITIVE_UNTRACKED_NAMES = {
    ".env",
    "credentials",
    "credentials.json",
    "id_ed25519",
    "id_rsa",
    "secrets",
    "secrets.json",
}
SENSITIVE_UNTRACKED_SUFFIXES = {".key", ".p12", ".pem", ".pfx"}
MAX_AUTO_COMMIT_FILE_BYTES = 50 * 1024 * 1024


@dataclasses.dataclass(frozen=True)
class CommandResult:
    returncode: int
    stdout: bytes
    stderr: bytes

    @property
    def text(self) -> str:
        return self.stdout.decode(errors="replace").strip()


@dataclasses.dataclass
class WorkspaceFacts:
    workspace_ids: set[str] = dataclasses.field(default_factory=set)
    task_ids: set[str] = dataclasses.field(default_factory=set)
    pinned: bool = False
    continuous: bool = False
    open_workspace: bool = False
    manifest_activity: list[float] = dataclasses.field(default_factory=list)


@dataclasses.dataclass(frozen=True)
class TaskFacts:
    task_id: str
    status: str
    kind: str
    branch: str | None
    repo_url: str | None
    updated_at: float | None

    @property
    def protects(self) -> bool:
        return self.kind == "continuous" or self.status in PROTECTED_TASK_STATUSES


@dataclasses.dataclass(frozen=True)
class GitFacts:
    main_repo: Path
    origin_url: str | None
    head: str
    branch: str | None
    preserving_refs: tuple[str, ...]
    dirty_entries: tuple[str, ...]
    untracked_entries: tuple[str, ...]
    unmerged_entries: tuple[str, ...]
    unsafe_untracked: tuple[str, ...]
    ignored_entries: tuple[str, ...]
    meaningful_ignored: tuple[str, ...]
    activity_at: tuple[float, ...]


@dataclasses.dataclass(frozen=True)
class LandedProof:
    landed: bool
    reason: str
    main_ref: str | None


@dataclasses.dataclass(frozen=True)
class Decision:
    path: Path
    action: str
    reason: str
    eligible: bool
    age_days: float | None = None
    threshold_days: int | None = None
    main_repo: Path | None = None
    branch: str | None = None
    head: str | None = None
    main_ref: str | None = None
    landed_reason: str | None = None
    workspace_ids: tuple[str, ...] = ()
    task_ids: tuple[str, ...] = ()
    details: tuple[str, ...] = ()

    def as_dict(self) -> dict[str, Any]:
        return {
            "path": str(self.path),
            "action": self.action,
            "reason": self.reason,
            "eligible": self.eligible,
            "age_days": round(self.age_days, 3) if self.age_days is not None else None,
            "threshold_days": self.threshold_days,
            "main_repo": str(self.main_repo) if self.main_repo else None,
            "branch": self.branch,
            "head": self.head,
            "main_ref": self.main_ref,
            "landed_reason": self.landed_reason,
            "workspace_ids": list(self.workspace_ids),
            "task_ids": list(self.task_ids),
            "details": list(self.details),
        }


@dataclasses.dataclass
class ScanContext:
    root: Path
    cm_home: Path
    config_path: Path
    manifest_paths: tuple[Path, ...]
    now: float
    retention_days: int
    workspaces: dict[Path, WorkspaceFacts]
    tasks: dict[str, TaskFacts]
    tasks_by_branch: dict[str, list[TaskFacts]]
    live_paths: set[Path]
    process_paths: dict[Path, set[int]]
    task_state_available: bool
    session_state_available: bool
    fetch: bool
    fetched_repos: dict[Path, bool] = dataclasses.field(default_factory=dict)
    landed_cache: dict[tuple[Path, str], LandedProof] = dataclasses.field(default_factory=dict)


def run(
    args: Sequence[str | os.PathLike[str]],
    *,
    cwd: Path | None = None,
    input_bytes: bytes | None = None,
    env: dict[str, str] | None = None,
    timeout: float = 60,
) -> CommandResult:
    command = [os.fspath(a) for a in args]
    try:
        proc = subprocess.run(
            command,
            cwd=cwd,
            input=input_bytes,
            capture_output=True,
            env=env,
            timeout=timeout,
            check=False,
        )
    except subprocess.TimeoutExpired as exc:
        stderr = exc.stderr or b""
        if isinstance(stderr, str):
            stderr = stderr.encode()
        stderr += f"command timed out after {timeout}s: {command!r}".encode()
        stdout = exc.stdout or b""
        if isinstance(stdout, str):
            stdout = stdout.encode()
        return CommandResult(124, stdout, stderr)
    return CommandResult(proc.returncode, proc.stdout, proc.stderr)


def git(
    repo: Path,
    *args: str,
    input_bytes: bytes | None = None,
    env: dict[str, str] | None = None,
    timeout: float = 20,
) -> CommandResult:
    return run(("git", "-C", repo, *args), input_bytes=input_bytes, env=env, timeout=timeout)


def parse_timestamp(value: Any) -> float | None:
    if isinstance(value, (int, float)):
        return float(value)
    if not isinstance(value, str) or not value:
        return None
    try:
        parsed = dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    if parsed.tzinfo is None:
        return None
    return parsed.timestamp()


def iter_manifest_workspaces(data: dict[str, Any]) -> Iterable[dict[str, Any]]:
    raw = data.get("workspaces") or {}
    if isinstance(raw, dict):
        yield from (value for value in raw.values() if isinstance(value, dict))
    elif isinstance(raw, list):
        yield from (value for value in raw if isinstance(value, dict))


def load_workspace_facts(manifest_paths: Iterable[Path]) -> dict[Path, WorkspaceFacts]:
    facts: dict[Path, WorkspaceFacts] = defaultdict(WorkspaceFacts)
    for manifest_path in manifest_paths:
        try:
            data = json.loads(manifest_path.read_text())
        except (OSError, json.JSONDecodeError):
            continue
        bindings = data.get("bindings") or {}
        by_workspace: dict[str, set[str]] = defaultdict(set)
        if isinstance(bindings, dict):
            for task_id, workspace_id in bindings.items():
                if isinstance(task_id, str) and isinstance(workspace_id, str):
                    by_workspace[workspace_id].add(task_id)
        for workspace in iter_manifest_workspaces(data):
            raw_path = workspace.get("worktree_path")
            if not isinstance(raw_path, str) or not raw_path:
                continue
            path = Path(raw_path).resolve(strict=False)
            current = facts[path]
            workspace_id = workspace.get("id")
            if isinstance(workspace_id, str):
                current.workspace_ids.add(workspace_id)
                current.task_ids.update(by_workspace.get(workspace_id, set()))
            current.pinned = current.pinned or bool(workspace.get("pinned"))
            current.open_workspace = current.open_workspace or not bool(workspace.get("is_closed"))
            for record in list(workspace.get("sessions") or []) + list(workspace.get("tombstones") or []):
                if not isinstance(record, dict):
                    continue
                task_id = record.get("task_id")
                if isinstance(task_id, str):
                    current.task_ids.add(task_id)
                current.continuous = current.continuous or bool(record.get("continuous_task_id"))
                for key in ("exited_at", "reported_done_at", "created_at", "last_input_at"):
                    stamp = parse_timestamp(record.get(key))
                    if stamp is not None:
                        current.manifest_activity.append(stamp)
    return dict(facts)


def load_daemon_config(config_path: Path) -> tuple[str, str]:
    config = tomllib.loads(config_path.read_text())
    api_url = str(config.get("api_url") or os.environ.get("CM_API_URL") or "").rstrip("/")
    token = str(config.get("api_token") or os.environ.get("CM_API_TOKEN") or "")
    if not api_url or not token:
        raise RuntimeError(f"planning API credentials missing from {config_path}")
    return api_url, token


def load_tasks(config_path: Path) -> tuple[dict[str, TaskFacts], bool, str | None]:
    try:
        api_url, token = load_daemon_config(config_path)
        query = urllib.parse.urlencode({"include_archived": "true"})
        request = urllib.request.Request(
            f"{api_url}/tasks?{query}",
            headers={"Authorization": f"Bearer {token}"},
        )
        with urllib.request.urlopen(request, timeout=15) as response:
            payload = json.load(response)
        if not isinstance(payload, list):
            raise RuntimeError("planning API returned a non-list task payload")
        tasks: dict[str, TaskFacts] = {}
        for item in payload:
            if not isinstance(item, dict) or not isinstance(item.get("id"), str):
                continue
            task_id = item["id"]
            tasks[task_id] = TaskFacts(
                task_id=task_id,
                status=str(item.get("status") or "unknown").lower(),
                kind=str(item.get("kind") or "oneshot").lower(),
                branch=item.get("wip_branch") if isinstance(item.get("wip_branch"), str) else None,
                repo_url=item.get("repo_url") if isinstance(item.get("repo_url"), str) else None,
                updated_at=parse_timestamp(item.get("updated_at")),
            )
        return tasks, True, None
    except Exception as exc:  # fail closed; the caller reports the exact cause
        return {}, False, f"{type(exc).__name__}: {exc}"


def recv_exact(sock: socket.socket, count: int) -> bytes:
    chunks: list[bytes] = []
    remaining = count
    while remaining:
        chunk = sock.recv(remaining)
        if not chunk:
            raise ConnectionError("daemon socket closed early")
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def load_live_paths(cm_home: Path) -> tuple[set[Path], bool, str | None]:
    try:
        token = (cm_home / "operator-token").read_text().strip()
        request = json.dumps(
            {
                "id": "worktree-reaper",
                "caller": {"token_id": token},
                "method": "list_sessions",
                "params": {},
            }
        ).encode()
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
            sock.settimeout(15)
            sock.connect(str(cm_home / "daemon.sock"))
            sock.sendall(struct.pack(">I", len(request)) + request)
            length = struct.unpack(">I", recv_exact(sock, 4))[0]
            response = json.loads(recv_exact(sock, length))
        if not response.get("ok"):
            raise RuntimeError(str(response.get("error") or "daemon list_sessions failed"))
        rows = response.get("result") or []
        paths = {
            Path(row["worktree_path"]).resolve(strict=False)
            for row in rows
            if isinstance(row, dict) and isinstance(row.get("worktree_path"), str)
        }
        return paths, True, None
    except Exception as exc:  # fail closed; the caller reports the exact cause
        return set(), False, f"{type(exc).__name__}: {exc}"


def process_references(root: Path) -> dict[Path, set[int]]:
    references: dict[Path, set[int]] = defaultdict(set)
    root_text = str(root.resolve(strict=False)) + os.sep
    proc = Path("/proc")
    for pid_path in proc.iterdir():
        if not pid_path.name.isdigit():
            continue
        pid = int(pid_path.name)
        links = [pid_path / "cwd"]
        try:
            links.extend((pid_path / "fd").iterdir())
        except OSError:
            pass
        for link in links:
            try:
                target = os.readlink(link)
            except OSError:
                continue
            if target.endswith(" (deleted)"):
                target = target[: -len(" (deleted)")]
            if not target.startswith(root_text):
                continue
            relative = target[len(root_text) :]
            first = relative.split(os.sep, 1)[0]
            if first:
                references[(root / first).resolve(strict=False)].add(pid)
    return dict(references)


def split_nul_paths(output: bytes, *, statuses: set[str] | None = None) -> tuple[str, ...]:
    entries: list[str] = []
    parts = output.split(b"\0")
    index = 0
    while index < len(parts):
        raw = parts[index]
        index += 1
        if not raw:
            continue
        text = raw.decode(errors="replace")
        status = text[:2]
        path = text[3:] if len(text) >= 4 else text
        if statuses is None or status in statuses:
            entries.append(path)
        if "R" in status or "C" in status:
            index += 1  # porcelain v1 -z emits the second rename/copy path separately
    return tuple(entries)


def is_safe_ignored(worktree: Path, main_repo: Path, path: str) -> bool:
    normalized = path.rstrip("/")
    if normalized in SAFE_IGNORED_FILES:
        return True
    candidate = worktree / normalized
    expected_target = main_repo / normalized
    if candidate.is_symlink():
        try:
            return candidate.resolve(strict=False) == expected_target.resolve(strict=False)
        except OSError:
            return False
    parts = PurePosixPath(normalized).parts
    if any(part in SAFE_IGNORED_DIRS for part in parts):
        return True
    return any(part.endswith(".egg-info") for part in parts)


def unsafe_untracked_paths(worktree: Path, paths: Iterable[str]) -> tuple[str, ...]:
    unsafe: list[str] = []
    for relative in paths:
        pure = PurePosixPath(relative)
        lowered_parts = tuple(part.lower() for part in pure.parts)
        name = lowered_parts[-1] if lowered_parts else ""
        sensitive_name = name in SENSITIVE_UNTRACKED_NAMES or name.startswith(".env.")
        sensitive_path = any(part in {".ssh", "secrets", "credentials"} for part in lowered_parts)
        sensitive_suffix = any(name.endswith(suffix) for suffix in SENSITIVE_UNTRACKED_SUFFIXES)
        oversized = False
        try:
            candidate = worktree / relative
            oversized = candidate.is_file() and not candidate.is_symlink() and candidate.stat().st_size > MAX_AUTO_COMMIT_FILE_BYTES
        except OSError:
            unsafe.append(f"{relative} (unreadable)")
            continue
        if sensitive_name or sensitive_path or sensitive_suffix:
            unsafe.append(f"{relative} (secret-like untracked path)")
        elif oversized:
            unsafe.append(f"{relative} (>50MiB untracked file)")
    return tuple(unsafe)


def worktree_entry_mtime(worktree: Path, relative: str) -> float | None:
    """Return the changed path's mtime, or its nearest existing parent's.

    Git reports deleted paths too. Their old inode is gone, but removing one
    updates the containing directory, which is still an activity signal.
    """
    pure = PurePosixPath(relative)
    if pure.is_absolute() or ".." in pure.parts:
        return None
    candidate = worktree.joinpath(*pure.parts)
    while candidate != worktree:
        try:
            return candidate.lstat().st_mtime
        except OSError:
            candidate = candidate.parent
    try:
        return worktree.stat().st_mtime
    except OSError:
        return None


def newest_claude_transcript_mtime(path: Path) -> float | None:
    encoded = str(path).replace("/", "-").replace(".", "-")
    transcript_dir = Path.home() / ".claude" / "projects" / encoded
    try:
        return max((entry.stat().st_mtime for entry in transcript_dir.glob("*.jsonl")), default=None)
    except OSError:
        return None


def parse_reflog_epoch(repo: Path) -> float | None:
    result = git(repo, "reflog", "-1", "--date=unix", "--format=%gd", "HEAD")
    if result.returncode:
        return None
    match = re.search(r"@\{(\d+)(?: [+-]\d+)?\}", result.text)
    return float(match.group(1)) if match else None


def inspect_git(path: Path, *, include_worktree_state: bool = True) -> tuple[GitFacts | None, str | None]:
    inside = git(path, "rev-parse", "--is-inside-work-tree")
    if inside.returncode or inside.text != "true":
        return None, "not a valid Git worktree"
    common = git(path, "rev-parse", "--path-format=absolute", "--git-common-dir")
    head_result = git(path, "rev-parse", "--verify", "HEAD^{commit}")
    if common.returncode or head_result.returncode:
        return None, "could not resolve Git common directory or HEAD"
    common_path = Path(common.text).resolve(strict=False)
    if common_path.name != ".git":
        return None, f"unexpected Git common directory {common_path}"
    main_repo = common_path.parent
    if main_repo.resolve(strict=False) == path.resolve(strict=False):
        return None, "path is the main checkout"

    branch_result = git(path, "symbolic-ref", "--quiet", "--short", "HEAD")
    branch = branch_result.text if branch_result.returncode == 0 and branch_result.text else None
    origin_result = git(main_repo, "remote", "get-url", "origin")
    origin_url = origin_result.text if origin_result.returncode == 0 and origin_result.text else None
    refs_result = git(
        path,
        "for-each-ref",
        "--format=%(refname)",
        "--contains=HEAD",
        "refs/heads",
        "refs/remotes",
    )
    preserving_refs = tuple(line for line in refs_result.text.splitlines() if line) if refs_result.returncode == 0 else ()

    dirty_entries: tuple[str, ...] = ()
    untracked_entries: tuple[str, ...] = ()
    unmerged_entries: tuple[str, ...] = ()
    unsafe_untracked: tuple[str, ...] = ()
    ignored_entries: tuple[str, ...] = ()
    meaningful_ignored: tuple[str, ...] = ()
    if include_worktree_state:
        dirty_result = git(path, "status", "--porcelain=v1", "-z", "--untracked-files=all")
        untracked_result = git(path, "ls-files", "--others", "--exclude-standard", "-z")
        unmerged_result = git(path, "diff", "--name-only", "--diff-filter=U", "-z")
        ignored_result = git(
            path,
            "status",
            "--porcelain=v1",
            "-z",
            "--ignored=matching",
            "--untracked-files=normal",
        )
        if dirty_result.returncode or ignored_result.returncode or untracked_result.returncode or unmerged_result.returncode:
            errors = b"\n".join(
                result.stderr
                for result in (dirty_result, ignored_result, untracked_result, unmerged_result)
                if result.returncode and result.stderr
            )
            suffix = errors.decode(errors="replace").strip()
            return None, f"git status failed: {suffix}" if suffix else "git status failed"
        dirty_entries = split_nul_paths(dirty_result.stdout)
        untracked_entries = tuple(
            raw.decode(errors="replace") for raw in untracked_result.stdout.split(b"\0") if raw
        )
        unmerged_entries = tuple(
            raw.decode(errors="replace") for raw in unmerged_result.stdout.split(b"\0") if raw
        )
        unsafe_untracked = unsafe_untracked_paths(path, untracked_entries)
        ignored_entries = split_nul_paths(ignored_result.stdout, statuses={"!!"})
        meaningful_ignored = tuple(
            ignored_path
            for ignored_path in ignored_entries
            if not is_safe_ignored(path, main_repo, ignored_path)
        )

    activity: list[float] = []
    for candidate in (path, path / ".git"):
        try:
            activity.append(candidate.stat().st_mtime)
        except OSError:
            pass
    commit_time = git(path, "show", "-s", "--format=%ct", "HEAD")
    if commit_time.returncode == 0 and commit_time.text.isdigit():
        activity.append(float(commit_time.text))
    reflog_time = parse_reflog_epoch(path)
    if reflog_time is not None:
        activity.append(reflog_time)
    transcript_time = newest_claude_transcript_mtime(path)
    if transcript_time is not None:
        activity.append(transcript_time)
    for changed_path in dirty_entries:
        changed_time = worktree_entry_mtime(path, changed_path)
        if changed_time is not None:
            activity.append(changed_time)

    return (
        GitFacts(
            main_repo=main_repo,
            origin_url=origin_url,
            head=head_result.text,
            branch=branch,
            preserving_refs=preserving_refs,
            dirty_entries=dirty_entries,
            untracked_entries=untracked_entries,
            unmerged_entries=unmerged_entries,
            unsafe_untracked=unsafe_untracked,
            ignored_entries=ignored_entries,
            meaningful_ignored=meaningful_ignored,
            activity_at=tuple(activity),
        ),
        None,
    )


def resolve_main_ref(main_repo: Path) -> str | None:
    symbolic = git(main_repo, "symbolic-ref", "--quiet", "refs/remotes/origin/HEAD")
    candidates = [symbolic.text] if symbolic.returncode == 0 and symbolic.text else []
    candidates.extend(("refs/remotes/origin/main", "refs/remotes/origin/master"))
    for candidate in candidates:
        if git(main_repo, "rev-parse", "--verify", "--quiet", f"{candidate}^{{commit}}").returncode == 0:
            return candidate
    return None


def fetch_main(ctx: ScanContext, main_repo: Path) -> bool:
    main_repo = main_repo.resolve(strict=False)
    if main_repo in ctx.fetched_repos:
        return ctx.fetched_repos[main_repo]
    if not ctx.fetch:
        ok = True
    else:
        ok = git(main_repo, "fetch", "--quiet", "--prune", "origin").returncode == 0
    ctx.fetched_repos[main_repo] = ok
    return ok


def aggregate_patch_present(main_repo: Path, main_ref: str, head: str) -> bool:
    base = git(main_repo, "merge-base", main_ref, head)
    if base.returncode or not base.text:
        return False
    patch = git(main_repo, "diff", "--binary", base.text, head)
    if patch.returncode:
        return False
    if not patch.stdout:
        return True
    fd, index_name = tempfile.mkstemp(prefix="cm-reaper-index-")
    os.close(fd)
    os.unlink(index_name)
    env = os.environ.copy()
    env["GIT_INDEX_FILE"] = index_name
    try:
        if git(main_repo, "read-tree", main_ref, env=env).returncode:
            return False
        reverse = git(
            main_repo,
            "apply",
            "--cached",
            "--reverse",
            "--check",
            "--whitespace=nowarn",
            "-",
            input_bytes=patch.stdout,
            env=env,
        )
        return reverse.returncode == 0
    finally:
        try:
            os.unlink(index_name)
        except FileNotFoundError:
            pass


def landed_proof(ctx: ScanContext, facts: GitFacts) -> LandedProof:
    cache_key = (facts.main_repo.resolve(strict=False), facts.head)
    if cache_key in ctx.landed_cache:
        return ctx.landed_cache[cache_key]
    if not fetch_main(ctx, facts.main_repo):
        proof = LandedProof(False, "origin fetch failed; fast-path disabled", None)
        ctx.landed_cache[cache_key] = proof
        return proof
    main_ref = resolve_main_ref(facts.main_repo)
    if main_ref is None:
        proof = LandedProof(False, "no origin main/master ref", None)
        ctx.landed_cache[cache_key] = proof
        return proof
    if git(facts.main_repo, "merge-base", "--is-ancestor", facts.head, main_ref).returncode == 0:
        proof = LandedProof(True, "head_is_ancestor", main_ref)
    elif git(facts.main_repo, "diff", "--quiet", facts.head, main_ref).returncode == 0:
        proof = LandedProof(True, "tree_matches_main", main_ref)
    else:
        cherry = git(facts.main_repo, "cherry", main_ref, facts.head)
        cherry_lines = tuple(line for line in cherry.text.splitlines() if line)
        if cherry.returncode == 0 and cherry_lines and all(line.startswith("-") for line in cherry_lines):
            proof = LandedProof(True, "all_commits_patch_equivalent", main_ref)
        elif aggregate_patch_present(facts.main_repo, main_ref, facts.head):
            proof = LandedProof(True, "aggregate_patch_present", main_ref)
        else:
            proof = LandedProof(False, "branch_changes_not_proven_in_main", main_ref)
    ctx.landed_cache[cache_key] = proof
    return proof


def canonical_repo_url(value: str | None) -> str | None:
    if not value:
        return None
    cleaned = value.strip().rstrip("/")
    if cleaned.endswith(".git"):
        cleaned = cleaned[:-4]
    scp_match = re.fullmatch(r"(?:[^@/]+@)?([^:/]+):(.+)", cleaned)
    if scp_match and "://" not in cleaned:
        return f"{scp_match.group(1).lower()}/{scp_match.group(2).lstrip('/')}"
    parsed = urllib.parse.urlparse(cleaned)
    if parsed.scheme and parsed.netloc:
        host = (parsed.hostname or parsed.netloc).lower()
        return f"{host}/{parsed.path.lstrip('/')}"
    try:
        return str(Path(cleaned).expanduser().resolve(strict=False))
    except OSError:
        return cleaned


def task_facts_for(git_facts: GitFacts, workspace: WorkspaceFacts, ctx: ScanContext) -> list[TaskFacts]:
    result: dict[str, TaskFacts] = {}
    for task_id in workspace.task_ids:
        task = ctx.tasks.get(task_id)
        if task is not None:
            result[task_id] = task
    if git_facts.branch:
        repo_identity = canonical_repo_url(git_facts.origin_url)
        for task in ctx.tasks_by_branch.get(git_facts.branch, []):
            if repo_identity is not None and canonical_repo_url(task.repo_url) == repo_identity:
                result[task.task_id] = task
    return list(result.values())


def decision(
    path: Path,
    ctx: ScanContext,
    *,
    refresh_processes: bool = False,
) -> Decision:
    resolved = path.resolve(strict=False)
    root = ctx.root.resolve(strict=False)
    try:
        resolved.relative_to(root)
    except ValueError:
        return Decision(path, "block", "path_escape", False, details=("outside configured worktree root",))
    if path.is_symlink() or not path.is_dir():
        return Decision(path, "block", "unsafe_path", False, details=("not a real directory",))

    workspace = ctx.workspaces.get(resolved, WorkspaceFacts())
    common = {
        "workspace_ids": tuple(sorted(workspace.workspace_ids)),
        "task_ids": tuple(sorted(workspace.task_ids)),
    }
    if workspace.pinned:
        return Decision(path, "protect", "pinned_workspace", False, **common)
    if workspace.continuous:
        return Decision(path, "protect", "continuous_workspace", False, **common)
    if resolved in ctx.live_paths:
        return Decision(path, "protect", "live_session", False, **common)
    process_paths = process_references(ctx.root) if refresh_processes else ctx.process_paths
    if resolved in process_paths:
        pids = ",".join(str(pid) for pid in sorted(process_paths[resolved])[:12])
        return Decision(path, "protect", "live_process_reference", False, details=(f"pids={pids}",), **common)
    if not ctx.task_state_available:
        return Decision(path, "protect", "task_state_unavailable", False, **common)
    if not ctx.session_state_available:
        return Decision(path, "protect", "session_state_unavailable", False, **common)

    manifest_tasks = [ctx.tasks[task_id] for task_id in workspace.task_ids if task_id in ctx.tasks]
    missing_task_ids = sorted(task_id for task_id in workspace.task_ids if task_id not in ctx.tasks)
    if missing_task_ids:
        return Decision(path, "protect", "associated_task_missing", False, details=tuple(missing_task_ids[:12]), **common)
    if any(task.kind == "continuous" for task in manifest_tasks):
        detail = tuple(
            f"{task.task_id}:{task.status}:{task.kind}" for task in manifest_tasks if task.kind == "continuous"
        )
        return Decision(path, "protect", "continuous_task", False, details=detail, **common)
    nonterminal_manifest_tasks = [task for task in manifest_tasks if task.status not in TERMINAL_TASK_STATUSES]
    if nonterminal_manifest_tasks:
        detail = tuple(f"{task.task_id}:{task.status}:{task.kind}" for task in nonterminal_manifest_tasks)
        return Decision(path, "protect", "task_not_terminal", False, details=detail, **common)
    known_activity = list(workspace.manifest_activity)
    known_activity.extend(task.updated_at for task in manifest_tasks if task.updated_at is not None)
    if manifest_tasks and known_activity:
        known_age_days = max(0.0, (ctx.now - max(known_activity)) / DAY)
        if known_age_days < ctx.retention_days:
            return Decision(
                path,
                "wait",
                "below_retention",
                False,
                age_days=known_age_days,
                threshold_days=ctx.retention_days,
                **common,
            )

    git_facts, error = inspect_git(path, include_worktree_state=False)
    if git_facts is None:
        return Decision(path, "block", "git_state_ambiguous", False, details=(error or "unknown Git error",), **common)
    common.update(
        {
            "main_repo": git_facts.main_repo,
            "branch": git_facts.branch,
            "head": git_facts.head,
        }
    )
    tasks = task_facts_for(git_facts, workspace, ctx)
    all_task_ids = set(workspace.task_ids)
    all_task_ids.update(task.task_id for task in tasks)
    common["task_ids"] = tuple(sorted(all_task_ids))
    if not tasks:
        return Decision(path, "protect", "unowned_worktree", False, **common)
    if any(task.kind == "continuous" for task in tasks):
        detail = tuple(f"{task.task_id}:{task.status}:{task.kind}" for task in tasks if task.kind == "continuous")
        return Decision(path, "protect", "continuous_task", False, details=detail, **common)
    nonterminal_tasks = [task for task in tasks if task.status not in TERMINAL_TASK_STATUSES]
    if nonterminal_tasks:
        detail = tuple(f"{task.task_id}:{task.status}:{task.kind}" for task in nonterminal_tasks)
        return Decision(path, "protect", "task_not_terminal", False, details=detail, **common)
    activity = list(git_facts.activity_at)
    activity.extend(workspace.manifest_activity)
    activity.extend(task.updated_at for task in tasks if task.updated_at is not None)
    if not activity:
        return Decision(path, "block", "activity_unknown", False, **common)
    last_activity = max(activity)
    age_days = max(0.0, (ctx.now - last_activity) / DAY)
    threshold = ctx.retention_days
    common.update(
        {
            "age_days": age_days,
            "threshold_days": threshold,
        }
    )
    if age_days < threshold:
        return Decision(path, "wait", "below_retention", False, **common)

    full_git_facts, full_error = inspect_git(path)
    if full_git_facts is None:
        return Decision(path, "block", "git_state_ambiguous", False, details=(full_error or "unknown Git error",), **common)
    if (
        full_git_facts.head != git_facts.head
        or full_git_facts.branch != git_facts.branch
        or full_git_facts.main_repo != git_facts.main_repo
    ):
        return Decision(path, "protect", "git_state_changed_during_scan", False, **common)
    git_facts = full_git_facts
    full_activity = list(git_facts.activity_at)
    full_activity.extend(workspace.manifest_activity)
    full_activity.extend(task.updated_at for task in tasks if task.updated_at is not None)
    full_age_days = max(0.0, (ctx.now - max(full_activity)) / DAY)
    common["age_days"] = full_age_days
    if full_age_days < threshold:
        return Decision(path, "wait", "below_retention", False, **common)
    if git_facts.meaningful_ignored:
        return Decision(path, "block", "meaningful_ignored", False, details=git_facts.meaningful_ignored[:12], **common)
    if git_facts.unmerged_entries:
        return Decision(path, "block", "unmerged_changes", False, details=git_facts.unmerged_entries[:12], **common)
    if git_facts.unsafe_untracked:
        return Decision(path, "block", "unsafe_untracked", False, details=git_facts.unsafe_untracked[:12], **common)

    proof = landed_proof(ctx, git_facts)
    commit_detail = ("will create WIP commit before removal",) if git_facts.dirty_entries else ()
    rescue_detail = ("will create rescue branch for detached HEAD",) if git_facts.branch is None else ()
    common.update({"main_ref": proof.main_ref, "landed_reason": proof.reason})
    if git_facts.dirty_entries:
        return Decision(path, "reap", "terminal_inactive_auto_commit", True, details=commit_detail + rescue_detail, **common)
    return Decision(path, "reap", "terminal_and_inactive", True, details=rescue_detail, **common)


def build_context(args: argparse.Namespace) -> tuple[ScanContext, list[str]]:
    root = args.root.expanduser().resolve(strict=False)
    cm_home = args.cm_home.expanduser().resolve(strict=False)
    manifests = tuple(args.manifest or [cm_home / "daemon-sessions.json", cm_home / "tui-sessions.json"])
    workspace_facts = load_workspace_facts(path for path in manifests if path.is_file())
    tasks, task_ok, task_error = load_tasks(args.config.expanduser())
    live_paths, sessions_ok, session_error = load_live_paths(cm_home)
    tasks_by_branch: dict[str, list[TaskFacts]] = defaultdict(list)
    for task in tasks.values():
        if task.branch:
            tasks_by_branch[task.branch].append(task)
    warnings = []
    if task_error:
        warnings.append(f"task state unavailable: {task_error}")
    if session_error:
        warnings.append(f"session state unavailable: {session_error}")
    return (
        ScanContext(
            root=root,
            cm_home=cm_home,
            config_path=args.config.expanduser().resolve(strict=False),
            manifest_paths=manifests,
            now=args.now if args.now is not None else dt.datetime.now(dt.timezone.utc).timestamp(),
            retention_days=args.retention_days,
            workspaces=workspace_facts,
            tasks=tasks,
            tasks_by_branch=dict(tasks_by_branch),
            live_paths=live_paths,
            process_paths=process_references(root),
            task_state_available=task_ok,
            session_state_available=sessions_ok,
            fetch=not args.no_fetch,
        ),
        warnings,
    )


def worktree_paths(root: Path) -> list[Path]:
    try:
        return sorted(
            (
                entry
                for entry in root.iterdir()
                if entry.is_dir()
                and not entry.is_symlink()
                and (entry / ".git").exists()
            ),
            key=lambda path: path.name,
        )
    except OSError:
        return []


def directory_size(path: Path) -> int | None:
    result = run(("du", "-sx", "--block-size=1", path), timeout=300)
    if result.returncode:
        return None
    try:
        return int(result.text.split()[0])
    except (IndexError, ValueError):
        return None


def human_bytes(value: int | None) -> str:
    if value is None:
        return "?"
    size = float(value)
    for unit in ("B", "KiB", "MiB", "GiB", "TiB"):
        if size < 1024 or unit == "TiB":
            return f"{size:.1f}{unit}"
        size /= 1024
    return f"{size:.1f}TiB"


def append_ledger(path: Path, record: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(record, sort_keys=True) + "\n")
        handle.flush()
        os.fsync(handle.fileno())


def refresh_dynamic_state(ctx: ScanContext) -> None:
    tasks, task_ok, _ = load_tasks(ctx.config_path)
    live_paths, sessions_ok, _ = load_live_paths(ctx.cm_home)
    ctx.task_state_available = task_ok
    ctx.session_state_available = sessions_ok
    if task_ok:
        ctx.tasks = tasks
        by_branch: dict[str, list[TaskFacts]] = defaultdict(list)
        for task in tasks.values():
            if task.branch:
                by_branch[task.branch].append(task)
        ctx.tasks_by_branch = dict(by_branch)
    if sessions_ok:
        ctx.live_paths = live_paths
    ctx.workspaces = load_workspace_facts(path for path in ctx.manifest_paths if path.is_file())


def create_rescue_branch(worktree: Path, head: str) -> tuple[bool, str | None, str | None]:
    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    base = f"cm-reaper/rescue-{stamp}-{head[:7]}"
    for suffix in range(100):
        branch = base if suffix == 0 else f"{base}-{suffix}"
        if git(worktree, "show-ref", "--verify", "--quiet", f"refs/heads/{branch}").returncode == 0:
            continue
        result = git(worktree, "switch", "-c", branch)
        if result.returncode == 0:
            return True, branch, None
        return False, None, result.stderr.decode(errors="replace").strip()
    return False, None, "could not allocate a unique rescue branch name"


def preserve_dirty_worktree(worktree: Path, task_ids: tuple[str, ...]) -> tuple[bool, str | None, str | None]:
    before, error = inspect_git(worktree)
    if before is None:
        return False, None, error or "Git pre-commit inspection failed"
    branch = before.branch
    if branch is None:
        ok, branch, branch_error = create_rescue_branch(worktree, before.head)
        if not ok:
            return False, None, f"rescue branch failed: {branch_error}"
    if not before.dirty_entries:
        return True, branch, None
    add = git(worktree, "add", "-A")
    if add.returncode:
        return False, branch, add.stderr.decode(errors="replace").strip() or "git add failed"
    staged = git(worktree, "diff", "--cached", "--quiet")
    if staged.returncode not in (0, 1):
        return False, branch, staged.stderr.decode(errors="replace").strip() or "staged diff check failed"
    if staged.returncode == 1:
        stamp = dt.datetime.now(dt.timezone.utc).isoformat()
        body = f"Worktree: {worktree}\nTasks: {','.join(task_ids) or 'none'}\nPreserved-at: {stamp}"
        commit = git(
            worktree,
            "-c",
            "user.name=CM Worktree Reaper",
            "-c",
            "user.email=worktree-reaper@localhost",
            "commit",
            "-m",
            "chore: preserve stale worktree before reaping",
            "-m",
            body,
        )
        if commit.returncode:
            git(worktree, "reset")
            return False, branch, commit.stderr.decode(errors="replace").strip() or "WIP commit failed"
    after, after_error = inspect_git(worktree)
    if after is None:
        return False, branch, after_error or "Git post-commit inspection failed"
    if after.dirty_entries:
        return False, branch, f"worktree still dirty after WIP commit: {', '.join(after.dirty_entries[:8])}"
    return True, branch, None


def reap_one(candidate: Decision, ctx: ScanContext, ledger: Path) -> tuple[bool, str, int | None]:
    refresh_dynamic_state(ctx)
    fresh = decision(candidate.path, ctx, refresh_processes=True)
    if not fresh.eligible:
        return False, f"pre-remove recheck changed to {fresh.reason}", None
    preserved, preserved_branch, preserve_error = preserve_dirty_worktree(candidate.path, fresh.task_ids)
    if not preserved:
        return False, f"could not preserve changes: {preserve_error}", None
    refresh_dynamic_state(ctx)
    post_preserve = decision(candidate.path, ctx, refresh_processes=True)
    expected_clock_reset = fresh.reason == "terminal_inactive_auto_commit" or fresh.branch is None
    if not post_preserve.eligible and not (expected_clock_reset and post_preserve.reason == "below_retention"):
        return False, f"post-commit recheck changed to {post_preserve.reason}", None
    size = directory_size(candidate.path)
    result = git(post_preserve.main_repo or candidate.path, "worktree", "remove", "--force", str(candidate.path))
    if result.returncode:
        return False, result.stderr.decode(errors="replace").strip() or "git worktree remove failed", None
    git(post_preserve.main_repo or candidate.path, "worktree", "prune")
    record = post_preserve.as_dict()
    record.update(
        {
            "reaped_at": dt.datetime.now(dt.timezone.utc).isoformat(),
            "bytes_before": size,
            "eligibility_reason": fresh.reason,
            "pre_preservation_head": fresh.head,
            "preservation_commit_created": fresh.reason == "terminal_inactive_auto_commit",
            "branch_preserved": True,
            "preserved_branch": preserved_branch or post_preserve.branch,
        }
    )
    append_ledger(ledger, record)
    return True, "removed; branch preserved", size


def print_human(
    decisions: list[Decision],
    warnings: list[str],
    apply_results: list[dict[str, Any]],
    *,
    summary_only: bool = False,
) -> None:
    for warning in warnings:
        print(f"WARNING {warning}", file=sys.stderr)
    if not summary_only:
        for item in decisions:
            age = f" age={item.age_days:.1f}d/{item.threshold_days}d" if item.age_days is not None else ""
            branch = f" branch={item.branch}" if item.branch else ""
            detail = f" details={'; '.join(item.details)}" if item.details else ""
            print(f"{item.action.upper():7} {item.reason:36} {item.path}{age}{branch}{detail}")
    counts: dict[str, int] = defaultdict(int)
    for item in decisions:
        counts[item.reason] += 1
    print("\nSummary:")
    for reason, count in sorted(counts.items()):
        print(f"  {reason}: {count}")
    print(f"  eligible: {sum(item.eligible for item in decisions)}")
    if apply_results:
        removed = [item for item in apply_results if item["removed"]]
        reclaimed = sum(item.get("bytes") or 0 for item in removed)
        print(f"  removed: {len(removed)} ({human_bytes(reclaimed)})")
        for item in apply_results:
            print(f"  {'REMOVED' if item['removed'] else 'SKIPPED'} {item['path']}: {item['message']}")


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--root", type=Path, default=Path("~/.cm/worktrees"), help="managed worktree root")
    result.add_argument("--cm-home", type=Path, default=Path("~/.cm"), help="claude-manager state directory")
    result.add_argument("--config", type=Path, default=Path("~/.cm/daemon.toml"), help="daemon config carrying planning API credentials")
    result.add_argument("--manifest", type=Path, action="append", help="manifest to include; repeatable")
    result.add_argument("--retention-days", type=int, default=7, help="retention after every associated task is terminal")
    result.add_argument("--max-removals", type=int, default=25, help="mass-event cap per apply run")
    result.add_argument("--apply", action="store_true", help="remove eligible worktrees; default is dry-run")
    result.add_argument("--json", action="store_true", help="emit JSON instead of the human report")
    result.add_argument("--summary-only", action="store_true", help="omit per-worktree dry-run rows")
    result.add_argument("--no-fetch", action="store_true", help="do not refresh origin refs; intended for tests/offline diagnosis")
    result.add_argument("--ledger", type=Path, default=Path("~/.cm/worktree-reaper.jsonl"), help="append-only apply audit ledger")
    result.add_argument("--lock", type=Path, default=Path("~/.cm/worktree-reaper.lock"), help="host-local apply lock")
    result.add_argument("--now", type=float, help=argparse.SUPPRESS)
    return result


def main(argv: Sequence[str] | None = None) -> int:
    signal.signal(signal.SIGPIPE, signal.SIG_DFL)
    args = parser().parse_args(argv)
    if args.retention_days < 1:
        parser().error("--retention-days must be positive")
    if args.max_removals < 1:
        parser().error("--max-removals must be positive")

    lock_path = args.lock.expanduser()
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    with lock_path.open("a+") as lock_handle:
        fcntl.flock(lock_handle, fcntl.LOCK_EX)
        ctx, warnings = build_context(args)
        decisions = [decision(path, ctx) for path in worktree_paths(ctx.root)]
        apply_results: list[dict[str, Any]] = []
        if args.apply:
            for candidate in (item for item in decisions if item.eligible):
                if len(apply_results) >= args.max_removals:
                    break
                removed, message, size = reap_one(candidate, ctx, args.ledger.expanduser())
                apply_results.append(
                    {"path": str(candidate.path), "removed": removed, "message": message, "bytes": size}
                )

    if args.json:
        print(
            json.dumps(
                {
                    "mode": "apply" if args.apply else "dry-run",
                    "policy": {"retention_days": args.retention_days, "terminal_statuses": sorted(TERMINAL_TASK_STATUSES)},
                    "warnings": warnings,
                    "decisions": [item.as_dict() for item in decisions],
                    "apply_results": apply_results,
                },
                indent=2,
                sort_keys=True,
            )
        )
    else:
        print_human(decisions, warnings, apply_results, summary_only=args.summary_only)
    if args.apply and warnings:
        return 2
    return 1 if args.apply and any(not item["removed"] for item in apply_results) else 0


if __name__ == "__main__":
    raise SystemExit(main())
