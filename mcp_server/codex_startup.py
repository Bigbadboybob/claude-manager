"""Suppress only Codex's cached update notice before a CM frontend starts.

The CLI's update-check switch does not suppress an already cached version.
No terminal input, auth/config rewrites or package installation belongs here.
"""
from __future__ import annotations

import fcntl
import json
import os
from pathlib import Path
import stat
import time
import uuid

UPDATE_OFF = "check_for_update_on_startup=false"
MAX_CACHE_BYTES = 64 * 1024
LOCK_WAIT = 1.0


class StartupSuppressionError(RuntimeError):
    pass


def _regular(fd):
    meta = os.fstat(fd)
    if not stat.S_ISREG(meta.st_mode) or meta.st_nlink != 1 or meta.st_uid != os.getuid():
        raise StartupSuppressionError("unsafe update-cache file")
    return meta


def _stamp(meta):
    return (meta.st_dev, meta.st_ino, meta.st_size, meta.st_mtime_ns, meta.st_ctime_ns)


def _object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise StartupSuppressionError("duplicate update-cache key")
        result[key] = value
    return result


def _read_cache(root):
    try:
        fd = os.open("version.json", os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=root)
    except FileNotFoundError:
        return "missing", None, None
    with os.fdopen(fd, "rb") as f:
        before = _regular(f.fileno())
        if before.st_size > MAX_CACHE_BYTES:
            raise StartupSuppressionError("oversized update cache")
        raw = f.read(MAX_CACHE_BYTES + 1)
        if len(raw) > MAX_CACHE_BYTES or _stamp(os.fstat(f.fileno())) != _stamp(before):
            raise StartupSuppressionError("update cache changed during read")
    if _stamp(os.stat("version.json", dir_fd=root, follow_symlinks=False)) != _stamp(before):
        raise StartupSuppressionError("update cache changed before dismissal")
    if not raw.strip():
        return "empty", None, before
    data = json.loads(raw, object_pairs_hook=_object,
                      parse_constant=lambda _: (_ for _ in ()).throw(ValueError()))
    if not isinstance(data, dict):
        raise StartupSuppressionError("update cache must be an object")
    latest = data.get("latest_version")
    if latest is None or latest == "":
        return "no_notice", data, before
    if not isinstance(latest, str) or len(latest.encode()) > 256:
        raise StartupSuppressionError("update cache has invalid latest_version")
    if data.get("dismissed_version") == latest:
        return "already_dismissed", data, before
    return "notice", data, before


def dismiss_cached_update():
    """Preserve unknown keys, changing only dismissed_version; fail closed.

    CODEX_HOME is a literal path, just as in Codex (no tilde expansion). Empty
    or unset uses ~/.codex. Its parent may be absent on a clean install; both
    child processes must also receive UPDATE_OFF. A CM-only flock serializes
    concurrent CM launches. Codex itself does not honor it: pre-replace identity
    checks detect observed races, but cannot exclude a later upstream writer.
    """
    home = Path(os.environ.get("CODEX_HOME") or Path.home() / ".codex")
    root = lock = None
    tmp = None
    try:
        try:
            # Follow a configured home alias like Codex, then anchor all file IO
            # to that directory. Missing is distinct from permission/IO errors.
            root = os.open(home, os.O_RDONLY | os.O_DIRECTORY | os.O_NONBLOCK)
        except FileNotFoundError:
            # lstat distinguishes a dangling configured root from genuine absence.
            try:
                home.lstat()
            except FileNotFoundError:
                # Missing ancestor could itself be a dangling symlink.
                for parent in home.parents:
                    try:
                        parent.lstat()
                    except FileNotFoundError:
                        continue
                    try:
                        parent.stat()
                    except FileNotFoundError:
                        raise StartupSuppressionError("dangling update-cache home") from None
                    break
                return "missing"
            raise StartupSuppressionError("dangling update-cache home")
        # A clean/managed install needs no lock file or write access at all.
        try:
            status, _, _ = _read_cache(root)
        except StartupSuppressionError as exc:
            if not str(exc).startswith("update cache changed"):
                raise
            status = "notice"  # A concurrent CM writer: re-read under its lock.
        if status != "notice":
            return status
        lock = os.open(".cm-version.lock", os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600, dir_fd=root)
        _regular(lock)
        deadline = time.monotonic() + LOCK_WAIT
        while True:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    raise StartupSuppressionError("update-cache lock busy")
                time.sleep(0.01)
        status, data, before = _read_cache(root)
        if status != "notice":
            return status
        latest = data["latest_version"]
        if not before.st_mode & stat.S_IWUSR:
            raise StartupSuppressionError("update cache is read-only")
        data["dismissed_version"] = latest
        encoded = json.dumps(data, ensure_ascii=False, allow_nan=False).encode() + b"\n"
        if len(encoded) > MAX_CACHE_BYTES:
            raise StartupSuppressionError("oversized updated cache")
        tmp = ".cm-version-" + uuid.uuid4().hex
        fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600, dir_fd=root)
        with os.fdopen(fd, "wb") as f:
            f.write(encoded)
            f.flush()
            os.fsync(f.fileno())
        current = os.stat("version.json", dir_fd=root, follow_symlinks=False)
        if _stamp(current) != _stamp(before) or (os.stat(home).st_dev, os.stat(home).st_ino) != (os.fstat(root).st_dev, os.fstat(root).st_ino):
            raise StartupSuppressionError("update cache changed before replacement")
        os.replace(tmp, "version.json", src_dir_fd=root, dst_dir_fd=root)
        tmp = None
        os.fsync(root)
        # Refuse a detected upstream overwrite; don't keep retrying against it.
        fd = os.open("version.json", os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=root)
        with os.fdopen(fd, "rb") as f:
            _regular(f.fileno())
            if f.read(MAX_CACHE_BYTES + 1) != encoded:
                raise StartupSuppressionError("update cache changed after replacement")
        return "dismissed"
    except StartupSuppressionError as exc:
        raise StartupSuppressionError(
            f"{home / 'version.json'}: {exc}; repair the cache and set dismissed_version to latest_version"
        ) from None
    except (OSError, ValueError, UnicodeError, RecursionError) as exc:
        # Never print cache content or arbitrary JSON fields; name the repair file.
        raise StartupSuppressionError(
            f"{home / 'version.json'}: update-cache suppression failed ({type(exc).__name__}); "
            "repair the cache and set dismissed_version to latest_version"
        ) from None
    finally:
        if tmp is not None and root is not None:
            try:
                os.unlink(tmp, dir_fd=root)
            except OSError:
                pass
        if lock is not None:
            os.close(lock)
        if root is not None:
            os.close(root)
