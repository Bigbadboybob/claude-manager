"""Account switching must follow Claude Code's files and lock, and never leak tokens."""

import contextlib
import importlib.machinery
import importlib.util
import io
import json
import os
from pathlib import Path
import stat
import sys
import tempfile
import threading
import time
import unittest
from unittest import mock


SCRIPT = Path(__file__).resolve().parents[1] / "scripts/cm-claude-accounts"
loader = importlib.machinery.SourceFileLoader("cm_claude_accounts", str(SCRIPT))
spec = importlib.util.spec_from_loader(loader.name, loader)
accounts = importlib.util.module_from_spec(spec)
sys.modules[loader.name] = accounts  # dataclasses resolve annotations through it
loader.exec_module(accounts)


def login(tag, refresh_expires_in=30 * 86400):
    now_ms = int(time.time() * 1000)
    return {
        "accessToken": f"sk-ant-oat-ACCESS-{tag}",
        "refreshToken": f"sk-ant-ort-REFRESH-{tag}",
        "expiresAt": now_ms + 8 * 3600 * 1000,
        "refreshTokenExpiresAt": now_ms + refresh_expires_in * 1000,
        "scopes": ["user:inference"],
        "subscriptionType": "max",
        "rateLimitTier": "default_claude_max_20x",
    }


def profile(name):
    return {"accountUuid": f"{name}-uuid-0000", "emailAddress": f"{name}@example.com",
            "organizationUuid": f"{name}-org"}


class AccountsTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name)
        (root / ".claude").mkdir()
        self.paths = accounts.Paths(root / ".claude", root / ".claude.json", root / "store")
        self.mcp = {"some-server": {"accessToken": "mcp-secret"}}
        self.paths.claude_json.write_text(json.dumps({"numStartups": 7, "projects": {"x": 1}}))
        self.paths.claude_json.chmod(0o600)
        self.tokens = set()

    # Simulate Owner's /login: Claude Code writes both files.
    def do_login(self, name, tag=None):
        oauth = login(tag or name)
        self.tokens.update({oauth["accessToken"], oauth["refreshToken"]})
        creds = self.paths.credentials
        data = json.loads(creds.read_text()) if creds.exists() else {"mcpOAuth": self.mcp}
        data["claudeAiOauth"] = oauth
        creds.write_text(json.dumps(data))
        creds.chmod(0o600)
        cj = json.loads(self.paths.claude_json.read_text())
        cj["oauthAccount"] = profile(name)
        self.paths.claude_json.write_text(json.dumps(cj))

    # Simulate a session's token refresh: rotates both tokens, profile untouched.
    def session_refresh(self, tag):
        data = json.loads(self.paths.credentials.read_text())
        data["claudeAiOauth"] = login(tag)
        self.tokens.update({data["claudeAiOauth"]["accessToken"], data["claudeAiOauth"]["refreshToken"]})
        self.paths.credentials.write_text(json.dumps(data))

    def run_cli(self, *argv):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stderr(err):
            code = accounts.main(list(argv), paths=self.paths, out=out)
        text = out.getvalue() + err.getvalue()
        for token in self.tokens | {"mcp-secret"}:
            self.assertNotIn(token, text)
        return code, text

    def live(self):
        return json.loads(self.paths.credentials.read_text())

    def live_email(self):
        return json.loads(self.paths.claude_json.read_text())["oauthAccount"]["emailAddress"]

    def state(self):
        return json.loads(self.paths.state.read_text())

    def capture_two(self):
        self.do_login("alice")
        self.assertEqual(self.run_cli("capture")[0], 0)
        self.do_login("bob")
        self.assertEqual(self.run_cli("capture")[0], 0)

    def test_capture_saves_by_uuid_privately(self):
        self.do_login("alice")
        code, text = self.run_cli("capture")
        self.assertEqual(code, 0)
        self.assertIn("alice@example.com", text)
        saved = self.paths.accounts / "alice-uuid-0000.json"
        self.assertEqual(stat.S_IMODE(saved.stat().st_mode), 0o600)
        self.assertEqual(stat.S_IMODE(self.paths.store.stat().st_mode), 0o700)
        record = json.loads(saved.read_text())
        self.assertEqual(record["claudeAiOauth"]["refreshToken"], "sk-ant-ort-REFRESH-alice")
        self.assertNotIn("mcpOAuth", record)
        self.assertEqual(self.state()["active"], "alice-uuid-0000")

    def test_capture_refuses_profile_that_lags_the_tokens(self):
        self.capture_two()
        # Tokens are bob's, but the profile now names alice (stale display data).
        cj = json.loads(self.paths.claude_json.read_text())
        cj["oauthAccount"] = profile("alice")
        self.paths.claude_json.write_text(json.dumps(cj))
        code, text = self.run_cli("capture")
        self.assertEqual(code, 1)
        self.assertIn("belongs to bob@example.com", text)
        saved = json.loads((self.paths.accounts / "alice-uuid-0000.json").read_text())
        self.assertEqual(saved["claudeAiOauth"]["refreshToken"], "sk-ant-ort-REFRESH-alice")

    def test_switch_swaps_in_place_and_keeps_other_secrets_and_settings(self):
        self.capture_two()
        inode = self.paths.credentials.stat().st_ino
        code, text = self.run_cli("switch", "alice@example.com")
        self.assertEqual(code, 0, text)
        live = self.live()
        self.assertEqual(live["claudeAiOauth"]["refreshToken"], "sk-ant-ort-REFRESH-alice")
        self.assertEqual(live["mcpOAuth"], self.mcp)
        self.assertEqual(self.paths.credentials.stat().st_ino, inode)
        self.assertEqual(stat.S_IMODE(self.paths.credentials.stat().st_mode), 0o600)
        cj = json.loads(self.paths.claude_json.read_text())
        self.assertEqual(cj["oauthAccount"]["emailAddress"], "alice@example.com")
        self.assertEqual((cj["numStartups"], cj["projects"]), (7, {"x": 1}))
        self.assertEqual(self.state()["active"], "alice-uuid-0000")
        self.assertEqual(self.state()["history"][-1]["event"], "switch")

    def test_switch_writes_back_rotated_tokens_so_switching_back_works(self):
        self.capture_two()
        self.session_refresh("bob-2")  # bob's tokens rotate; saved copy is now stale
        self.assertEqual(self.run_cli("switch", "alice")[0], 0)  # accepts uuid prefix
        saved_bob = json.loads((self.paths.accounts / "bob-uuid-0000.json").read_text())
        self.assertEqual(saved_bob["claudeAiOauth"]["refreshToken"], "sk-ant-ort-REFRESH-bob-2")
        self.assertEqual(self.run_cli("switch", "bob@example.com")[0], 0)
        self.assertEqual(self.live()["claudeAiOauth"]["refreshToken"], "sk-ant-ort-REFRESH-bob-2")

    def test_external_login_is_captured_not_overwritten(self):
        self.capture_two()
        self.do_login("carol")  # Owner ran /login outside the tool
        code, text = self.run_cli("switch", "alice@example.com")
        self.assertEqual(code, 0, text)
        saved = json.loads((self.paths.accounts / "carol-uuid-0000.json").read_text())
        self.assertEqual(saved["claudeAiOauth"]["refreshToken"], "sk-ant-ort-REFRESH-carol")
        self.assertRegex(text, r"switched carol@example\.com \S+ -> alice@example\.com")
        self.assertEqual(self.state()["history"][-2]["event"], "external-login")

    def test_unidentifiable_login_is_refused_and_nothing_changes(self):
        self.capture_two()
        self.session_refresh("mystery")
        cj = json.loads(self.paths.claude_json.read_text())
        del cj["oauthAccount"]
        self.paths.claude_json.write_text(json.dumps(cj))
        before = self.paths.credentials.read_text()
        state = json.loads(self.paths.state.read_text())
        state["active"] = None
        self.paths.state.write_text(json.dumps(state))
        code, text = self.run_cli("switch", "alice@example.com")
        self.assertEqual(code, 1)
        self.assertIn("cannot tell which account", text)
        self.assertEqual(self.paths.credentials.read_text(), before)

    def test_expired_target_login_is_refused_and_marked(self):
        self.do_login("alice")
        self.run_cli("capture")
        record_path = self.paths.accounts / "alice-uuid-0000.json"
        record = json.loads(record_path.read_text())
        record["claudeAiOauth"]["refreshTokenExpiresAt"] = int(time.time() * 1000) - 1000
        record_path.write_text(json.dumps(record))
        self.do_login("bob")
        self.run_cli("capture")
        before = self.paths.credentials.read_text()
        code, text = self.run_cli("switch", "alice@example.com")
        self.assertEqual(code, 1)
        self.assertIn("has expired", text)
        self.assertEqual(self.paths.credentials.read_text(), before)
        self.assertTrue(json.loads(record_path.read_text())["needs_login"])
        self.assertIn("needs /login", self.run_cli("list")[1])

    def test_switch_holds_claude_code_refresh_lock_while_writing(self):
        self.capture_two()
        seen = []
        real = accounts.write_in_place

        def checking_write(path, data):
            seen.append([d.is_dir() for d in self.paths.refresh_locks])
            # A session trying to refresh now gets EEXIST, as with proper-lockfile.
            with self.assertRaises(FileExistsError):
                os.mkdir(self.paths.refresh_locks[0])
            real(path, data)

        with mock.patch.object(accounts, "write_in_place", checking_write):
            self.assertEqual(self.run_cli("switch", "alice@example.com")[0], 0)
        self.assertEqual(seen, [[True, True]])
        self.assertFalse(any(d.exists() for d in self.paths.refresh_locks))

    def test_switch_waits_for_a_refreshing_session(self):
        self.capture_two()
        lock = self.paths.refresh_locks[0]
        os.mkdir(lock)

        def session_finishes_refresh():
            time.sleep(0.5)
            self.session_refresh("bob-3")
            os.rmdir(lock)

        worker = threading.Thread(target=session_finishes_refresh)
        worker.start()
        code, text = self.run_cli("switch", "alice@example.com")
        worker.join()
        self.assertEqual(code, 0, text)
        saved_bob = json.loads((self.paths.accounts / "bob-uuid-0000.json").read_text())
        self.assertEqual(saved_bob["claudeAiOauth"]["refreshToken"], "sk-ant-ort-REFRESH-bob-3")

    def test_busy_lock_times_out_without_changes(self):
        self.capture_two()
        os.mkdir(self.paths.refresh_locks[1])  # legacy lock held by a session
        before = self.paths.credentials.read_text()
        with mock.patch.object(accounts, "LOCK_WAIT_SECONDS", 0.3), \
                mock.patch.object(accounts.RefreshLock.__init__, "__defaults__", (0.3,)):
            code, text = self.run_cli("switch", "alice@example.com")
        self.assertEqual(code, 1)
        self.assertIn("refresh lock busy", text)
        self.assertEqual(self.paths.credentials.read_text(), before)
        # The current lock taken before the legacy one failed was released again.
        self.assertFalse(self.paths.refresh_locks[0].exists())

    def test_list_marks_active_and_hides_tokens(self):
        self.capture_two()
        code, text = self.run_cli("list")
        self.assertEqual(code, 0)
        lines = text.splitlines()
        self.assertTrue(any(l.startswith("* bob@example.com") for l in lines))
        self.assertTrue(any(l.startswith("  alice@example.com") for l in lines))

    def test_remove_refuses_active_account(self):
        self.capture_two()
        self.assertEqual(self.run_cli("remove", "bob@example.com")[0], 1)
        self.assertEqual(self.run_cli("remove", "alice@example.com")[0], 0)
        self.assertFalse((self.paths.accounts / "alice-uuid-0000.json").exists())


if __name__ == "__main__":
    unittest.main()
