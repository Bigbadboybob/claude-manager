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

    def test_daemon_socket_env_selects_mcp_start_session(self):
        """Sub-2c: under daemon-spawn the TUI's `build_env`
        populates `CM_DAEMON_SOCKET` (plus `CM_TUI_SOCKET` too).
        The wrapper's method-selection consults
        `daemon_socket_pinned()` — non-empty `CM_DAEMON_SOCKET`
        → `mcp_start_session`. Pre-2c the opt-in
        `CM_USE_DAEMON_SOCKET=1` had its own route-decision
        branch; sub-2c folds that into the env-var-driven model
        (`build_env` is the single producer of the env)."""
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
            (cm_dir / "tui.sock").touch()
            # Production env shape under SpawnTarget::Daemon
            # (sub-2c): both sockets pinned, no opt-in flag
            # leaking in.
            with mock.patch.dict(
                "os.environ",
                {
                    "HOME": tmp,
                    "CM_DAEMON_SOCKET": str(cm_dir / "daemon.sock"),
                    "CM_TUI_SOCKET": str(cm_dir / "tui.sock"),
                },
                clear=True,
            ), mock.patch.object(control_client, "call", side_effect=fake_call):
                from mcp_server import server as mcp_server

                mcp_server.start_session(type="bash", label="test")

        self.assertEqual(
            captured.get("method"),
            "mcp_start_session",
            "daemon-spawn env (CM_DAEMON_SOCKET pinned) must route "
            "start_session→mcp_start_session so the daemon's minimal-"
            "shape handler accepts the wire payload",
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
    """Sub-2c review-2 (restoring round-8 #2): the
    method-string choice and the actual socket dial must bind
    to ONE resolution. `server.start_session` calls
    `resolve_socket_route()` once and passes the result both
    as the method-selection signal AND as `socket_path=` to
    `control_client.call()`, so a daemon socket
    appearing/disappearing between two independent
    resolutions can't route the wrong method to the wrong
    server."""

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
            (cm_dir / "tui.sock").touch()
            with mock.patch.dict(
                "os.environ",
                {
                    "HOME": tmp,
                    "CM_DAEMON_SOCKET": str(cm_dir / "daemon.sock"),
                    "CM_TUI_SOCKET": str(cm_dir / "tui.sock"),
                },
                clear=True,
            ), mock.patch.object(control_client, "call", side_effect=fake_call):
                from mcp_server import server as mcp_server

                mcp_server.start_session(type="bash", label="test")
        self.assertEqual(captured.get("method"), "mcp_start_session")
        self.assertEqual(
            captured.get("socket_path"),
            cm_dir / "daemon.sock",
            "sub-2c review-2: server.start_session must pass the "
            "resolved socket_path to call() so the method choice and "
            "the dial are bound to one resolution",
        )

    def test_start_session_under_opt_in_flag_passes_daemon_path(self):
        """Under `CM_USE_DAEMON_SOCKET=1` (round-13 opt-in,
        no explicit CM_DAEMON_SOCKET), the resolved path is
        `~/.cm/daemon.sock`. server.start_session must pass
        THAT path to call() — pre-fix the path was missing
        and `call()` re-resolved (which would also pick the
        opt-in path, but the binding wasn't there)."""
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
        self.assertEqual(captured.get("method"), "mcp_start_session")
        self.assertEqual(
            captured.get("socket_path"),
            cm_dir / "daemon.sock",
            "opt-in flag must pass `~/.cm/daemon.sock` through to call()",
        )

    def test_start_session_tui_local_passes_tui_path(self):
        """TUI-spawn (no daemon pin, no opt-in): method is
        `start_session`, path is the TUI socket. Both bound
        in one resolution."""
        from mcp_server import control_client

        captured: dict = {}

        def fake_call(method, params, *args, socket_path=None, **kw):
            captured["method"] = method
            captured["socket_path"] = socket_path
            return {"session_uid": "ts-fake"}

        with TemporaryDirectory() as tmp:
            with mock.patch.dict(
                "os.environ", {"HOME": tmp}, clear=True
            ), mock.patch.object(control_client, "call", side_effect=fake_call):
                from mcp_server import server as mcp_server

                mcp_server.start_session(type="bash", label="test")
        self.assertEqual(captured.get("method"), "start_session")
        self.assertEqual(
            captured.get("socket_path"),
            Path(tmp) / ".cm" / "tui.sock",
            "TUI-spawn: server.start_session must pass the TUI socket "
            "path to call()",
        )


class PerMethodRoutingTests(unittest.TestCase):
    """Sub-2c: under daemon-spawn the TUI's `build_env` populates
    BOTH `CM_DAEMON_SOCKET` and `CM_TUI_SOCKET` with real paths.
    `control_client.call(method, ...)` routes per-method via
    `DAEMON_METHODS`:
      - methods in the set → CM_DAEMON_SOCKET (no silent
        fallback when daemon is pinned)
      - methods not in the set → CM_TUI_SOCKET (the agent's
        only path to `workflow_transition` / `workflow_done`
        from inside a daemon-spawned session)
    """

    def test_daemon_method_routes_to_daemon_socket_when_pinned(self):
        """`mcp_start_session` ∈ DAEMON_METHODS → daemon socket
        when CM_DAEMON_SOCKET is pinned, regardless of
        CM_TUI_SOCKET being populated too."""
        from mcp_server import control_client

        with TemporaryDirectory() as tmp:
            with mock.patch.dict(
                "os.environ",
                {
                    "HOME": tmp,
                    "CM_DAEMON_SOCKET": "/tmp/d.sock",
                    "CM_TUI_SOCKET": "/tmp/t.sock",
                },
                clear=True,
            ):
                path = control_client.resolve_socket_for_method("mcp_start_session")
        self.assertEqual(str(path), "/tmp/d.sock")

    def test_tui_only_method_routes_to_tui_socket_under_daemon_spawn(self):
        """`workflow_transition` ∉ DAEMON_METHODS → CM_TUI_SOCKET
        even when CM_DAEMON_SOCKET is pinned. This is the
        load-bearing sub-2c invariant: daemon-spawned agents
        can reach `workflow_transition` / `workflow_done` via
        the TUI socket."""
        from mcp_server import control_client

        with TemporaryDirectory() as tmp:
            with mock.patch.dict(
                "os.environ",
                {
                    "HOME": tmp,
                    "CM_DAEMON_SOCKET": "/tmp/d.sock",
                    "CM_TUI_SOCKET": "/tmp/t.sock",
                },
                clear=True,
            ):
                for tui_method in (
                    "workflow_transition",
                    "workflow_done",
                    "create_subtask",
                    "get_workflow_state",
                ):
                    with self.subTest(method=tui_method):
                        path = control_client.resolve_socket_for_method(tui_method)
                        self.assertEqual(
                            str(path),
                            "/tmp/t.sock",
                            f"{tui_method!r} must route to CM_TUI_SOCKET — "
                            "this is the sub-2c fix for workflow methods "
                            "reachable from daemon-spawned agents",
                        )

    def test_daemon_method_falls_back_to_tui_when_no_daemon_pin(self):
        """Under TuiLocal-spawn (CM_DAEMON_SOCKET unset), a
        daemon-supported method like `list_sessions` falls back
        to CM_TUI_SOCKET — the TUI implements list_sessions too,
        so this is the legitimate path."""
        from mcp_server import control_client

        with TemporaryDirectory() as tmp:
            with mock.patch.dict(
                "os.environ",
                {"HOME": tmp, "CM_TUI_SOCKET": "/tmp/t.sock"},
                clear=True,
            ):
                path = control_client.resolve_socket_for_method("list_sessions")
        self.assertEqual(
            str(path),
            "/tmp/t.sock",
            "TuiLocal-spawn (no daemon pin) falls back to TUI socket "
            "even for daemon-supported methods",
        )

    def test_empty_daemon_pin_treated_as_unset(self):
        """`CM_DAEMON_SOCKET=""` (empty) counts as unset —
        same hygiene as the Rust env_socket_override helper.
        Under TuiLocal target the daemon socket is the empty
        string (authoritative empty); the resolver must NOT
        try to dial '' as a path."""
        from mcp_server import control_client

        with TemporaryDirectory() as tmp:
            with mock.patch.dict(
                "os.environ",
                {
                    "HOME": tmp,
                    "CM_DAEMON_SOCKET": "",
                    "CM_TUI_SOCKET": "/tmp/t.sock",
                },
                clear=True,
            ):
                path = control_client.resolve_socket_for_method("list_sessions")
                self.assertEqual(str(path), "/tmp/t.sock")
                self.assertFalse(
                    control_client.daemon_socket_pinned(),
                    "empty CM_DAEMON_SOCKET must not be treated as pinned",
                )


class ProposeTaskPassesResolvedSocketToCallTests(unittest.TestCase):
    """Sub-2c review-2: `propose_task` must also bind the
    daemon-branch decision to the same resolution it uses for
    `call(socket_path=)`. Pre-fix the branch checked
    `daemon_socket_pinned()` and `call()` re-resolved
    independently — same TOCTOU as start_session."""

    def test_propose_task_passes_resolved_path_under_daemon_pin(self):
        from mcp_server import control_client

        captured: dict = {}

        def fake_call(method, params, *args, socket_path=None, **kw):
            captured["method"] = method
            captured["socket_path"] = socket_path
            return {"id": "t-fake", "name": "n"}

        def fake_detect_repo_url():
            return "git@github.com:fake/repo.git"

        with TemporaryDirectory() as tmp:
            cm_dir = Path(tmp) / ".cm"
            cm_dir.mkdir()
            (cm_dir / "daemon.sock").touch()
            with mock.patch.dict(
                "os.environ",
                {
                    "HOME": tmp,
                    "CM_DAEMON_SOCKET": str(cm_dir / "daemon.sock"),
                    "CM_TUI_SOCKET": str(cm_dir / "tui.sock"),
                },
                clear=True,
            ), mock.patch.object(control_client, "call", side_effect=fake_call):
                # Stub out the planning_client repo-url detector
                # so propose_task doesn't try to shell out to git.
                with mock.patch.dict(
                    "sys.modules",
                    {"cli.planning_client": mock.MagicMock(
                        _detect_repo_url=fake_detect_repo_url,
                    )},
                ):
                    from mcp_server import server as mcp_server
                    mcp_server.propose_task(
                        project="p", name="n", description="d", prompt="x"
                    )
        self.assertEqual(captured.get("method"), "propose_task")
        self.assertEqual(
            captured.get("socket_path"),
            cm_dir / "daemon.sock",
            "propose_task must bind the daemon-branch decision to "
            "the socket_path passed to call() (round-8 #2 / sub-2c review-2)",
        )


class OptInFlagRoutingTests(unittest.TestCase):
    """Sub-2c review-1: `CM_USE_DAEMON_SOCKET=1` (without
    explicit `CM_DAEMON_SOCKET`) is THE flag the project uses
    to enable daemon mode for testing — it's the round-13
    opt-in path. Sub-2c initially regressed this by checking
    `CM_DAEMON_SOCKET` directly in
    `resolve_socket_for_method`; the unified resolver now
    delegates daemon-path selection to `resolve_socket_route`,
    which honors both the explicit env var AND the opt-in flag.

    Each test below pins one of the reviewer-named cases.
    """

    def test_opt_in_flag_routes_daemon_methods_to_daemon_socket(self):
        """`CM_USE_DAEMON_SOCKET=1` + daemon.sock present
        (no explicit CM_DAEMON_SOCKET): every method in
        DAEMON_METHODS that an agent would call routes to
        the daemon socket."""
        from mcp_server import control_client

        with TemporaryDirectory() as tmp:
            cm_dir = Path(tmp) / ".cm"
            cm_dir.mkdir()
            (cm_dir / "daemon.sock").touch()
            with mock.patch.dict(
                "os.environ",
                {"HOME": tmp, "CM_USE_DAEMON_SOCKET": "1"},
                clear=True,
            ):
                for method in (
                    "ping",
                    "mcp_start_session",
                    "propose_task",
                    "list_sessions",
                    "send_input",
                    "read_session_output",
                    "kill_session",
                ):
                    with self.subTest(method=method):
                        path = control_client.resolve_socket_for_method(method)
                        self.assertEqual(
                            path,
                            cm_dir / "daemon.sock",
                            f"opt-in flag must route {method!r} to daemon "
                            "socket (regression of round-13 opt-in path)",
                        )

    def test_opt_in_flag_routes_tui_methods_to_tui_socket(self):
        """`CM_USE_DAEMON_SOCKET=1`: TUI-only methods like
        `workflow_transition` still route to CM_TUI_SOCKET
        (the opt-in flag affects DAEMON_METHODS routing, not
        TUI methods)."""
        from mcp_server import control_client

        with TemporaryDirectory() as tmp:
            cm_dir = Path(tmp) / ".cm"
            cm_dir.mkdir()
            (cm_dir / "daemon.sock").touch()
            (cm_dir / "tui.sock").touch()
            with mock.patch.dict(
                "os.environ",
                {
                    "HOME": tmp,
                    "CM_USE_DAEMON_SOCKET": "1",
                    "CM_TUI_SOCKET": str(cm_dir / "tui.sock"),
                },
                clear=True,
            ):
                path = control_client.resolve_socket_for_method("workflow_transition")
        self.assertEqual(path, cm_dir / "tui.sock")

    def test_no_env_routes_everything_to_tui_socket(self):
        """Neither CM_DAEMON_SOCKET nor CM_USE_DAEMON_SOCKET set:
        every method falls back to TUI socket (the home-relative
        default for ad-hoc CLI callers)."""
        from mcp_server import control_client

        with TemporaryDirectory() as tmp:
            with mock.patch.dict(
                "os.environ", {"HOME": tmp}, clear=True
            ):
                expected = Path(tmp) / ".cm" / "tui.sock"
                for method in (
                    "mcp_start_session",
                    "list_sessions",
                    "workflow_transition",
                    "ping",
                ):
                    with self.subTest(method=method):
                        path = control_client.resolve_socket_for_method(method)
                        self.assertEqual(
                            path,
                            expected,
                            f"no daemon env → {method!r} falls back to TUI socket",
                        )

    def test_explicit_cm_daemon_socket_routes_same_as_opt_in(self):
        """Explicit CM_DAEMON_SOCKET (no CM_USE_DAEMON_SOCKET):
        same routing as the opt-in flag — daemon methods to
        daemon, TUI methods to TUI."""
        from mcp_server import control_client

        with mock.patch.dict(
            "os.environ",
            {
                "CM_DAEMON_SOCKET": "/tmp/explicit-d.sock",
                "CM_TUI_SOCKET": "/tmp/explicit-t.sock",
            },
            clear=True,
        ):
            self.assertEqual(
                control_client.resolve_socket_for_method("mcp_start_session"),
                Path("/tmp/explicit-d.sock"),
            )
            self.assertEqual(
                control_client.resolve_socket_for_method("workflow_transition"),
                Path("/tmp/explicit-t.sock"),
            )

    def test_empty_string_daemon_socket_treated_as_unset(self):
        """`CM_DAEMON_SOCKET=""` is the round-13 invariant
        (the build_env authoritative-empty under TuiLocal).
        Must NOT be treated as pinned. Without
        CM_USE_DAEMON_SOCKET, everything routes to TUI."""
        from mcp_server import control_client

        with TemporaryDirectory() as tmp:
            with mock.patch.dict(
                "os.environ",
                {"HOME": tmp, "CM_DAEMON_SOCKET": ""},
                clear=True,
            ):
                expected = Path(tmp) / ".cm" / "tui.sock"
                self.assertEqual(
                    control_client.resolve_socket_for_method("mcp_start_session"),
                    expected,
                    "empty CM_DAEMON_SOCKET must be treated as unset (round-13)",
                )
                self.assertFalse(
                    control_client.daemon_socket_pinned(),
                    "empty CM_DAEMON_SOCKET must not be pinned",
                )


class LoudFailureOnUnreachableTargetTests(unittest.TestCase):
    """Sub-2c: when the resolver picks a socket for a method
    based on `DAEMON_METHODS`, and that socket is unreachable
    (file missing, connect refused), the failure surfaces as
    `TransportError` — NOT silently re-routed to the other
    socket. This is the safety property the reviewer named:
    `workflow_transition` under daemon-spawn with a missing
    TUI socket must fail loudly, never accidentally land on
    the daemon (which doesn't implement it).
    """

    def test_tui_only_method_fails_loudly_when_tui_socket_missing(self):
        """Under daemon-spawn with both env vars set but the
        TUI socket file missing, calling a TUI-only method
        raises TransportError. It does NOT silently re-route
        to the daemon socket (which would land on a method-
        not-found because the daemon doesn't dispatch
        workflow_transition)."""
        from mcp_server import control_client

        with TemporaryDirectory() as tmp:
            cm_dir = Path(tmp) / ".cm"
            cm_dir.mkdir()
            # Only the daemon socket exists; TUI socket does
            # not. Both env vars are set (sub-2c build_env
            # shape), so the resolver returns the TUI path
            # for non-daemon methods.
            daemon_sock = cm_dir / "daemon.sock"
            tui_sock = cm_dir / "tui.sock"  # NOT created
            daemon_sock.touch()
            with mock.patch.dict(
                "os.environ",
                {
                    "HOME": tmp,
                    "CM_DAEMON_SOCKET": str(daemon_sock),
                    "CM_TUI_SOCKET": str(tui_sock),
                },
                clear=True,
            ):
                # Sanity: the resolver returns the TUI path,
                # not the daemon path.
                self.assertEqual(
                    control_client.resolve_socket_for_method("workflow_transition"),
                    tui_sock,
                )
                # The actual call surfaces TransportError at
                # connect time — loud failure, not silent
                # re-route.
                with self.assertRaises(control_client.TransportError):
                    control_client.call(
                        "workflow_transition", {"to": "reviewer", "prompt": "x"}
                    )


class DaemonMethodsAlignmentTests(unittest.TestCase):
    """Sub-2c: snapshot test that `DAEMON_METHODS` matches the
    daemon's actual dispatch arms in
    `daemon/src/control/dispatch.rs`. If the daemon adds a new
    method and forgets to update `DAEMON_METHODS`, the call
    would route to TUI (which returns method-not-found — loud
    failure, but the test catches it earlier at build time).
    """

    def test_daemon_methods_matches_dispatch_arms(self):
        from mcp_server.control_client import DAEMON_METHODS

        dispatch_path = (
            Path(__file__).resolve().parent.parent.parent
            / "daemon" / "src" / "control" / "dispatch.rs"
        )
        content = dispatch_path.read_text()
        # Match Rust dispatch arms of the form:
        #     "method_name" => DispatchOutcome::Done(...)
        # or  "method_name" => { ... }
        # Only arms inside the dispatch_request match — they all
        # have the same `        "..." =>` indentation prefix.
        import re
        arms = re.findall(r'^\s{8}"([a-zA-Z0-9_.]+)"\s*=>', content, re.MULTILINE)
        # `_` => is the catch-all; not a method name.
        dispatch_methods = {m for m in arms if m and not m.startswith("_")}
        # The set must match exactly. If the daemon adds a method,
        # update DAEMON_METHODS; if a method is removed from
        # daemon dispatch, drop it from DAEMON_METHODS.
        self.assertEqual(
            set(DAEMON_METHODS),
            dispatch_methods,
            "DAEMON_METHODS must match daemon/src/control/dispatch.rs arms. "
            f"Only-in-DAEMON_METHODS: {set(DAEMON_METHODS) - dispatch_methods!r}; "
            f"only-in-dispatch.rs: {dispatch_methods - set(DAEMON_METHODS)!r}",
        )


if __name__ == "__main__":
    unittest.main()
