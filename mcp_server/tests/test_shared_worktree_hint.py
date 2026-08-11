"""`workspace_shared_with` — the shared-checkout hint on `list_sessions`.

Before this hint, an agent listing its siblings got a `worktree_path`
per row and had to eyeball which ones matched to learn that another
live session was editing the same checkout. Now the MCP layer computes
it: every live row that shares a `worktree_path` with another live row
carries `workspace_shared_with` naming the others.

The hint is derived STRICTLY from the rows the host returned — which
are already scoped to the caller by the host's own auth walk (the
daemon's `list_sessions` runs every row through `should_include`; the
TUI twin runs `caller_authorized_for`). These tests pin that: the tool
issues exactly ONE `list_sessions` call and never widens the row set.
"""

from __future__ import annotations

import unittest
from unittest import mock


def _row(uid, path, *, label=None, state="ready", idle=True):
    return {
        "session_uid": uid,
        "label": label or uid,
        "type": "claude-code",
        "state": state,
        "idle": idle,
        "worktree_path": path,
    }


class SharedWorktreeHintTests(unittest.TestCase):
    def _list(self, rows, **kwargs):
        from mcp_server import control_client, server

        calls: list = []

        def fake_call(method, params=None, **kw):
            calls.append((method, params))
            return rows

        with mock.patch.object(control_client, "call", side_effect=fake_call):
            out = server.list_sessions(**kwargs)
        return out, calls

    def test_shared_path_names_the_other_live_sessions(self):
        rows = [
            _row("ses-a", "/wt/one", label="worker"),
            _row("ses-b", "/wt/one", label="reviewer"),
            _row("ses-c", "/wt/one", label="manager"),
            _row("ses-solo", "/wt/two", label="solo"),
        ]
        out, calls = self._list(rows)
        by_uid = {s["session_uid"]: s for s in out}

        self.assertEqual(
            by_uid["ses-a"]["workspace_shared_with"],
            [
                {"session_uid": "ses-b", "label": "reviewer"},
                {"session_uid": "ses-c", "label": "manager"},
            ],
            "the hint lists the OTHER sessions on the path, never self",
        )
        self.assertEqual(
            [p["session_uid"] for p in by_uid["ses-c"]["workspace_shared_with"]],
            ["ses-a", "ses-b"],
        )
        self.assertNotIn(
            "workspace_shared_with",
            by_uid["ses-solo"],
            "a session alone in its checkout must not carry the key",
        )
        self.assertEqual(
            [m for m, _ in calls],
            ["list_sessions"],
            "the hint must come from the caller-scoped rows we already "
            "have — no second, wider query",
        )

    def test_no_key_when_nothing_is_shared(self):
        rows = [_row("ses-a", "/wt/one"), _row("ses-b", "/wt/two")]
        out, _ = self._list(rows)
        for s in out:
            self.assertNotIn("workspace_shared_with", s)

    def test_pathless_rows_are_not_grouped_together(self):
        # `worktree_path` is null for a cloud/anchor workspace. Two nulls
        # are not "the same checkout" — grouping them would invent a
        # collision that doesn't exist.
        rows = [_row("ses-a", None), _row("ses-b", None)]
        out, _ = self._list(rows)
        for s in out:
            self.assertNotIn("workspace_shared_with", s)

    def test_exited_rows_neither_receive_nor_count(self):
        rows = [
            _row("ses-live", "/wt/one"),
            _row("ses-dead", "/wt/one", state="exited"),
        ]
        out, _ = self._list(rows, include_exited=True)
        by_uid = {s["session_uid"]: s for s in out}
        self.assertNotIn(
            "workspace_shared_with",
            by_uid["ses-live"],
            "a closed session isn't touching the checkout — the live "
            "session still has it to itself",
        )
        self.assertNotIn("workspace_shared_with", by_uid["ses-dead"])

    def test_exited_row_does_not_hide_a_real_sharer(self):
        rows = [
            _row("ses-a", "/wt/one"),
            _row("ses-b", "/wt/one"),
            _row("ses-dead", "/wt/one", state="exited"),
        ]
        out, _ = self._list(rows, include_exited=True)
        by_uid = {s["session_uid"]: s for s in out}
        self.assertEqual(
            by_uid["ses-a"]["workspace_shared_with"],
            [{"session_uid": "ses-b", "label": "ses-b"}],
        )

    def test_grouped_view_carries_the_same_hint(self):
        from mcp_server import control_client, server

        rows = [
            dict(_row("ses-a", "/wt/one"), workspace_id="ws-1"),
            # Same checkout, DIFFERENT workspace (in-place launch) — the
            # grouping key wouldn't reveal this, the hint does.
            dict(_row("ses-b", "/wt/one"), workspace_id="ws-2"),
        ]

        def fake_call(method, params=None, **kw):
            if method == "ping":
                return {"uid": "ses-a", "global_perms": True}
            return rows

        with mock.patch.object(control_client, "call", side_effect=fake_call):
            tree = server.list_sessions_grouped()

        found = {
            s["session_uid"]: s
            for ws in tree["workspaces"]
            for t in ws["tasks"]
            for s in t["sessions"]
        }
        self.assertEqual(len(tree["workspaces"]), 2, "grouped by workspace_id")
        self.assertEqual(
            found["ses-a"]["workspace_shared_with"],
            [{"session_uid": "ses-b", "label": "ses-b"}],
            "shared checkout is visible even across workspace groups",
        )

    def test_empty_listing_is_fine(self):
        out, _ = self._list([])
        self.assertEqual(out, [])


if __name__ == "__main__":
    unittest.main()
