"""scripts/cm-availability: argument parsing, the RPC it sends, and output."""
from __future__ import annotations

import importlib.machinery
import importlib.util
import io
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest import mock

_PATH = Path(__file__).resolve().parents[2] / "scripts" / "cm-availability"
_loader = importlib.machinery.SourceFileLoader("cm_availability", str(_PATH))
_spec = importlib.util.spec_from_loader("cm_availability", _loader)
cli = importlib.util.module_from_spec(_spec)
_loader.exec_module(cli)


class CmAvailabilityTests(unittest.TestCase):
    def run_cli(self, argv, reply):
        out = io.StringIO()
        with mock.patch.object(cli, "rpc", return_value=reply) as rpc, redirect_stdout(out):
            code = cli.main(argv)
        return code, out.getvalue().strip(), rpc

    def test_no_argument_reads_the_level(self):
        reply = {"ok": True, "result": {"owner_availability": {
            "level": "focused", "set": True, "changed_at": "2026-10-06T14:02:11.000Z",
            "age_s": 840, "owner_note": "deep work"}}}
        code, out, rpc = self.run_cli([], reply)
        self.assertEqual(code, 0)
        rpc.assert_called_once_with("messaging.availability", {"action": "get"})
        self.assertEqual(out, "focused since 14:02Z (14m) — deep work")

    def test_level_and_note_set_with_a_fresh_request_id(self):
        reply = {"ok": True, "result": {"owner_availability": {"level": "away", "set": True}}}
        code, out, rpc = self.run_cli(["away", "--note", "back 08:00Z"], reply)
        params = rpc.call_args.args[1]
        self.assertEqual((params["action"], params["level"], params["note"], params["source"]),
                         ("set", "away", "back 08:00Z", "cli"))
        self.assertTrue(params["request_id"].startswith("cm-availability-"))
        self.assertEqual(out, "away")

    def test_unset_and_errors(self):
        code, out, _ = self.run_cli(["unset"], {"ok": True, "result": {"owner_availability": {"set": False}}})
        self.assertEqual(out, "unset (every Owner alert is delivered)")
        code, _, _ = self.run_cli(["away"], {"ok": False, "error": {"message": "unauthorized: Only Owner"}})
        self.assertEqual(code, 1)
        with self.assertRaises(SystemExit):
            cli.main(["asleep"])
        with self.assertRaises(SystemExit):
            cli.main(["--note", "x"])


if __name__ == "__main__":
    unittest.main()
