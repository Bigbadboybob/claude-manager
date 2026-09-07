"""Own one backend process group until its CM launcher pipe closes.

Kept separate from the async frontend so even SIGKILL of the launcher closes
the pipe and reaps the backend and its tools. stdin is a liveness pipe, never
agent input. No session history or credentials are read here.
"""

import ctypes
import os
import select
import signal
import subprocess
import sys
import time
from pathlib import Path


def parent_death_signal():
    if sys.platform == "linux":
        parent = os.getppid()
        ctypes.CDLL(None).prctl(1, signal.SIGTERM)
        if os.getppid() != parent:
            os.kill(os.getpid(), signal.SIGTERM)


def main():
    if sys.argv[1:2] == ["--exec-child"]:
        expected_parent = int(sys.argv[2])
        if os.getppid() != expected_parent:
            return 143
        parent_death_signal()
        if os.getppid() != expected_parent:
            return 143
        os.execvp(sys.argv[3], sys.argv[3:])

    stop = False

    def stopping(_sig, _frame):
        nonlocal stop
        stop = True

    for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        signal.signal(sig, stopping)
    child = subprocess.Popen(
        [
            sys.executable,
            str(Path(__file__).resolve()),
            "--exec-child",
            str(os.getpid()),
            *sys.argv[1:],
        ],
        stdin=subprocess.DEVNULL,
        start_new_session=True,
    )
    try:
        while not stop and child.poll() is None:
            ready, _, _ = select.select([sys.stdin.buffer], [], [], 0.1)
            if ready and not os.read(0, 1):
                break
    finally:
        # A group can still contain tools/plugin helpers after its leader exits.
        # This group was minted by our own child and is never looked up by name.
        try:
            os.killpg(child.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            try:
                os.killpg(child.pid, 0)
            except ProcessLookupError:
                break
            child.poll()
            time.sleep(0.05)
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        child.wait()
    return child.returncode if child.returncode >= 0 else 128 - child.returncode


if __name__ == "__main__":
    raise SystemExit(main())
