"""Tests for `control_client.resolve_socket_route()` — sub-2b-3
review-5 #3.

The resolver exposes the socket path AND whether the daemon was
chosen. `server.py` uses the boolean to pick between
`mcp_start_session` (daemon's minimal-shape MCP entry) and
`start_session` (TUI's full-shape handler). Pre-fix, `server.py`
only checked `CM_DAEMON_SOCKET` directly, which diverged from
`default_socket_path()` — under `CM_USE_DAEMON_SOCKET=1` the
socket routed to the daemon but the method stayed `start_session`,
which the daemon rejects with `InvalidParams` because the wire
shape is missing required fields.
"""

from __future__ import annotations

import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest import mock

from mcp_server.control_client import (
    default_socket_path,
    resolve_socket_route,
)


class ResolveSocketRouteTests(unittest.TestCase):
    def test_explicit_daemon_socket_chose_daemon_true(self):
        """Explicit `CM_DAEMON_SOCKET` → chose_daemon=True."""
        with mock.patch.dict(
            "os.environ",
            {"CM_DAEMON_SOCKET": "/tmp/explicit-daemon.sock"},
            clear=True,
        ):
            route = resolve_socket_route()
        self.assertEqual(route.path, Path("/tmp/explicit-daemon.sock"))
        self.assertTrue(
            route.chose_daemon,
            "CM_DAEMON_SOCKET signals daemon target",
        )

    def test_explicit_tui_socket_chose_daemon_false(self):
        """Explicit `CM_TUI_SOCKET` → chose_daemon=False."""
        with mock.patch.dict(
            "os.environ",
            {"CM_TUI_SOCKET": "/tmp/explicit-tui.sock"},
            clear=True,
        ):
            route = resolve_socket_route()
        self.assertEqual(route.path, Path("/tmp/explicit-tui.sock"))
        self.assertFalse(
            route.chose_daemon,
            "CM_TUI_SOCKET signals TUI target",
        )

    def test_use_daemon_opt_in_with_daemon_sock_present(self):
        """`CM_USE_DAEMON_SOCKET=1` with `~/.cm/daemon.sock` present
        → chose_daemon=True. Pre-fix, the OPT-IN flag would route the
        SOCKET to the daemon but `server.py`'s method selection
        ignored it, so the call dialed daemon with the wrong wire
        shape. This is the test that exercises the unified
        decision."""
        with TemporaryDirectory() as tmp:
            cm_dir = Path(tmp) / ".cm"
            cm_dir.mkdir()
            (cm_dir / "daemon.sock").touch()
            with mock.patch.dict(
                "os.environ",
                {"HOME": tmp, "CM_USE_DAEMON_SOCKET": "1"},
                clear=True,
            ):
                route = resolve_socket_route()
            self.assertEqual(route.path, cm_dir / "daemon.sock")
            self.assertTrue(
                route.chose_daemon,
                "opt-in + present daemon.sock must signal daemon route — "
                "the divergence this slice fixes is the method selector "
                "ignoring this path",
            )

    def test_use_daemon_opt_in_without_daemon_sock_falls_back(self):
        """`CM_USE_DAEMON_SOCKET=1` without daemon.sock on disk → falls
        back to `tui.sock`, chose_daemon=False. The flag is an
        OPT-IN, not a force — if the daemon binary isn't running and
        the socket isn't there, the resolver shouldn't send traffic
        nowhere."""
        with TemporaryDirectory() as tmp:
            with mock.patch.dict(
                "os.environ",
                {"HOME": tmp, "CM_USE_DAEMON_SOCKET": "1"},
                clear=True,
            ):
                route = resolve_socket_route()
            self.assertEqual(route.path, Path(tmp) / ".cm" / "tui.sock")
            self.assertFalse(route.chose_daemon)

    def test_no_env_fallback(self):
        """No env vars → tui.sock, chose_daemon=False."""
        with TemporaryDirectory() as tmp:
            with mock.patch.dict(
                "os.environ", {"HOME": tmp}, clear=True
            ):
                route = resolve_socket_route()
            self.assertEqual(route.path, Path(tmp) / ".cm" / "tui.sock")
            self.assertFalse(route.chose_daemon)

    def test_default_socket_path_matches_route_path(self):
        """The legacy `default_socket_path()` returns the same path
        as `resolve_socket_route().path`. Ensures the path-only
        helper stays in lockstep with the route resolver — no
        independent env-var sniffing."""
        with TemporaryDirectory() as tmp:
            cm_dir = Path(tmp) / ".cm"
            cm_dir.mkdir()
            (cm_dir / "daemon.sock").touch()
            for env in [
                {"HOME": tmp},
                {"HOME": tmp, "CM_USE_DAEMON_SOCKET": "1"},
                {"CM_DAEMON_SOCKET": "/tmp/explicit.sock", "HOME": tmp},
                {"CM_TUI_SOCKET": "/tmp/explicit-tui.sock", "HOME": tmp},
            ]:
                with self.subTest(env=env), mock.patch.dict(
                    "os.environ", env, clear=True
                ):
                    self.assertEqual(
                        default_socket_path(),
                        resolve_socket_route().path,
                        "default_socket_path() must mirror resolve_socket_route().path",
                    )


class StartSessionMethodSelectionTests(unittest.TestCase):
    """Pin the wire-method that `server.start_session()` picks based
    on the resolved route. Pre-fix this only checked
    `CM_DAEMON_SOCKET`; under `CM_USE_DAEMON_SOCKET=1` it would
    send `start_session` to the daemon (which rejects the minimal
    shape with InvalidParams). The fix routes via
    `resolve_socket_route().chose_daemon`.
    """

    def test_opt_in_flag_selects_mcp_start_session(self):
        """`CM_USE_DAEMON_SOCKET=1` + daemon.sock present → the
        wrapper sends `mcp_start_session`. This is the regression
        the slice closes."""
        from mcp_server import control_client

        captured: dict = {}

        def fake_call(method, params, **kw):
            captured["method"] = method
            captured["params"] = params
            return {"session_uid": "ts-fake"}

        with TemporaryDirectory() as tmp:
            cm_dir = Path(tmp) / ".cm"
            cm_dir.mkdir()
            (cm_dir / "daemon.sock").touch()
            with mock.patch.dict(
                "os.environ",
                {"HOME": tmp, "CM_USE_DAEMON_SOCKET": "1"},
                clear=True,
            ), mock.patch.object(control_client, "call", side_effect=fake_call):
                # Import here so the patched control_client is in scope.
                from mcp_server import server as mcp_server

                # `start_session` is a plain module-level
                # function; FastMCP's @tool decorator returns the
                # original callable, so we invoke directly.
                mcp_server.start_session(type="bash", label="test")

        self.assertEqual(
            captured.get("method"),
            "mcp_start_session",
            "CM_USE_DAEMON_SOCKET=1 must route start_session→mcp_start_session "
            "so the daemon's minimal-shape handler accepts the wire payload",
        )

    def test_explicit_cm_daemon_socket_selects_mcp_start_session(self):
        """`CM_DAEMON_SOCKET` set → `mcp_start_session` (legacy
        behavior preserved)."""
        from mcp_server import control_client

        captured: dict = {}

        def fake_call(method, params, **kw):
            captured["method"] = method
            return {"session_uid": "ts-fake"}

        with mock.patch.dict(
            "os.environ", {"CM_DAEMON_SOCKET": "/tmp/x.sock"}, clear=True
        ), mock.patch.object(control_client, "call", side_effect=fake_call):
            from mcp_server import server as mcp_server

            mcp_server.start_session(type="bash", label="test")

        self.assertEqual(captured.get("method"), "mcp_start_session")

    def test_no_env_selects_tui_start_session(self):
        """No daemon env → `start_session` (TUI handler)."""
        from mcp_server import control_client

        captured: dict = {}

        def fake_call(method, params, **kw):
            captured["method"] = method
            return {"session_uid": "ts-fake"}

        with TemporaryDirectory() as tmp:
            # No daemon.sock and no CM_USE_DAEMON_SOCKET → tui.sock
            # is the resolved socket, method is the TUI's.
            with mock.patch.dict(
                "os.environ", {"HOME": tmp}, clear=True
            ), mock.patch.object(control_client, "call", side_effect=fake_call):
                from mcp_server import server as mcp_server

                mcp_server.start_session(type="bash", label="test")

        self.assertEqual(
            captured.get("method"),
            "start_session",
            "no daemon route → TUI's full-shape start_session handler",
        )


class RouteResolutionPassthroughTests(unittest.TestCase):
    """Sub-2b-3 review-8 #2: the socket path that informed the
    method-string choice in `server.py` must be the SAME path
    `control_client.call()` dials. Pre-fix, the two resolutions
    were independent — a daemon socket appearing or
    disappearing between them (which is realistic under
    `CM_USE_DAEMON_SOCKET=1` testing) could route the wrong
    method shape to the wrong server.
    """

    def test_call_uses_explicit_socket_path_ignoring_env_changes(self):
        """When `call()` receives a `socket_path=` argument, it
        dials exactly that path even if the env has been mutated
        post-resolution to point elsewhere. This is the race-
        between-resolutions guarantee — the test simulates the
        race by changing `CM_DAEMON_SOCKET` between the
        `resolve_socket_route()` call and the `call()`
        invocation."""
        from mcp_server import control_client

        attempted_path: list = []

        def fake_socket_connect(self, addr):
            # Capture what path was actually dialed.
            attempted_path.append(str(addr))
            # Raise so we don't proceed into the real socket
            # send loop — that needs a real listener.
            raise OSError("test: aborting before real connect")

        # Resolve via the helper FIRST, then mutate env to a
        # different daemon path. Without the pass-through, the
        # second resolution would pick up the mutated env.
        with mock.patch.dict(
            "os.environ",
            {"CM_DAEMON_SOCKET": "/tmp/resolved-at-decision-time.sock"},
            clear=True,
        ):
            route = control_client.resolve_socket_route()
        self.assertEqual(str(route.path), "/tmp/resolved-at-decision-time.sock")
        # Now the env mutates AFTER resolution — simulating
        # the race-between-resolutions window.
        with mock.patch.dict(
            "os.environ",
            {"CM_DAEMON_SOCKET": "/tmp/INTRUDER-after-resolution.sock"},
            clear=True,
        ), mock.patch("socket.socket.connect", new=fake_socket_connect):
            try:
                control_client.call(
                    "ping",
                    {},
                    socket_path=route.path,
                )
            except control_client.TransportError:
                pass  # expected — we aborted via fake_socket_connect
        self.assertEqual(
            attempted_path,
            ["/tmp/resolved-at-decision-time.sock"],
            "call() must use the explicitly-passed socket_path, "
            "NOT re-resolve from env (review-8 #2)",
        )

    def test_call_without_socket_path_falls_back_to_resolver(self):
        """When `call()` is invoked without `socket_path=` (the
        ad-hoc CLI path), it resolves via the legacy
        `default_socket_path()`. The pass-through is opt-in for
        two-step callers; one-step callers retain the legacy
        behavior."""
        from mcp_server import control_client

        attempted_path: list = []

        def fake_socket_connect(self, addr):
            attempted_path.append(str(addr))
            raise OSError("test abort")

        with TemporaryDirectory() as tmp:
            with mock.patch.dict(
                "os.environ",
                {"HOME": tmp, "CM_TUI_SOCKET": "/tmp/legacy-tui.sock"},
                clear=True,
            ), mock.patch("socket.socket.connect", new=fake_socket_connect):
                try:
                    control_client.call("ping", {})
                except control_client.TransportError:
                    pass
        self.assertEqual(
            attempted_path,
            ["/tmp/legacy-tui.sock"],
            "call() without socket_path must fall back to default_socket_path()",
        )


class StartSessionPassesResolvedSocketToCallTests(unittest.TestCase):
    """Verifies `server.py::start_session` resolves once and
    passes the path through. Without the pass-through (legacy
    code), the test would observe `socket_path=None` at the
    call boundary."""

    def test_start_session_passes_resolved_path_to_call(self):
        from mcp_server import control_client

        captured: dict = {}

        def fake_call(method, params, *args, socket_path=None, **kw):
            captured["method"] = method
            captured["socket_path"] = socket_path
            return {"session_uid": "ts-fake"}

        with TemporaryDirectory() as tmp:
            cm_dir = Path(tmp) / ".cm"
            cm_dir.mkdir()
            (cm_dir / "daemon.sock").touch()
            with mock.patch.dict(
                "os.environ",
                {"HOME": tmp, "CM_USE_DAEMON_SOCKET": "1"},
                clear=True,
            ), mock.patch.object(control_client, "call", side_effect=fake_call):
                from mcp_server import server as mcp_server

                mcp_server.start_session(type="bash", label="test")
        self.assertEqual(
            captured.get("socket_path"),
            cm_dir / "daemon.sock",
            "server.start_session must pass the resolved socket_path "
            "(review-8 #2 — single-resolution two-step routing)",
        )


if __name__ == "__main__":
    unittest.main()
