#!/usr/bin/env python3
"""Inspect or raise the running laptop viewer's soft file limit without restarting it."""
import argparse
import os
from pathlib import Path
import resource
import socket


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--apply", action="store_true", help="raise the soft limit up to 65536 within the existing hard limit")
    args = parser.parse_args()
    if socket.gethostname().split(".")[0] in {"cm-sessions", "cm-manager"}:
        parser.error("Run in a local laptop terminal, outside a CM cloud Bash pane.")
    found = False
    for proc in Path("/proc").iterdir():
        if not proc.name.isdigit():
            continue
        try:
            if proc.stat().st_uid != os.getuid():
                continue
            executable = os.readlink(proc / "exe").removesuffix(" (deleted)")
            if Path(executable).name != "claude-manager-tui":
                continue
            found = True
            pid = int(proc.name)
            soft, hard = resource.prlimit(pid, resource.RLIMIT_NOFILE)
            count = len(list((proc / "fd").iterdir()))
            target = max(soft, min(65_536, hard)) if hard != resource.RLIM_INFINITY else max(soft, 65_536)
            if args.apply and target > soft:
                resource.prlimit(pid, resource.RLIMIT_NOFILE, (target, hard))
            current, _ = resource.prlimit(pid, resource.RLIMIT_NOFILE)
            print(f"Viewer PID {pid}: {count} open files; soft limit {soft} -> {current}; hard limit {hard}")
        except (FileNotFoundError, ProcessLookupError):
            continue
        except PermissionError as exc:
            print(f"Cannot inspect or update PID {proc.name}: {exc}")
    if not found:
        raise SystemExit("No running claude-manager-tui found for this user.")
    if args.apply:
        print("No sessions or processes were restarted. Retry Alt+s and image paste.")


if __name__ == "__main__":
    main()
