"""Tests for `control_client.default_socket_path` resolution.

Phase-1 transitional state: default is still `~/.cm/tui.sock` because
the daemon scaffold (slice 2 of doc/persistent-host-daemon.md) closes
incoming connections — it doesn't dispatch MCP RPCs yet. The default
flips to `~/.cm/daemon.sock` only after daemon-side dispatch lands
(deferred half of slice 4). `CM_USE_DAEMON_SOCKET=1` is the opt-in
flag for exercising the new path before the flip.
"""

from __future__ import annotations

import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest import mock

from mcp_server.control_client import default_socket_path


class DefaultSocketPathTests(unittest.TestCase):
    def test_env_override_wins(self):
        # Even when daemon.sock and tui.sock both exist, an explicit
        # CM_TUI_SOCKET wins. This is the daemon/TUI-spawned-agent
        # path: the parent injects the env, the agent uses it
        # without filesystem probes.
        with TemporaryDirectory() as tmp:
            cm_dir = Path(tmp) / ".cm"
            cm_dir.mkdir()
            (cm_dir / "daemon.sock").touch()
            (cm_dir / "tui.sock").touch()
            with mock.patch.dict(
                "os.environ",
                {
                    "HOME": tmp,
                    "CM_TUI_SOCKET": "/tmp/explicit-override.sock",
                },
                clear=True,
            ):
                self.assertEqual(
                    default_socket_path(),
                    Path("/tmp/explicit-override.sock"),
                )

    def test_env_empty_string_is_treated_as_unset(self):
        with TemporaryDirectory() as tmp:
            with mock.patch.dict(
                "os.environ",
                {"HOME": tmp, "CM_TUI_SOCKET": "   "},
                clear=True,
            ):
                self.assertEqual(
                    default_socket_path(),
                    Path(tmp) / ".cm" / "tui.sock",
                )

    def test_default_is_tui_sock_even_when_daemon_sock_exists(self):
        # The reviewer-caught regression: pre-revert, daemon.sock
        # being present would cause this case to return daemon.sock,
        # which the daemon scaffold then closed — breaking every MCP
        # call. The default must stay on tui.sock until daemon-side
        # dispatch is wired.
        with TemporaryDirectory() as tmp:
            cm_dir = Path(tmp) / ".cm"
            cm_dir.mkdir()
            (cm_dir / "daemon.sock").touch()
            with mock.patch.dict(
                "os.environ", {"HOME": tmp}, clear=True
            ):
                self.assertEqual(
                    default_socket_path(),
                    cm_dir / "tui.sock",
                    "default must be tui.sock until daemon dispatches",
                )

    def test_default_is_tui_sock_when_no_socket_exists(self):
        # No daemon.sock, no tui.sock — the resolver still produces
        # a well-formed tui.sock path. Connect will surface a useful
        # "no such file" error to the caller.
        with TemporaryDirectory() as tmp:
            with mock.patch.dict(
                "os.environ", {"HOME": tmp}, clear=True
            ):
                self.assertEqual(
                    default_socket_path(),
                    Path(tmp) / ".cm" / "tui.sock",
                )

    # 10f: deleted `test_opt_in_uses_daemon_sock_when_it_exists` —
    # the opt-in resolution branch is gone (daemon-mandatory model;
    # routing is decided by explicit env vars set by build_env).

    def test_opt_in_env_var_silently_ignored_after_flip(self):
        # 10f: `CM_USE_DAEMON_SOCKET` is now a silent no-op. With
        # neither CM_DAEMON_SOCKET nor CM_TUI_SOCKET set, the
        # resolver falls through to `~/.cm/tui.sock` regardless of
        # the env var's value. This pins the post-flip ignore
        # contract.
        with TemporaryDirectory() as tmp:
            cm_dir = Path(tmp) / ".cm"
            cm_dir.mkdir()
            (cm_dir / "daemon.sock").touch()
            for env_val in ("1", "true", "yes", "0", "", "TRUE"):
                with mock.patch.dict(
                    "os.environ",
                    {"HOME": tmp, "CM_USE_DAEMON_SOCKET": env_val},
                    clear=True,
                ):
                    self.assertEqual(
                        default_socket_path(),
                        cm_dir / "tui.sock",
                        f"CM_USE_DAEMON_SOCKET={env_val!r} must be ignored post-flip",
                    )

    def test_missing_home_falls_back_to_tmp(self):
        with mock.patch.dict("os.environ", {}, clear=True):
            p = default_socket_path()
            self.assertEqual(p, Path("/tmp") / ".cm" / "tui.sock")

    def test_cm_daemon_socket_env_pin_wins_over_tui_socket_env(self):
        # Slice-11 review fix: agents spawned by a TUI running with
        # CM_USE_DAEMON_SOCKET=1 receive CM_DAEMON_SOCKET, not
        # CM_TUI_SOCKET. The Rust side guarantees never injecting
        # both, but if a hand-crafted env supplies both, the
        # daemon-socket pin still wins — the explicit daemon
        # injection encodes the operator's deliberate choice.
        with mock.patch.dict(
            "os.environ",
            {
                "CM_DAEMON_SOCKET": "/tmp/daemon-pin.sock",
                "CM_TUI_SOCKET": "/tmp/tui-pin.sock",
            },
            clear=True,
        ):
            self.assertEqual(
                default_socket_path(),
                Path("/tmp/daemon-pin.sock"),
                "CM_DAEMON_SOCKET must take precedence over CM_TUI_SOCKET",
            )

    def test_cm_daemon_socket_env_pin_alone_resolves(self):
        with mock.patch.dict(
            "os.environ",
            {"CM_DAEMON_SOCKET": "/tmp/only-daemon-pin.sock"},
            clear=True,
        ):
            self.assertEqual(
                default_socket_path(),
                Path("/tmp/only-daemon-pin.sock"),
            )

    def test_cm_daemon_socket_empty_string_treated_as_unset(self):
        # Whitespace-only / empty CM_DAEMON_SOCKET must not bind the
        # resolver to an empty path — fall through to CM_TUI_SOCKET
        # or the filesystem fallback.
        with mock.patch.dict(
            "os.environ",
            {"CM_DAEMON_SOCKET": "   ", "CM_TUI_SOCKET": "/tmp/legit.sock"},
            clear=True,
        ):
            self.assertEqual(
                default_socket_path(),
                Path("/tmp/legit.sock"),
            )


if __name__ == "__main__":
    unittest.main()
