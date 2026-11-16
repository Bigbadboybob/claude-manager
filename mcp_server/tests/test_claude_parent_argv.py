"""claude_parent_argv walks only through CM's own launcher and stdio proxy."""

import unittest
from unittest import mock

from mcp_server import native_claude

CLAUDE = ["claude", "--session-id", "abc", "--mcp-config", "/x.json"]
LAUNCHER = ["/bin/sh", "/home/u/.cm/mcp/launcher.sh", "server.py"]
PROXY = ["/opt/cm-daemon/mcp_server/.venv/bin/python",
         "/opt/cm-daemon/mcp_server/mcp_proxy.py", "--", "/home/u/.cm/mcp/launcher.sh", "server.py"]


def chain(*argvs):
    """pid 10 is the MCP server; each next argv is the next ancestor."""
    parents = {10 + i: 11 + i for i in range(len(argvs))}
    table = {11 + i: argv for i, argv in enumerate(argvs)}
    return (mock.patch.object(native_claude, "_ppid", side_effect=parents.get),
            mock.patch.object(native_claude, "_argv", side_effect=table.get))


class ClaudeParentArgvTests(unittest.TestCase):
    def walk(self, *argvs):
        ppid, argv = chain(*argvs)
        with ppid, argv:
            return native_claude.claude_parent_argv(10)

    def test_direct_launcher_and_proxy_chains_find_claude(self):
        self.assertEqual(self.walk(CLAUDE), CLAUDE)
        self.assertEqual(self.walk(LAUNCHER, CLAUDE), CLAUDE)
        self.assertEqual(self.walk(PROXY, CLAUDE), CLAUDE)  # launcher exec'd the proxy
        self.assertEqual(self.walk(PROXY, LAUNCHER, CLAUDE), CLAUDE)
        self.assertEqual(native_claude.argv_session_id(self.walk(PROXY, CLAUDE)), "abc")

    def test_unrelated_ancestors_stop_the_walk(self):
        self.assertIsNone(self.walk(["python", "other.py"], CLAUDE))
        self.assertIsNone(self.walk(["bash"], CLAUDE))
        self.assertIsNone(self.walk(PROXY, LAUNCHER, PROXY, CLAUDE))  # at most three steps


if __name__ == "__main__":
    unittest.main()
