#!/usr/bin/env python3
"""Install the laptop callback forward for the cloud-owned Codex LB pool."""

from pathlib import Path
import errno
import json
import os
import socket
import subprocess
import tempfile
import time


UNIT_NAME = "cm-codex-oauth-tunnel.service"
UNIT = """[Unit]
Description=Codex LB browser login callback to cm-sessions

[Service]
Type=simple
ExecStart=/usr/bin/ssh -NT -o BatchMode=yes -o ControlMaster=no -o ControlPath=none -o ExitOnForwardFailure=yes -o ConnectTimeout=10 -o ServerAliveInterval=30 -o ServerAliveCountMax=3 -L localhost:1455:127.0.0.1:1455 cm-sessions
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
"""


def systemctl(*args, check=True):
    return subprocess.run(["systemctl", "--user", *args], check=check,
                          capture_output=True, text=True, timeout=30)


def active():
    return systemctl("is-active", "--quiet", UNIT_NAME, check=False).returncode == 0


def ensure_port_free():
    # A different pool or a direct `codex login` may already own this port.
    for family, address in ((socket.AF_INET, "127.0.0.1"), (socket.AF_INET6, "::1")):
        if family == socket.AF_INET6 and not socket.has_ipv6:
            continue
        try:
            with socket.socket(family, socket.SOCK_STREAM) as probe:
                probe.bind((address, 1455))
        except OSError as exc:
            # IPv6 may be disabled even when the Python build supports it.
            if family == socket.AF_INET6 and exc.errno in (errno.EADDRNOTAVAIL, errno.EAFNOSUPPORT):
                continue
            raise SystemExit("Port 1455 is already in use or cannot be bound. Finish the other local login before installing; no process was stopped.") from exc


def listening():
    try:
        with socket.create_connection(("localhost", 1455), timeout=1):
            return True
    except OSError:
        return False


def verify_target():
    # Read only public readiness. Never load/copy accounts, keys, or OAuth codes.
    result = subprocess.run(
        ["ssh", "-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "cm-sessions",
         "curl --fail --silent --show-error --max-time 10 http://127.0.0.1:2455/health/ready"],
        check=True, capture_output=True, text=True, timeout=25,
    )
    if json.loads(result.stdout).get("status") not in ("ok", "ready"):
        raise SystemExit("The cm-sessions Codex LB readiness check failed; nothing was installed.")


def install():
    if socket.gethostname().split(".", 1)[0] in ("cm-sessions", "cm-manager"):
        raise SystemExit("Run this installer in a local laptop terminal, outside a CM cloud Bash pane.")
    path = Path.home() / ".config/systemd/user" / UNIT_NAME
    exists = path.exists() or path.is_symlink()
    if exists and (path.is_symlink() or path.read_text() != UNIT):
        raise SystemExit(f"Refusing to replace the existing, different unit: {path}")
    already_active = active()
    if already_active and not exists:
        raise SystemExit("An existing unit with this name is active outside the installer location; nothing changed.")
    if not already_active:
        ensure_port_free()
    verify_target()
    path.parent.mkdir(parents=True, exist_ok=True)
    if not exists:
        with tempfile.NamedTemporaryFile(mode="w", dir=path.parent, delete=False) as handle:
            handle.write(UNIT)
            temporary = Path(handle.name)
        try:
            temporary.chmod(0o644)
            os.replace(temporary, path)
        finally:
            temporary.unlink(missing_ok=True)
    try:
        systemctl("daemon-reload")
        systemctl("enable", "--now", UNIT_NAME)
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            if active() and listening():
                print("Installed and enabled: laptop localhost:1455 -> cm-sessions localhost:1455.")
                print("Open Codex LB at http://localhost:2455 and start a fresh browser sign-in. The old login link may have expired.")
                print("Account enrollment is verified only when the dashboard reports success.")
                return
            time.sleep(0.5)
        raise RuntimeError("The callback tunnel did not become ready within 15 seconds.")
    except (OSError, subprocess.SubprocessError, RuntimeError):
        if not exists:
            systemctl("disable", "--now", UNIT_NAME, check=False)
            path.unlink(missing_ok=True)
            systemctl("daemon-reload", check=False)
        raise


if __name__ == "__main__":
    try:
        install()
    except (OSError, ValueError, subprocess.SubprocessError, RuntimeError) as exc:
        raise SystemExit(f"Callback tunnel installation failed: {exc}. Check SSH access with 'ssh cm-sessions true' and the user service with 'systemctl --user status {UNIT_NAME}'. No account credentials were changed.") from exc
