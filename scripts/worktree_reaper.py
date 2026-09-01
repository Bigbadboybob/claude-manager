#!/usr/bin/env python3
"""Safely age out claude-manager worktrees.

The command is dry-run by default.  ``--apply`` removes only worktree
directories; it deliberately preserves every local and remote branch ref.

Policy:

* hard-protect live sessions/process references, pinned workspaces, and
  continuous tasks;
* require every associated planning task to be ``done`` or ``archived``;
* require seven days without checkout-specific session, transcript, reflog, or
  changed-file activity; task status timestamps and directory mtimes do not
  count;
* apply the same seven-day branch-preserving policy to unowned checkouts;
* discover Claude-native worktrees and inherit terminal-task state through the
  exact subagent metadata link to their manager parent;
* preserve ordinary tracked/untracked changes in a local WIP commit and move
  meaningful ignored outputs into a mode-700 artifact vault;
* preserve every branch ref. A detached checkout gets a rescue branch, and a
  standalone clone under the managed root gets a verified Git bundle.

Every apply decision is re-evaluated immediately before removal, serialized
with a host-local flock, and appended to an audit ledger.
"""

from __future__ import annotations

import argparse
import dataclasses
import datetime as dt
import fcntl
import hashlib
import json
import os
import re
import shutil
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
from collections.abc import Iterable, Sequence
from pathlib import Path, PurePosixPath
from typing import Any

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
DISPOSABLE_IGNORED_PARTS = {
    ".scratch",
    "cache",
    "logs",
    "scratch",
}
DISPOSABLE_IGNORED_SUFFIXES = {".log"}
PUBLIC_ENV_TEMPLATE_NAMES = {".env.example", ".env.sample", ".env.template"}
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
    standalone_repo: bool
    preserving_refs: tuple[str, ...]
    dirty_entries: tuple[str, ...]
    untracked_entries: tuple[str, ...]
    unmerged_entries: tuple[str, ...]
    unsafe_untracked: tuple[str, ...]
    ignored_entries: tuple[str, ...]
    ignored_archive: tuple[str, ...]
    ignored_discard: tuple[str, ...]
    ignored_blocked: tuple[str, ...]
    ignored_archive_bytes: int
    ignored_discard_bytes: int
    activity_at: tuple[float, ...]


@dataclasses.dataclass(frozen=True)
class NativeParentLink:
    parent_worktree: Path
    metadata_paths: tuple[Path, ...]
    activity_at: tuple[float, ...]


@dataclasses.dataclass(frozen=True)
class ArtifactGcCandidate:
    path: Path
    age_days: float
    size_bytes: int
    artifact_kind: str

    def as_dict(self) -> dict[str, Any]:
        return {
            "path": str(self.path),
            "age_days": round(self.age_days, 3),
            "size_bytes": self.size_bytes,
            "artifact_kind": self.artifact_kind,
        }


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
    worktree_kind: str = "manager"
    parent_worktree: Path | None = None
    archive_paths: tuple[str, ...] = ()
    discard_paths: tuple[str, ...] = ()
    archive_bytes: int = 0
    discard_bytes: int = 0
    size_bytes: int | None = None
    standalone_repo: bool = False

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
            "worktree_kind": self.worktree_kind,
            "parent_worktree": str(self.parent_worktree) if self.parent_worktree else None,
            "archive_paths": list(self.archive_paths[:100]),
            "archive_path_count": len(self.archive_paths),
            "archive_paths_truncated": len(self.archive_paths) > 100,
            "discard_paths": list(self.discard_paths[:100]),
            "discard_path_count": len(self.discard_paths),
            "discard_paths_truncated": len(self.discard_paths) > 100,
            "archive_bytes": self.archive_bytes,
            "discard_bytes": self.discard_bytes,
            "size_bytes": self.size_bytes,
            "standalone_repo": self.standalone_repo,
        }


@dataclasses.dataclass
class ScanContext:
    root: Path
    cm_home: Path
    config_path: Path
    manifest_paths: tuple[Path, ...]
    now: float
    retention_days: int
    unowned_retention_days: int
    workspaces: dict[Path, WorkspaceFacts]
    tasks: dict[str, TaskFacts]
    tasks_by_branch: dict[str, list[TaskFacts]]
    live_paths: set[Path]
    process_paths: dict[Path, set[int]]
    task_state_available: bool
    session_state_available: bool
    fetch: bool
    artifact_root: Path = Path("~/.cm/worktree-artifacts")
    worktree_kind: str = "manager"
    native_parents: dict[Path, NativeParentLink] = dataclasses.field(default_factory=dict)
    external_activity: dict[Path, tuple[float, ...]] = dataclasses.field(default_factory=dict)
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
        parsed = dt.datetime.fromisoformat(value)
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
                # Closure/report timestamps are bookkeeping and are often bulk-updated.
                # Only timestamps tied to the checkout's actual use belong in its clock.
                for key in ("created_at", "last_input_at"):
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
            raise TypeError("planning API returned a non-list task payload")
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
    except Exception as exc:  # noqa: BLE001 - fail closed and report the exact cause
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
        paths = {Path(row["worktree_path"]).resolve(strict=False) for row in rows if isinstance(row, dict) and isinstance(row.get("worktree_path"), str)}
        return paths, True, None
    except Exception as exc:  # noqa: BLE001 - fail closed and report the exact cause
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
            target = target.removesuffix(" (deleted)")
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


def path_is_within(path: Path, parent: Path) -> bool:
    try:
        path.resolve(strict=False).relative_to(parent.resolve(strict=False))
        return True
    except (OSError, ValueError):
        return False


def secret_like_path(path: str) -> bool:
    pure = PurePosixPath(path.rstrip("/"))
    lowered_parts = tuple(part.lower() for part in pure.parts)
    name = lowered_parts[-1] if lowered_parts else ""
    if name in PUBLIC_ENV_TEMPLATE_NAMES:
        return False
    return (
        name in SENSITIVE_UNTRACKED_NAMES
        or name.startswith(".env.")
        or any(part in {".ssh", "secrets", "credentials"} for part in lowered_parts)
        or any(name.endswith(suffix) for suffix in SENSITIVE_UNTRACKED_SUFFIXES)
    )


def path_has_payload(path: Path) -> bool:
    """Return whether an ignored path owns anything beyond empty directories."""
    if path.is_symlink() or path.is_file():
        return True
    if not path.is_dir():
        return False
    try:
        for current, dirnames, filenames in os.walk(path, followlinks=False):
            if filenames:
                return True
            current_path = Path(current)
            if any((current_path / dirname).is_symlink() for dirname in dirnames):
                return True
    except OSError:
        return True
    return False


def newest_payload_mtime(path: Path) -> float | None:
    if path.is_symlink() or path.is_file():
        try:
            return path.lstat().st_mtime
        except OSError:
            return None
    newest: float | None = None
    try:
        for current, dirnames, filenames in os.walk(path, followlinks=False):
            current_path = Path(current)
            for name in filenames:
                try:
                    stamp = (current_path / name).lstat().st_mtime
                except OSError:
                    continue
                newest = stamp if newest is None else max(newest, stamp)
            for name in dirnames:
                candidate = current_path / name
                if not candidate.is_symlink():
                    continue
                try:
                    stamp = candidate.lstat().st_mtime
                except OSError:
                    continue
                newest = stamp if newest is None else max(newest, stamp)
    except OSError:
        return newest
    return newest


def ignored_path_bytes(path: Path) -> int:
    result = run(("du", "-sx", "--block-size=1", path), timeout=300)
    if result.returncode:
        return 0
    try:
        return int(result.text.split()[0])
    except (IndexError, ValueError):
        return 0


def is_disposable_ignored(path: str) -> bool:
    normalized = path.rstrip("/")
    if normalized in SAFE_IGNORED_FILES:
        return True
    parts = tuple(part.lower() for part in PurePosixPath(normalized).parts)
    if any(part in SAFE_IGNORED_DIRS for part in parts):
        return True
    if any(part in DISPOSABLE_IGNORED_PARTS for part in parts):
        return True
    name = parts[-1] if parts else ""
    return any(part.endswith(".egg-info") for part in parts) or any(name.endswith(suffix) for suffix in DISPOSABLE_IGNORED_SUFFIXES)


def files_identical(left: Path, right: Path) -> bool:
    try:
        if not left.is_file() or not right.is_file() or left.stat().st_size != right.stat().st_size:
            return False
        with left.open("rb") as left_handle, right.open("rb") as right_handle:
            while True:
                left_chunk = left_handle.read(1024 * 1024)
                right_chunk = right_handle.read(1024 * 1024)
                if left_chunk != right_chunk:
                    return False
                if not left_chunk:
                    return True
    except OSError:
        return False


def classify_ignored(
    worktree: Path,
    main_repo: Path,
    paths: Iterable[str],
) -> tuple[tuple[str, ...], tuple[str, ...], tuple[str, ...], int, int, tuple[float, ...]]:
    archive: list[str] = []
    discard: list[str] = []
    blocked: list[str] = []
    archive_bytes = 0
    discard_bytes = 0
    activity: list[float] = []
    for raw_path in paths:
        normalized = raw_path.rstrip("/")
        pure = PurePosixPath(normalized)
        if not normalized or pure.is_absolute() or ".." in pure.parts:
            blocked.append(f"{raw_path} (unsafe ignored path)")
            continue
        candidate = worktree.joinpath(*pure.parts)
        if candidate.is_symlink():
            try:
                target = candidate.resolve(strict=False)
            except OSError:
                blocked.append(f"{raw_path} (unreadable symlink)")
                continue
            if not path_is_within(target, worktree):
                discard.append(raw_path)
                continue
            blocked.append(f"{raw_path} (symlink targets this worktree)")
            continue
        if not candidate.exists():
            blocked.append(f"{raw_path} (ignored path disappeared or is unreadable)")
            continue
        if secret_like_path(normalized):
            canonical = main_repo.joinpath(*pure.parts)
            if files_identical(candidate, canonical):
                discard.append(raw_path)
                continue
            archive.append(raw_path)
            archive_bytes += ignored_path_bytes(candidate)
            stamp = newest_payload_mtime(candidate)
            if stamp is not None:
                activity.append(stamp)
            continue
        if not path_has_payload(candidate) or is_disposable_ignored(normalized):
            discard.append(raw_path)
            continue
        archive.append(raw_path)
        archive_bytes += ignored_path_bytes(candidate)
        stamp = newest_payload_mtime(candidate)
        if stamp is not None:
            activity.append(stamp)
    return (
        tuple(archive),
        tuple(discard),
        tuple(blocked),
        archive_bytes,
        discard_bytes,
        tuple(activity),
    )


def unsafe_untracked_paths(worktree: Path, paths: Iterable[str]) -> tuple[str, ...]:
    unsafe: list[str] = []
    for relative in paths:
        oversized = False
        try:
            candidate = worktree / relative
            oversized = candidate.is_file() and not candidate.is_symlink() and candidate.stat().st_size > MAX_AUTO_COMMIT_FILE_BYTES
        except OSError:
            unsafe.append(f"{relative} (unreadable)")
            continue
        if secret_like_path(relative):
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
        return max(
            (entry.stat().st_mtime for entry in transcript_dir.glob("*.jsonl")),
            default=None,
        )
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
    standalone_repo = main_repo.resolve(strict=False) == path.resolve(strict=False)

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
    ignored_archive: tuple[str, ...] = ()
    ignored_discard: tuple[str, ...] = ()
    ignored_blocked: tuple[str, ...] = ()
    ignored_archive_bytes = 0
    ignored_discard_bytes = 0
    ignored_activity: tuple[float, ...] = ()
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
                for result in (
                    dirty_result,
                    ignored_result,
                    untracked_result,
                    unmerged_result,
                )
                if result.returncode and result.stderr
            )
            suffix = errors.decode(errors="replace").strip()
            return (
                None,
                f"git status failed: {suffix}" if suffix else "git status failed",
            )
        dirty_entries = split_nul_paths(dirty_result.stdout)
        untracked_entries = tuple(raw.decode(errors="replace") for raw in untracked_result.stdout.split(b"\0") if raw)
        unmerged_entries = tuple(raw.decode(errors="replace") for raw in unmerged_result.stdout.split(b"\0") if raw)
        unsafe_untracked = unsafe_untracked_paths(path, untracked_entries)
        ignored_entries = split_nul_paths(ignored_result.stdout, statuses={"!!"})
        (
            ignored_archive,
            ignored_discard,
            ignored_blocked,
            ignored_archive_bytes,
            ignored_discard_bytes,
            ignored_activity,
        ) = classify_ignored(
            path,
            main_repo,
            ignored_entries,
        )

    activity: list[float] = []
    # Root/.git mtimes are changed by inventory jobs, and HEAD's commit time may
    # merely be inherited from trunk. Neither proves use of this checkout.
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
    activity.extend(ignored_activity)

    return (
        GitFacts(
            main_repo=main_repo,
            origin_url=origin_url,
            head=head_result.text,
            branch=branch,
            standalone_repo=standalone_repo,
            preserving_refs=preserving_refs,
            dirty_entries=dirty_entries,
            untracked_entries=untracked_entries,
            unmerged_entries=unmerged_entries,
            unsafe_untracked=unsafe_untracked,
            ignored_entries=ignored_entries,
            ignored_archive=ignored_archive,
            ignored_discard=ignored_discard,
            ignored_blocked=ignored_blocked,
            ignored_archive_bytes=ignored_archive_bytes,
            ignored_discard_bytes=ignored_discard_bytes,
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
    cleaned = cleaned.removesuffix(".git")
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


def encode_claude_project_path(path: Path) -> str:
    return str(path.resolve(strict=False)).replace("/", "-").replace(".", "-")


def load_native_parent_links(
    claude_projects: Path,
    parent_candidates: Iterable[Path],
) -> tuple[dict[Path, NativeParentLink], set[Path], list[str]]:
    encoded_candidates: dict[str, list[Path]] = defaultdict(list)
    for candidate in parent_candidates:
        resolved = candidate.resolve(strict=False)
        encoded_candidates[encode_claude_project_path(resolved)].append(resolved)
    links_by_child: dict[Path, list[tuple[Path, Path, tuple[float, ...]]]] = defaultdict(list)
    native_roots: set[Path] = set()
    warnings: list[str] = []
    try:
        metadata_paths = claude_projects.glob("*/*/subagents/*.meta.json")
        for metadata_path in metadata_paths:
            try:
                payload = json.loads(metadata_path.read_text())
            except (OSError, json.JSONDecodeError):
                continue
            raw_child = payload.get("worktreePath")
            if not payload.get("spawnedWithWorktree") or not isinstance(raw_child, str) or not raw_child:
                continue
            child = Path(raw_child).expanduser().resolve(strict=False)
            native_roots.add(child.parent)
            project_name = metadata_path.relative_to(claude_projects).parts[0]
            parents = encoded_candidates.get(project_name, [])
            if len(parents) != 1:
                if len(parents) > 1:
                    warnings.append(f"ambiguous Claude project encoding {project_name}")
                continue
            activity: list[float] = []
            transcript = metadata_path.with_name(metadata_path.name.removesuffix(".meta.json") + ".jsonl")
            for evidence in (metadata_path, transcript):
                try:
                    activity.append(evidence.stat().st_mtime)
                except OSError:
                    pass
            links_by_child[child].append((parents[0], metadata_path, tuple(activity)))
    except OSError as exc:
        warnings.append(f"could not scan Claude subagent metadata: {exc}")

    links: dict[Path, NativeParentLink] = {}
    for child, candidates in links_by_child.items():
        candidates.sort(key=lambda item: max(item[2], default=0.0), reverse=True)
        parent = candidates[0][0]
        matching = [candidate for candidate in candidates if candidate[0] == parent]
        links[child] = NativeParentLink(
            parent_worktree=parent,
            metadata_paths=tuple(candidate[1] for candidate in matching),
            activity_at=tuple(stamp for candidate in matching for stamp in candidate[2]),
        )
    return links, native_roots, warnings


def workspace_facts_for_native(
    manager_workspaces: dict[Path, WorkspaceFacts],
    links: dict[Path, NativeParentLink],
) -> dict[Path, WorkspaceFacts]:
    result = dict(manager_workspaces)
    for child, link in links.items():
        parent = manager_workspaces.get(link.parent_worktree, WorkspaceFacts())
        result[child] = WorkspaceFacts(
            workspace_ids=set(parent.workspace_ids),
            task_ids=set(parent.task_ids),
            pinned=parent.pinned,
            continuous=parent.continuous,
            open_workspace=parent.open_workspace,
            manifest_activity=list(link.activity_at),
        )
    return result


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
        return Decision(
            path,
            "block",
            "path_escape",
            False,
            details=("outside configured worktree root",),
        )
    if path.is_symlink() or not path.is_dir():
        return Decision(path, "block", "unsafe_path", False, details=("not a real directory",))

    workspace = ctx.workspaces.get(resolved, WorkspaceFacts())
    parent_link = ctx.native_parents.get(resolved)
    common: dict[str, Any] = {
        "workspace_ids": tuple(sorted(workspace.workspace_ids)),
        "task_ids": tuple(sorted(workspace.task_ids)),
        "worktree_kind": ctx.worktree_kind,
        "parent_worktree": parent_link.parent_worktree if parent_link else None,
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
        return Decision(
            path,
            "protect",
            "live_process_reference",
            False,
            details=(f"pids={pids}",),
            **common,
        )
    if not ctx.task_state_available:
        return Decision(path, "protect", "task_state_unavailable", False, **common)
    if not ctx.session_state_available:
        return Decision(path, "protect", "session_state_unavailable", False, **common)

    manifest_tasks = [ctx.tasks[task_id] for task_id in workspace.task_ids if task_id in ctx.tasks]
    # A deleted planning row must not pin a checkout forever. Keep its id in
    # provenance and let the branch-preserving unowned policy decide normally.
    if any(task.kind == "continuous" for task in manifest_tasks):
        detail = tuple(f"{task.task_id}:{task.status}:{task.kind}" for task in manifest_tasks if task.kind == "continuous")
        return Decision(path, "protect", "continuous_task", False, details=detail, **common)
    nonterminal_manifest_tasks = [task for task in manifest_tasks if task.status not in TERMINAL_TASK_STATUSES]
    if nonterminal_manifest_tasks:
        detail = tuple(f"{task.task_id}:{task.status}:{task.kind}" for task in nonterminal_manifest_tasks)
        return Decision(path, "protect", "task_not_terminal", False, details=detail, **common)
    git_facts, error = inspect_git(path, include_worktree_state=False)
    if git_facts is None:
        return Decision(
            path,
            "block",
            "git_state_ambiguous",
            False,
            details=(error or "unknown Git error",),
            **common,
        )
    common.update(
        {
            "main_repo": git_facts.main_repo,
            "branch": git_facts.branch,
            "head": git_facts.head,
            "standalone_repo": git_facts.standalone_repo,
        }
    )
    tasks = task_facts_for(git_facts, workspace, ctx)
    all_task_ids = set(workspace.task_ids)
    all_task_ids.update(task.task_id for task in tasks)
    common["task_ids"] = tuple(sorted(all_task_ids))
    unowned = not tasks
    if not unowned:
        if any(task.kind == "continuous" for task in tasks):
            detail = tuple(f"{task.task_id}:{task.status}:{task.kind}" for task in tasks if task.kind == "continuous")
            return Decision(path, "protect", "continuous_task", False, details=detail, **common)
        nonterminal_tasks = [task for task in tasks if task.status not in TERMINAL_TASK_STATUSES]
        if nonterminal_tasks:
            detail = tuple(f"{task.task_id}:{task.status}:{task.kind}" for task in nonterminal_tasks)
            return Decision(path, "protect", "task_not_terminal", False, details=detail, **common)
    activity = list(git_facts.activity_at)
    activity.extend(workspace.manifest_activity)
    activity.extend(ctx.external_activity.get(resolved, ()))
    if not activity:
        return Decision(path, "block", "activity_unknown", False, **common)
    last_activity = max(activity)
    age_days = max(0.0, (ctx.now - last_activity) / DAY)
    threshold = ctx.unowned_retention_days if unowned else ctx.retention_days
    below_retention_reason = "unowned_below_retention" if unowned else "below_retention"
    common.update(
        {
            "age_days": age_days,
            "threshold_days": threshold,
        }
    )
    if age_days < threshold:
        return Decision(path, "wait", below_retention_reason, False, **common)

    full_git_facts, full_error = inspect_git(path)
    if full_git_facts is None:
        return Decision(
            path,
            "block",
            "git_state_ambiguous",
            False,
            details=(full_error or "unknown Git error",),
            **common,
        )
    if (
        full_git_facts.head != git_facts.head
        or full_git_facts.branch != git_facts.branch
        or full_git_facts.main_repo != git_facts.main_repo
        or full_git_facts.standalone_repo != git_facts.standalone_repo
    ):
        return Decision(path, "protect", "git_state_changed_during_scan", False, **common)
    git_facts = full_git_facts
    full_activity = list(git_facts.activity_at)
    full_activity.extend(workspace.manifest_activity)
    full_activity.extend(ctx.external_activity.get(resolved, ()))
    full_age_days = max(0.0, (ctx.now - max(full_activity)) / DAY)
    common["age_days"] = full_age_days
    if full_age_days < threshold:
        return Decision(path, "wait", below_retention_reason, False, **common)
    common.update(
        {
            "archive_paths": git_facts.ignored_archive,
            "discard_paths": git_facts.ignored_discard,
            "archive_bytes": git_facts.ignored_archive_bytes,
            "discard_bytes": git_facts.ignored_discard_bytes,
        }
    )
    if git_facts.ignored_blocked:
        return Decision(
            path,
            "block",
            "unsafe_ignored",
            False,
            details=git_facts.ignored_blocked[:12],
            **common,
        )
    if git_facts.unmerged_entries:
        return Decision(
            path,
            "block",
            "unmerged_changes",
            False,
            details=git_facts.unmerged_entries[:12],
            **common,
        )
    if git_facts.unsafe_untracked:
        return Decision(
            path,
            "block",
            "unsafe_untracked",
            False,
            details=git_facts.unsafe_untracked[:12],
            **common,
        )

    rescue_detail = ("will create rescue branch for detached HEAD",) if git_facts.branch is None else ()
    standalone_detail = ("will preserve standalone repository refs in a verified Git bundle",) if git_facts.standalone_repo else ()
    archive_detail = (f"will archive {len(git_facts.ignored_archive)} ignored path(s) ({human_bytes(git_facts.ignored_archive_bytes)})",) if git_facts.ignored_archive else ()
    discard_detail = (f"will discard {len(git_facts.ignored_discard)} regenerable/empty ignored path(s)",) if git_facts.ignored_discard else ()
    proof = landed_proof(ctx, git_facts)
    commit_detail = ("will create WIP commit before removal",) if git_facts.dirty_entries else ()
    common.update({"main_ref": proof.main_ref, "landed_reason": proof.reason})
    if unowned:
        reason = "unowned_inactive_auto_commit" if git_facts.dirty_entries else "unowned_inactive"
        return Decision(
            path,
            "reap",
            reason,
            True,
            details=commit_detail + archive_detail + discard_detail + rescue_detail + standalone_detail,
            **common,
        )
    if git_facts.dirty_entries:
        return Decision(
            path,
            "reap",
            "terminal_inactive_auto_commit",
            True,
            details=commit_detail + archive_detail + discard_detail + rescue_detail + standalone_detail,
            **common,
        )
    return Decision(
        path,
        "reap",
        "terminal_and_inactive",
        True,
        details=archive_detail + discard_detail + rescue_detail + standalone_detail,
        **common,
    )


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
            now=args.now if args.now is not None else dt.datetime.now(dt.UTC).timestamp(),
            retention_days=args.retention_days,
            unowned_retention_days=args.unowned_retention_days,
            workspaces=workspace_facts,
            tasks=tasks,
            tasks_by_branch=dict(tasks_by_branch),
            live_paths=live_paths,
            process_paths=process_references(root),
            task_state_available=task_ok,
            session_state_available=sessions_ok,
            fetch=not args.no_fetch,
            artifact_root=args.artifact_root.expanduser().resolve(strict=False),
        ),
        warnings,
    )


def build_scan_contexts(
    args: argparse.Namespace,
) -> tuple[list[ScanContext], list[str]]:
    manager, warnings = build_context(args)
    manager_paths = worktree_paths(manager.root)
    parent_candidates = set(manager_paths) | set(manager.workspaces)
    native_links, discovered_roots, native_warnings = load_native_parent_links(
        args.claude_projects.expanduser().resolve(strict=False),
        parent_candidates,
    )
    warnings.extend(native_warnings)
    requested_roots = {path.expanduser().resolve(strict=False) for path in (args.native_root or [])}
    native_roots = requested_roots | discovered_roots
    contexts = [manager]
    native_workspaces = workspace_facts_for_native(manager.workspaces, native_links)
    for native_root in sorted(native_roots):
        if native_root == manager.root or not native_root.is_dir():
            continue
        contexts.append(
            dataclasses.replace(
                manager,
                root=native_root,
                workspaces=dict(native_workspaces),
                process_paths=process_references(native_root),
                worktree_kind="native",
                native_parents=dict(native_links),
                external_activity={child: link.activity_at for child, link in native_links.items()},
            )
        )
    return contexts, warnings


def worktree_paths(root: Path) -> list[Path]:
    try:
        return sorted(
            (entry for entry in root.iterdir() if entry.is_dir() and not entry.is_symlink() and (entry / ".git").exists()),
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


def directory_sizes(paths: Iterable[Path]) -> dict[Path, int]:
    requested = [path.resolve(strict=False) for path in paths]
    if not requested:
        return {}
    result = run(("du", "-sxl", "--block-size=1", "--", *requested), timeout=1200)
    if result.returncode:
        return {}
    sizes: dict[Path, int] = {}
    for line in result.text.splitlines():
        try:
            raw_size, raw_path = line.split("\t", 1)
            sizes[Path(raw_path).resolve(strict=False)] = int(raw_size)
        except (ValueError, OSError):
            continue
    return sizes


def artifact_gc_candidates(
    artifact_root: Path,
    now: float,
    retention_days: int,
) -> list[ArtifactGcCandidate]:
    root = artifact_root.expanduser().resolve(strict=False)
    if not root.is_dir():
        return []
    raw_candidates: list[tuple[Path, float, str]] = []
    for manifest in root.glob("*/*/*/manifest.json"):
        archive = manifest.parent
        if (archive / ".keep").exists():
            continue
        stamp: float | None = None
        try:
            payload = json.loads(manifest.read_text())
            stamp = parse_timestamp(payload.get("archived_at"))
        except (OSError, json.JSONDecodeError):
            pass
        if stamp is None:
            try:
                stamp = archive.stat().st_mtime
            except OSError:
                continue
        raw_candidates.append((archive, stamp, "ignored_outputs"))
    for bundle in root.glob("standalone-repositories/*/*.bundle"):
        if (bundle.parent / f"{bundle.name}.keep").exists():
            continue
        try:
            stamp = bundle.stat().st_mtime
        except OSError:
            continue
        raw_candidates.append((bundle, stamp, "standalone_bundle"))
    result: list[ArtifactGcCandidate] = []
    for path, stamp, artifact_kind in raw_candidates:
        age_days = max(0.0, (now - stamp) / DAY)
        if age_days < retention_days:
            continue
        size = directory_size(path) or 0
        result.append(ArtifactGcCandidate(path, age_days, size, artifact_kind))
    return sorted(result, key=lambda item: item.size_bytes, reverse=True)


def remove_artifact_candidate(
    candidate: ArtifactGcCandidate,
    artifact_root: Path,
    ledger: Path,
) -> tuple[bool, str]:
    root = artifact_root.expanduser().resolve(strict=False)
    path = candidate.path.resolve(strict=False)
    try:
        path.relative_to(root)
    except ValueError:
        return False, "artifact path escaped configured root"
    if path == root or path.is_symlink():
        return False, "refusing broad or symlink artifact target"
    try:
        if path.is_dir():
            shutil.rmtree(path)
        elif path.is_file():
            path.unlink()
        else:
            return False, "artifact target disappeared"
    except OSError as exc:
        return False, str(exc)
    append_ledger(
        ledger,
        {
            "event": "artifact_gc",
            "path": str(path),
            "artifact_kind": candidate.artifact_kind,
            "age_days": candidate.age_days,
            "bytes_before": candidate.size_bytes,
            "reaped_at": dt.datetime.now(dt.UTC).isoformat(),
        },
    )
    return True, "expired artifact removed"


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


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def artifact_inventory(source: Path, relative: str) -> list[dict[str, Any]]:
    records: list[dict[str, Any]] = []
    if source.is_symlink():
        records.append({"path": relative, "type": "symlink", "target": os.readlink(source)})
        return records
    if source.is_file():
        stat = source.stat()
        records.append(
            {
                "path": relative,
                "type": "file",
                "bytes": stat.st_size,
                "sha256": sha256_file(source),
                "mtime": dt.datetime.fromtimestamp(stat.st_mtime, dt.UTC).isoformat(),
            }
        )
        return records
    for current, dirnames, filenames in os.walk(source, followlinks=False):
        current_path = Path(current)
        for name in sorted(dirnames):
            candidate = current_path / name
            if not candidate.is_symlink():
                continue
            child_relative = candidate.relative_to(source.parent).as_posix()
            records.append(
                {
                    "path": child_relative,
                    "type": "symlink",
                    "target": os.readlink(candidate),
                }
            )
        for name in sorted(filenames):
            candidate = current_path / name
            child_relative = candidate.relative_to(source.parent).as_posix()
            if candidate.is_symlink():
                records.append(
                    {
                        "path": child_relative,
                        "type": "symlink",
                        "target": os.readlink(candidate),
                    }
                )
                continue
            stat = candidate.stat()
            records.append(
                {
                    "path": child_relative,
                    "type": "file",
                    "bytes": stat.st_size,
                    "sha256": sha256_file(candidate),
                    "mtime": dt.datetime.fromtimestamp(stat.st_mtime, dt.UTC).isoformat(),
                }
            )
    return records


def archive_ignored_paths(
    candidate: Decision,
    artifact_root: Path,
) -> tuple[bool, Path | None, str | None]:
    if not candidate.archive_paths:
        return True, None, None
    if candidate.main_repo is None or candidate.head is None:
        return False, None, "archive plan lacks repository or HEAD provenance"
    stamp = dt.datetime.now(dt.UTC).strftime("%Y%m%dT%H%M%SZ")
    destination_parent = artifact_root.expanduser() / candidate.main_repo.name / candidate.path.name
    destination_parent.mkdir(parents=True, mode=0o700, exist_ok=True)
    os.chmod(artifact_root.expanduser(), 0o700)
    os.chmod(destination_parent, 0o700)
    final = destination_parent / f"{stamp}-{candidate.head[:7]}"
    suffix = 0
    while final.exists():
        suffix += 1
        final = destination_parent / f"{stamp}-{candidate.head[:7]}-{suffix}"
    staging = destination_parent / f".{final.name}.partial"
    staging.mkdir(mode=0o700)
    inventory: list[dict[str, Any]] = []
    moved: list[str] = []
    try:
        destination_device = staging.stat().st_dev
        for raw_relative in candidate.archive_paths:
            relative = PurePosixPath(raw_relative.rstrip("/"))
            if relative.is_absolute() or ".." in relative.parts:
                raise RuntimeError(f"unsafe archive path {raw_relative}")
            source = candidate.path.joinpath(*relative.parts)
            if not source.exists() and not source.is_symlink():
                raise RuntimeError(f"archive source disappeared: {raw_relative}")
            if source.lstat().st_dev != destination_device:
                raise RuntimeError(f"artifact vault is on another filesystem: {raw_relative}")
            inventory.extend(artifact_inventory(source, relative.as_posix()))
            destination = staging / "files" / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            os.replace(source, destination)
            moved.append(raw_relative)
        manifest = {
            "archived_at": dt.datetime.now(dt.UTC).isoformat(),
            "source_worktree": str(candidate.path),
            "worktree_kind": candidate.worktree_kind,
            "parent_worktree": str(candidate.parent_worktree) if candidate.parent_worktree else None,
            "main_repo": str(candidate.main_repo),
            "branch": candidate.branch,
            "head": candidate.head,
            "task_ids": list(candidate.task_ids),
            "paths": list(candidate.archive_paths),
            "bytes": candidate.archive_bytes,
            "inventory": inventory,
        }
        manifest_path = staging / "manifest.json"
        with manifest_path.open("w", encoding="utf-8") as handle:
            json.dump(manifest, handle, indent=2, sort_keys=True)
            handle.write("\n")
            handle.flush()
            os.fsync(handle.fileno())
        os.chmod(manifest_path, 0o600)
        os.replace(staging, final)
        return True, final, None
    except Exception as exc:  # noqa: BLE001 - leave a recoverable partial archive for any failure
        recovery = staging if staging.exists() else final
        return False, recovery, f"{type(exc).__name__}: {exc}; moved={moved}"


def archive_standalone_bundle(
    candidate: Decision,
    artifact_root: Path,
) -> tuple[bool, Path | None, str | None]:
    if not candidate.standalone_repo:
        return True, None, None
    if candidate.head is None:
        return False, None, "standalone repository plan lacks HEAD provenance"
    destination = artifact_root.expanduser() / "standalone-repositories" / candidate.path.name
    destination.mkdir(parents=True, mode=0o700, exist_ok=True)
    os.chmod(artifact_root.expanduser(), 0o700)
    os.chmod(destination, 0o700)
    stamp = dt.datetime.now(dt.UTC).strftime("%Y%m%dT%H%M%SZ")
    final = destination / f"{stamp}-{candidate.head[:7]}.bundle"
    suffix = 0
    while final.exists():
        suffix += 1
        final = destination / f"{stamp}-{candidate.head[:7]}-{suffix}.bundle"
    staging = destination / f".{final.name}.partial"
    created = git(candidate.path, "bundle", "create", str(staging), "--all", timeout=1200)
    if created.returncode:
        return (
            False,
            staging if staging.exists() else None,
            created.stderr.decode(errors="replace").strip() or "git bundle create failed",
        )
    verified = git(candidate.path, "bundle", "verify", str(staging), timeout=1200)
    if verified.returncode:
        return (
            False,
            staging,
            verified.stderr.decode(errors="replace").strip() or "git bundle verify failed",
        )
    with staging.open("rb") as handle:
        os.fsync(handle.fileno())
    os.chmod(staging, 0o600)
    os.replace(staging, final)
    return True, final, None


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
    manager_workspaces = load_workspace_facts(path for path in ctx.manifest_paths if path.is_file())
    ctx.workspaces = workspace_facts_for_native(manager_workspaces, ctx.native_parents) if ctx.worktree_kind == "native" else manager_workspaces


def create_rescue_branch(worktree: Path, head: str) -> tuple[bool, str | None, str | None]:
    stamp = dt.datetime.now(dt.UTC).strftime("%Y%m%dT%H%M%SZ")
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
        return (
            False,
            branch,
            add.stderr.decode(errors="replace").strip() or "git add failed",
        )
    staged = git(worktree, "diff", "--cached", "--quiet")
    if staged.returncode not in (0, 1):
        return (
            False,
            branch,
            staged.stderr.decode(errors="replace").strip() or "staged diff check failed",
        )
    if staged.returncode == 1:
        stamp = dt.datetime.now(dt.UTC).isoformat()
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
            return (
                False,
                branch,
                commit.stderr.decode(errors="replace").strip() or "WIP commit failed",
            )
    after, after_error = inspect_git(worktree)
    if after is None:
        return False, branch, after_error or "Git post-commit inspection failed"
    if after.dirty_entries:
        return (
            False,
            branch,
            f"worktree still dirty after WIP commit: {', '.join(after.dirty_entries[:8])}",
        )
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
    expected_clock_reset = fresh.reason in {"terminal_inactive_auto_commit", "unowned_inactive_auto_commit"} or fresh.branch is None
    clock_reset_reasons = {"below_retention", "unowned_below_retention"}
    if not post_preserve.eligible and not (expected_clock_reset and post_preserve.reason in clock_reset_reasons):
        return False, f"post-commit recheck changed to {post_preserve.reason}", None
    archived, artifact_path, archive_error = archive_ignored_paths(fresh, ctx.artifact_root)
    if not archived:
        return (
            False,
            f"could not archive ignored artifacts: {archive_error}; recovery={artifact_path}",
            None,
        )
    refresh_dynamic_state(ctx)
    post_archive = decision(candidate.path, ctx, refresh_processes=True)
    if not post_archive.eligible and not (expected_clock_reset and post_archive.reason in clock_reset_reasons):
        return (
            False,
            f"post-archive recheck changed to {post_archive.reason}; artifacts={artifact_path}",
            None,
        )
    bundled, bundle_path, bundle_error = archive_standalone_bundle(fresh, ctx.artifact_root)
    if not bundled:
        return (
            False,
            f"could not preserve standalone repository: {bundle_error}; recovery={bundle_path}",
            None,
        )
    size = candidate.size_bytes if candidate.size_bytes is not None else directory_size(candidate.path)
    if fresh.standalone_repo:
        try:
            shutil.rmtree(candidate.path)
        except OSError as exc:
            return (
                False,
                f"standalone checkout removal failed: {exc}; bundle={bundle_path}; artifacts={artifact_path}",
                None,
            )
    else:
        result = git(
            post_archive.main_repo or candidate.path,
            "worktree",
            "remove",
            "--force",
            str(candidate.path),
        )
        if result.returncode:
            suffix = f"; artifacts={artifact_path}" if artifact_path else ""
            return (
                False,
                (result.stderr.decode(errors="replace").strip() or "git worktree remove failed") + suffix,
                None,
            )
        git(post_archive.main_repo or candidate.path, "worktree", "prune")
    record = fresh.as_dict()
    record.update(
        {
            "reaped_at": dt.datetime.now(dt.UTC).isoformat(),
            "bytes_before": size,
            "eligibility_reason": fresh.reason,
            "pre_preservation_head": fresh.head,
            "preservation_commit_created": fresh.reason in {"terminal_inactive_auto_commit", "unowned_inactive_auto_commit"},
            "branch_preserved": True,
            "preserved_branch": preserved_branch or post_archive.branch,
            "artifact_path": str(artifact_path) if artifact_path else None,
            "standalone_bundle": str(bundle_path) if bundle_path else None,
        }
    )
    append_ledger(ledger, record)
    artifact_suffix = f"; artifacts archived at {artifact_path}" if artifact_path else ""
    bundle_suffix = f"; standalone refs bundled at {bundle_path}" if bundle_path else ""
    return True, f"removed; branch preserved{artifact_suffix}{bundle_suffix}", size


def print_human(
    decisions: list[Decision],
    warnings: list[str],
    apply_results: list[dict[str, Any]],
    artifact_gc: list[ArtifactGcCandidate],
    artifact_gc_results: list[dict[str, Any]],
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
    eligible = [item for item in decisions if item.eligible]
    print("\nSummary:")
    for reason, count in sorted(counts.items()):
        print(f"  {reason}: {count}")
    print(f"  eligible: {len(eligible)} ({human_bytes(sum(item.size_bytes or 0 for item in eligible))})")
    print(f"  planned artifact archive: {human_bytes(sum(item.archive_bytes for item in eligible))}")
    print(f"  planned ignored discard: {sum(len(item.discard_paths) for item in eligible)} path(s)")
    print(f"  expired artifacts: {len(artifact_gc)} ({human_bytes(sum(item.size_bytes for item in artifact_gc))})")
    if apply_results:
        removed = [result for result in apply_results if result["removed"]]
        reclaimed = sum(result.get("bytes") or 0 for result in removed)
        print(f"  removed: {len(removed)} ({human_bytes(reclaimed)})")
        for result in apply_results:
            print(f"  {'REMOVED' if result['removed'] else 'SKIPPED'} {result['path']}: {result['message']}")
    if artifact_gc_results:
        for result in artifact_gc_results:
            print(f"  {'REMOVED' if result['removed'] else 'SKIPPED'} artifact {result['path']}: {result['message']}")


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument(
        "--root",
        type=Path,
        default=Path("~/.cm/worktrees"),
        help="managed worktree root",
    )
    result.add_argument(
        "--native-root",
        type=Path,
        action="append",
        help="Claude-native worktree root; repeatable (metadata-discovered roots are always included)",
    )
    result.add_argument(
        "--claude-projects",
        type=Path,
        default=Path("~/.claude/projects"),
        help="Claude transcript/agent metadata root used to link native children to manager parents",
    )
    result.add_argument(
        "--cm-home",
        type=Path,
        default=Path("~/.cm"),
        help="claude-manager state directory",
    )
    result.add_argument(
        "--config",
        type=Path,
        default=Path("~/.cm/daemon.toml"),
        help="daemon config carrying planning API credentials",
    )
    result.add_argument("--manifest", type=Path, action="append", help="manifest to include; repeatable")
    result.add_argument(
        "--retention-days",
        type=int,
        default=7,
        help="retention after every associated task is terminal",
    )
    result.add_argument(
        "--unowned-retention-days",
        type=int,
        default=7,
        help="retention for unowned worktrees; their branch and any safe WIP commit are preserved",
    )
    result.add_argument("--max-removals", type=int, default=100, help="mass-event cap per apply run")
    result.add_argument(
        "--apply",
        action="store_true",
        help="remove eligible worktrees; default is dry-run",
    )
    result.add_argument("--json", action="store_true", help="emit JSON instead of the human report")
    result.add_argument("--summary-only", action="store_true", help="omit per-worktree dry-run rows")
    result.add_argument(
        "--no-fetch",
        action="store_true",
        help="do not refresh origin refs; intended for tests/offline diagnosis",
    )
    result.add_argument(
        "--ledger",
        type=Path,
        default=Path("~/.cm/worktree-reaper.jsonl"),
        help="append-only apply audit ledger",
    )
    result.add_argument(
        "--artifact-root",
        type=Path,
        default=Path("~/.cm/worktree-artifacts"),
        help="same-filesystem vault for ignored outputs preserved before removal",
    )
    result.add_argument(
        "--artifact-retention-days",
        type=int,
        default=30,
        help="retention for unpinned archived outputs and standalone bundles",
    )
    result.add_argument(
        "--max-artifact-removals",
        type=int,
        default=100,
        help="mass-event cap for expired artifact removal per apply run",
    )
    result.add_argument(
        "--lock",
        type=Path,
        default=Path("~/.cm/worktree-reaper.lock"),
        help="host-local apply lock",
    )
    result.add_argument("--now", type=float, help=argparse.SUPPRESS)
    return result


def main(argv: Sequence[str] | None = None) -> int:
    signal.signal(signal.SIGPIPE, signal.SIG_DFL)
    args = parser().parse_args(argv)
    if args.retention_days < 1:
        parser().error("--retention-days must be positive")
    if args.unowned_retention_days < 1:
        parser().error("--unowned-retention-days must be positive")
    if args.max_removals < 1:
        parser().error("--max-removals must be positive")
    if args.artifact_retention_days < 1:
        parser().error("--artifact-retention-days must be positive")
    if args.max_artifact_removals < 1:
        parser().error("--max-artifact-removals must be positive")

    lock_path = args.lock.expanduser()
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    with lock_path.open("a+") as lock_handle:
        fcntl.flock(lock_handle, fcntl.LOCK_EX)
        contexts, warnings = build_scan_contexts(args)
        decision_contexts = [(decision(path, ctx), ctx) for ctx in contexts for path in worktree_paths(ctx.root)]
        size_by_path = directory_sizes(item.path for item, _ in decision_contexts if item.eligible)
        decision_contexts = [
            (
                dataclasses.replace(item, size_bytes=size_by_path.get(item.path.resolve(strict=False))) if item.eligible else item,
                ctx,
            )
            for item, ctx in decision_contexts
        ]
        decisions = [item for item, _ in decision_contexts]
        artifact_gc = artifact_gc_candidates(
            args.artifact_root,
            contexts[0].now,
            args.artifact_retention_days,
        )
        apply_results: list[dict[str, Any]] = []
        artifact_gc_results: list[dict[str, Any]] = []
        if args.apply:
            eligible_with_context = sorted(
                ((item, ctx) for item, ctx in decision_contexts if item.eligible),
                key=lambda pair: pair[0].size_bytes or -1,
                reverse=True,
            )
            for candidate, candidate_context in eligible_with_context:
                if len(apply_results) >= args.max_removals:
                    break
                removed, message, size = reap_one(candidate, candidate_context, args.ledger.expanduser())
                apply_results.append(
                    {
                        "path": str(candidate.path),
                        "removed": removed,
                        "message": message,
                        "bytes": size,
                    }
                )
            for artifact in artifact_gc[: args.max_artifact_removals]:
                removed, message = remove_artifact_candidate(
                    artifact,
                    args.artifact_root,
                    args.ledger.expanduser(),
                )
                artifact_gc_results.append(
                    {
                        "path": str(artifact.path),
                        "removed": removed,
                        "message": message,
                        "bytes": artifact.size_bytes,
                    }
                )

    if args.json:
        print(
            json.dumps(
                {
                    "mode": "apply" if args.apply else "dry-run",
                    "policy": {
                        "retention_days": args.retention_days,
                        "unowned_retention_days": args.unowned_retention_days,
                        "terminal_statuses": sorted(TERMINAL_TASK_STATUSES),
                        "roots": [str(ctx.root) for ctx in contexts],
                        "artifact_root": str(args.artifact_root.expanduser().resolve(strict=False)),
                        "artifact_retention_days": args.artifact_retention_days,
                    },
                    "warnings": warnings,
                    "decisions": [item.as_dict() for item in decisions],
                    "apply_results": apply_results,
                    "artifact_gc": [item.as_dict() for item in artifact_gc],
                    "artifact_gc_results": artifact_gc_results,
                },
                indent=2,
                sort_keys=True,
            )
        )
    else:
        print_human(
            decisions,
            warnings,
            apply_results,
            artifact_gc,
            artifact_gc_results,
            summary_only=args.summary_only,
        )
    if args.apply and warnings:
        return 2
    all_apply_results = apply_results + artifact_gc_results
    return 1 if args.apply and any(not item["removed"] for item in all_apply_results) else 0


if __name__ == "__main__":
    raise SystemExit(main())
