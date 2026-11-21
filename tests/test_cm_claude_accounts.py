"""Account switching must follow Claude Code's files and lock, and never leak tokens."""

import contextlib
import email.utils
from datetime import datetime, timezone
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


# Every test class, including those that do not inherit Harness, shares this
# notifier sandbox. Unknown notification commands fail before any subprocess.
_notifier_dir = None
_notifier_paths = set()
_test_subprocess_paths = set()
_original_run = accounts.subprocess.run
_original_notify = accounts.notify


def guarded_subprocess_run(argv, *args, **kwargs):
    command = argv[0] if isinstance(argv, (list, tuple)) and argv else argv
    if not isinstance(command, (str, os.PathLike)) or str(command) not in _test_subprocess_paths:
        raise AssertionError(f"tests must never invoke an unapproved subprocess: {command}")
    return _original_run(argv, *args, **kwargs)


def guarded_notify(state, paths, message, command):
    if str(command) not in _notifier_paths:
        raise AssertionError(f"test tried an unapproved notifier: {command}")
    return _original_notify(state, paths, message, command)


def setUpModule():
    global _notifier_dir, _notifier_patches
    _notifier_dir = tempfile.TemporaryDirectory(prefix="cm-accounts-notifier-")
    fake = Path(_notifier_dir.name) / "fake-notify"
    fake.write_text("#!/bin/sh\nexit 0\n")
    fake.chmod(0o700)
    _notifier_paths.add(str(fake))
    _test_subprocess_paths.add(str(fake))
    _notifier_patches = [mock.patch.object(accounts, "DEFAULT_NOTIFY", fake),
                         mock.patch.object(accounts, "notify", guarded_notify),
                         mock.patch.object(accounts.subprocess, "run", guarded_subprocess_run)]
    for patch in _notifier_patches:
        patch.start()


def tearDownModule():
    for patch in reversed(_notifier_patches):
        patch.stop()
    _notifier_paths.clear()
    _test_subprocess_paths.clear()
    _notifier_dir.cleanup()


class NotifierGuardTests(unittest.TestCase):
    def test_real_notifier_cannot_be_invoked(self):
        self.assertNotEqual(Path(accounts.DEFAULT_NOTIFY).name, "cm-notify")
        with self.assertRaisesRegex(AssertionError, "unapproved notifier"):
            accounts.notify({}, None, "test", "/home/lucas/.cm/bin/cm-notify")
        with self.assertRaisesRegex(AssertionError, "unapproved subprocess"):
            accounts.subprocess.run(["/home/lucas/.cm/bin/cm-notify", "test"])
        with self.assertRaisesRegex(AssertionError, "unapproved subprocess"):
            accounts.subprocess.run(["/bin/sh", "-c", "/home/lucas/.cm/bin/cm-notify test"])


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


class Harness(unittest.TestCase):
    """Fake Claude files and a fake Anthropic profile endpoint."""

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
        # Fake Anthropic profile endpoint: access token -> account uuid.
        self.anthropic = {}
        self.fetches = []

        def fetch(token):
            self.fetches.append([d.exists() for d in self.paths.refresh_locks])
            if token not in self.anthropic:
                raise accounts.AccountError("Anthropic rejected the current access token (HTTP 401)")
            return self.anthropic[token]

        self.enterContext(mock.patch.object(accounts, "fetch_account_uuid", side_effect=fetch))

    # Simulate Owner's /login: Claude Code writes both files.
    def do_login(self, name, tag=None):
        oauth = login(tag or name)
        self.tokens.update({oauth["accessToken"], oauth["refreshToken"]})
        self.anthropic[oauth["accessToken"]] = profile(name)["accountUuid"]
        creds = self.paths.credentials
        data = json.loads(creds.read_text()) if creds.exists() else {"mcpOAuth": self.mcp}
        data["claudeAiOauth"] = oauth
        creds.write_text(json.dumps(data))
        creds.chmod(0o600)
        cj = json.loads(self.paths.claude_json.read_text())
        cj["oauthAccount"] = profile(name)
        self.paths.claude_json.write_text(json.dumps(cj))

    # Simulate a session's token refresh: rotates both tokens, profile untouched.
    def session_refresh(self, tag, owner=None):
        data = json.loads(self.paths.credentials.read_text())
        data["claudeAiOauth"] = login(tag)
        if owner:
            self.anthropic[data["claudeAiOauth"]["accessToken"]] = profile(owner)["accountUuid"]
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


class AccountsTests(Harness):
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

    def write_profile_of(self, name):
        cj = json.loads(self.paths.claude_json.read_text())
        cj["oauthAccount"] = profile(name)
        self.paths.claude_json.write_text(json.dumps(cj))

    def saved_refresh(self, name):
        record = json.loads((self.paths.accounts / f"{name}-uuid-0000.json").read_text())
        return record["claudeAiOauth"]["refreshToken"]

    def test_capture_trusts_tokens_not_a_lagging_profile(self):
        self.capture_two()
        self.write_profile_of("alice")  # tokens are bob's; the profile lags
        code, text = self.run_cli("capture")
        self.assertEqual(code, 0, text)
        self.assertIn("captured bob@example.com", text)
        self.assertIn("names another account", text)
        self.assertEqual(self.saved_refresh("alice"), "sk-ant-ort-REFRESH-alice")
        saved_bob = json.loads((self.paths.accounts / "bob-uuid-0000.json").read_text())
        self.assertEqual(saved_bob["oauthAccount"]["emailAddress"], "bob@example.com")

    def test_stale_profile_after_refresh_is_not_taken_as_a_login(self):
        # Review scenario 1: on bob, a session writes alice's profile back,
        # then bob's tokens rotate. Nothing may be saved under alice.
        self.capture_two()
        self.assertEqual(self.run_cli("switch", "alice@example.com")[0], 0)
        self.assertEqual(self.run_cli("switch", "bob@example.com")[0], 0)
        self.write_profile_of("alice")
        self.session_refresh("bob-2", owner="bob")
        code, text = self.run_cli("switch", "alice@example.com")
        self.assertEqual(code, 0, text)
        self.assertRegex(text, r"switched bob@example\.com \S+ -> alice@example\.com")
        self.assertNotIn("already on", text)
        self.assertEqual(self.saved_refresh("alice"), "sk-ant-ort-REFRESH-alice")
        self.assertEqual(self.saved_refresh("bob"), "sk-ant-ort-REFRESH-bob-2")
        self.assertEqual(self.live()["claudeAiOauth"]["refreshToken"], "sk-ant-ort-REFRESH-alice")
        self.assertNotIn("external-login", [h["event"] for h in self.state()["history"]])

    def test_profile_write_failure_still_records_the_switch(self):
        # Review scenario 2: the credentials changed, so state must follow
        # even if ~/.claude.json cannot be updated.
        self.capture_two()
        with mock.patch.object(accounts, "write_profile",
                               side_effect=accounts.AccountError("kept changing")):
            code, text = self.run_cli("switch", "alice@example.com")
        self.assertEqual(code, 0, text)
        self.assertIn("warning: login switched", text)
        self.assertEqual(self.state()["active"], "alice-uuid-0000")
        self.assertEqual(self.live_email(), "bob@example.com")  # display lags; harmless
        self.session_refresh("alice-2", owner="alice")
        self.assertEqual(self.run_cli("switch", "bob@example.com")[0], 0)
        self.assertEqual(self.saved_refresh("alice"), "sk-ant-ort-REFRESH-alice-2")
        self.assertEqual(self.saved_refresh("bob"), "sk-ant-ort-REFRESH-bob")

    def test_confirmation_runs_outside_the_lock_and_retries_after_a_refresh(self):
        self.capture_two()
        self.session_refresh("bob-3", owner="bob")
        self.fetches.clear()
        real = self.anthropic.copy()

        def refresh_during_fetch(token):
            self.fetches.append([d.exists() for d in self.paths.refresh_locks])
            if len(self.fetches) == 1:
                self.session_refresh("bob-4", owner="bob")  # lands mid-identification
            return {**real, **self.anthropic}[token]

        with mock.patch.object(accounts, "fetch_account_uuid", side_effect=refresh_during_fetch):
            code, text = self.run_cli("switch", "alice@example.com")
        self.assertEqual(code, 0, text)
        self.assertEqual(self.fetches, [[False, False], [False, False]])
        self.assertEqual(self.saved_refresh("bob"), "sk-ant-ort-REFRESH-bob-4")

    def test_expired_unmatched_login_is_refused_without_asking_anthropic(self):
        self.capture_two()
        self.session_refresh("bob-5", owner="bob")
        data = self.live()
        data["claudeAiOauth"]["expiresAt"] = int(time.time() * 1000) - 1000
        self.paths.credentials.write_text(json.dumps(data))
        before = self.paths.credentials.read_text()
        self.fetches.clear()
        code, text = self.run_cli("switch", "alice@example.com")
        self.assertEqual(code, 1)
        self.assertIn("access token has expired", text)
        self.assertEqual(self.fetches, [])
        self.assertEqual(self.paths.credentials.read_text(), before)

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
        self.session_refresh("bob-2", owner="bob")  # saved copy is now stale
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
        self.assertIn("cannot confirm which account", text)
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
            self.session_refresh("bob-3", owner="bob")
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



def iso_in(seconds):
    return datetime.fromtimestamp(time.time() + seconds, timezone.utc).isoformat().replace("+00:00", "Z")


def usage(*limits, five_hour=None):
    """The usage endpoint's shape (2026-11-07): a limits list; five_hour is
    null on plans without a 5-hour limit."""
    return {"five_hour": five_hour, "seven_day": None, "extra": {"ignored": True},
            "limits": [{"kind": kind, "group": scope or "all", "scope": scope, "percent": pct,
                        "is_active": active, "severity": "normal", "resets_at": iso_in(reset_in)}
                       for kind, scope, pct, reset_in, active in limits]}


WEEKLY_ONLY = usage(("weekly_all", None, 89, 3 * 86400, True),
                    ("weekly_scoped", "opus", 40, 3 * 86400, False))


class PollHarness(Harness):
    """Adds a fake usage endpoint and a real fake notifier script."""

    def setUp(self):
        super().setUp()
        root = self.paths.store.parent
        self.sent = root / "sent.txt"
        self.notifier = root / "notify.sh"
        self.notifier.write_text(f'#!/bin/sh\nprintf "%s|%s\\n" "$CM_NOTIFY_TAG" "$1" >> {self.sent}\n')
        self.notifier.chmod(0o700)
        _notifier_paths.add(str(self.notifier))
        _test_subprocess_paths.add(str(self.notifier))
        self.addCleanup(_notifier_paths.discard, str(self.notifier))
        self.addCleanup(_test_subprocess_paths.discard, str(self.notifier))
        self.usage = {}  # access token -> usage response
        self.usage_error = None
        self.usage_calls = []

        def fetch(token):
            self.usage_calls.append(token)
            if self.usage_error:
                raise self.usage_error
            if token not in self.usage:
                raise accounts.TokenRejected("Anthropic rejected the current access token (HTTP 401)")
            return self.usage[token]

        self.enterContext(mock.patch.object(accounts, "fetch_usage", side_effect=fetch))
        # Never reach Anthropic's token endpoint from a test.
        self.enterContext(mock.patch.object(accounts, "refresh_login",
                                            side_effect=accounts.AccountError("no token renewal in tests")))
        # Most tests read usage on every poll; UsageIntervalTests covers the cache.
        self.enterContext(mock.patch.object(accounts, "USAGE_INTERVAL_SECONDS", 0))

    def poll(self):
        return self.run_cli("poll", "--notify-command", str(self.notifier))

    def alerts(self):
        return self.sent.read_text().splitlines() if self.sent.exists() else []

    def live_token(self):
        return self.live()["claudeAiOauth"]["accessToken"]

    def assert_no_tokens_on_disk(self):
        for path in (self.paths.state, self.paths.store / "alerts.log", self.sent):
            if path.exists():
                text = path.read_text()
                for token in self.tokens:
                    self.assertNotIn(token, text, path)


class PollTests(PollHarness):
    """poll reads only the live login, never refreshes, and alerts once."""

    def test_plan_without_a_5_hour_limit_is_read_from_the_limits_list(self):
        self.do_login("alice")
        self.run_cli("capture")
        self.usage[self.live_token()] = WEEKLY_ONLY
        code, text = self.poll()
        self.assertEqual(code, 0, text)
        reading = self.state()["usage"]["alice-uuid-0000"]
        self.assertEqual([(l["kind"], l["scope"], l["percent"]) for l in reading["limits"]],
                         [("weekly_all", None, 89), ("weekly_scoped", "opus", 40)])
        self.assertEqual(self.alerts(), [])  # 89% < 90% weekly threshold
        code, text = self.run_cli("status")
        self.assertIn("week 89% (resets in 3.0d, binding)", text)
        self.assertIn("week:opus 40%", text)
        self.assertNotIn("5h", text)
        self.assertIn("last poll 0m ago: ok", text)

    def test_older_fixed_field_shape_still_parses(self):
        limits = accounts.parse_limits({"five_hour": {"utilization": 12, "resets_at": iso_in(600)},
                                        "seven_day": {"utilization": 50, "resets_at": None}})
        self.assertEqual([(l["kind"], l["percent"]) for l in limits],
                         [("session", 12), ("weekly_all", 50)])
        self.assertEqual(accounts.parse_limits({"limits": [{"kind": "x"}, "junk", {"percent": 3}]}), [])

    def test_threshold_alerts_once_per_reset_period(self):
        self.do_login("alice")
        self.run_cli("capture")
        token = self.live_token()
        self.usage[token] = usage(("session", None, 85, 3600, True), ("weekly_all", None, 50, 86400, False))
        self.poll()
        self.poll()
        self.assertEqual(len(self.alerts()), 1)
        self.assertIn(f"{accounts.notify_tag()}|alice@example.com (alice-uu) is at 85% of its 5h limit",
                      self.alerts()[0])
        # A new 5-hour window is a new period: alert again.
        self.usage[token] = usage(("session", None, 81, 5 * 3600, True))
        self.poll()
        self.assertEqual(len(self.alerts()), 2)
        self.assert_no_tokens_on_disk()

    def test_thresholds_are_configurable(self):
        self.do_login("alice")
        self.run_cli("capture")
        state = self.state()
        state["config"] = {"thresholds": {"weekly": 85}}
        self.paths.state.write_text(json.dumps(state))
        self.usage[self.live_token()] = WEEKLY_ONLY
        self.poll()
        self.assertEqual(len(self.alerts()), 1)
        self.assertIn("89% of its week limit", self.alerts()[0])

    def test_expired_access_token_is_skipped_without_any_request(self):
        self.do_login("alice")
        self.run_cli("capture")
        creds = self.live()
        creds["claudeAiOauth"]["expiresAt"] = int(time.time() * 1000) - 1000
        self.paths.credentials.write_text(json.dumps(creds))
        before = self.paths.credentials.read_text()
        self.fetches.clear()
        code, text = self.poll()
        self.assertEqual(code, 0)
        self.assertIn("skipped", text)
        self.assertEqual((self.usage_calls, self.fetches), ([], []))
        self.assertEqual(self.paths.credentials.read_text(), before)  # never refreshed
        self.assertEqual(self.state()["poll"]["reason"], "access token expired")

    def test_rejected_token_is_a_skip_not_a_failure(self):
        self.do_login("alice")
        self.run_cli("capture")
        for _ in range(4):
            self.assertEqual(self.poll()[0], 0)
        self.assertNotIn("failures", self.state()["poll"])
        self.assertEqual(self.alerts(), [])

    def test_a_session_refresh_is_saved_so_switch_can_identify_it_later(self):
        self.capture_two()
        self.run_cli("switch", "alice@example.com")
        self.session_refresh("alice-2", owner="alice")
        self.usage[self.live_token()] = WEEKLY_ONLY
        self.fetches.clear()
        self.assertEqual(self.poll()[0], 0)
        # Confirmed with Anthropic outside Claude Code's lock, then saved.
        self.assertEqual(self.fetches, [[False, False]])
        saved = json.loads((self.paths.accounts / "alice-uuid-0000.json").read_text())
        self.assertEqual(saved["claudeAiOauth"]["refreshToken"], "sk-ant-ort-REFRESH-alice-2")
        # The token has since expired; switch identifies it by token, no network.
        creds = self.live()
        creds["claudeAiOauth"]["expiresAt"] = int(time.time() * 1000) - 1000
        self.paths.credentials.write_text(json.dumps(creds))
        saved["claudeAiOauth"]["expiresAt"] = creds["claudeAiOauth"]["expiresAt"]
        self.paths.accounts.joinpath("alice-uuid-0000.json").write_text(json.dumps(saved))
        code, text = self.run_cli("switch", "bob@example.com")
        self.assertEqual(code, 0, text)
        self.assertEqual(self.live()["claudeAiOauth"]["refreshToken"], "sk-ant-ort-REFRESH-bob")

    def test_poll_repairs_a_stale_profile(self):
        self.capture_two()
        self.run_cli("switch", "alice@example.com")
        cj = json.loads(self.paths.claude_json.read_text())
        cj["oauthAccount"] = profile("bob")  # a session wrote its old profile back
        self.paths.claude_json.write_text(json.dumps(cj))
        self.usage[self.live_token()] = WEEKLY_ONLY
        code, text = self.poll()
        self.assertIn("repaired the profile", text)
        self.assertEqual(self.live_email(), "alice@example.com")
        self.assertEqual(json.loads(self.paths.claude_json.read_text())["numStartups"], 7)

    def test_only_the_live_login_is_polled(self):
        self.capture_two()  # bob active; alice's saved token must never be used
        self.usage[self.live_token()] = WEEKLY_ONLY
        self.usage["sk-ant-oat-ACCESS-alice"] = WEEKLY_ONLY
        self.poll()
        self.assertEqual(self.usage_calls, ["sk-ant-oat-ACCESS-bob"])
        code, text = self.run_cli("status")
        self.assertIn("no usage reading yet (read only while active)", text)

    def test_inactive_account_whose_reset_passed_shows_likely_refilled(self):
        self.capture_two()
        self.run_cli("switch", "alice@example.com")
        self.usage[self.live_token()] = usage(("weekly_all", None, 95, 1, True))
        self.poll()
        self.run_cli("switch", "bob@example.com")
        time.sleep(1.1)
        code, text = self.run_cli("status")
        alice = text.split("alice@example.com", 1)[1].splitlines()[1]
        self.assertIn("week 95% -> likely refilled", alice)

    def test_repeated_failures_alert_once_then_recovery_alerts(self):
        self.do_login("alice")
        self.run_cli("capture")
        self.usage[self.live_token()] = WEEKLY_ONLY
        self.usage_error = accounts.AccountError("usage request failed (HTTP 500)")
        for _ in range(5):
            self.assertEqual(self.poll()[0], 1)
        self.assertEqual(len(self.alerts()), 1)
        self.assertIn("failed 3 times in a row: usage request failed (HTTP 500)", self.alerts()[0])
        self.assertIn("5 failures in a row", self.run_cli("status")[1])
        self.usage_error = None
        self.assertEqual(self.poll()[0], 0)
        self.assertIn("recovered after 5 failures", self.alerts()[1])

    def test_expired_saved_login_alerts_once(self):
        self.do_login("alice", tag="alice-old")
        self.run_cli("capture")
        saved = json.loads((self.paths.accounts / "alice-uuid-0000.json").read_text())
        saved["claudeAiOauth"]["refreshTokenExpiresAt"] = int(time.time() * 1000) - 1000
        self.paths.accounts.joinpath("alice-uuid-0000.json").write_text(json.dumps(saved))
        self.do_login("bob")
        self.run_cli("capture")
        self.usage[self.live_token()] = WEEKLY_ONLY
        self.poll()
        self.poll()
        self.assertEqual(len(self.alerts()), 1)
        self.assertIn("alice@example.com (alice-uu) needs a new login", self.alerts()[0])

    def test_new_login_is_saved_when_anthropic_confirms_its_profile(self):
        self.capture_two()
        self.do_login("carol")  # /login without `capture`
        self.usage[self.live_token()] = WEEKLY_ONLY
        self.assertEqual(self.poll()[0], 0)
        self.assertEqual(self.state()["active"], "carol-uuid-0000")
        self.assertTrue((self.paths.accounts / "carol-uuid-0000.json").exists())

    def test_unsaved_login_with_a_mismatched_profile_alerts_and_saves_nothing(self):
        self.capture_two()
        self.do_login("carol")
        cj = json.loads(self.paths.claude_json.read_text())
        cj["oauthAccount"] = profile("bob")
        self.paths.claude_json.write_text(json.dumps(cj))
        lock_held = []
        real_notify = accounts.notify

        def checking_notify(*a):
            lock_held.append(any(d.exists() for d in self.paths.refresh_locks))
            real_notify(*a)

        with mock.patch.object(accounts, "notify", checking_notify):
            self.assertEqual(self.poll()[0], 1)
            self.assertEqual(self.poll()[0], 1)
        self.assertEqual(lock_held, [False])  # never alerts inside Claude Code's lock
        self.assertFalse((self.paths.accounts / "carol-uuid-0000.json").exists())
        self.assertEqual(len(self.alerts()), 1)
        self.assertIn("account carol-uu, which is not saved", self.alerts()[0])

    def test_missing_notifier_logs_the_alert_and_status_says_so(self):
        self.do_login("alice")
        self.run_cli("capture")
        self.usage[self.live_token()] = usage(("session", None, 99, 600, True))
        self.notifier.unlink()
        self.assertEqual(self.poll()[0], 0)
        log = (self.paths.store / "alerts.log").read_text()
        self.assertIn("99% of its 5h limit", log)
        self.assertEqual(stat.S_IMODE((self.paths.store / "alerts.log").stat().st_mode), 0o600)
        self.assertIn("NOT delivered (notifier failed (FileNotFoundError))", self.run_cli("status")[1])
        self.assert_no_tokens_on_disk()

    def test_reset_time_jitter_does_not_repeat_an_alert(self):
        # Seen on the laptop 2026-11-09: 1794636000.02, then 1794635999.68.
        self.do_login("alice")
        self.run_cli("capture")
        token = self.live_token()
        for stamp in ("2030-11-14T06:00:00.020Z", "2030-11-14T05:59:59.680Z"):
            self.usage[token] = {"five_hour": None, "limits": [
                {"kind": "weekly_all", "group": "all", "scope": None, "percent": 96,
                 "is_active": True, "severity": "warning", "resets_at": stamp}]}
            self.poll()
        self.assertEqual(len(self.alerts()), 1)

    def test_model_scoped_limit_is_named_by_model(self):
        # weekly_scoped scope shape seen on the laptop 2026-11-09.
        self.do_login("alice")
        self.run_cli("capture")
        self.usage[self.live_token()] = {"five_hour": None, "limits": [
            {"kind": "weekly_scoped", "group": "model",
             "scope": {"model": {"id": None, "display_name": "Fable"}, "surface": None},
             "percent": 97, "is_active": False, "severity": "warning",
             "resets_at": iso_in(86400)}]}
        code, text = self.poll()
        self.assertIn("week:Fable 97%", text)
        self.assertIn("97% of its week:Fable limit", self.alerts()[0])
        self.assertIn("alice-uuid-0000:weekly_scoped:Fable:model", self.state()["limit_alerts"])
        self.assertNotIn("display_name", self.run_cli("status")[1])

    def test_undelivered_alert_is_retried_hourly_until_delivered(self):
        self.do_login("alice")
        self.run_cli("capture")
        self.usage[self.live_token()] = usage(("session", None, 90, 4 * 3600, True))
        hidden = self.notifier.with_suffix(".off")
        self.notifier.rename(hidden)
        log = self.paths.store / "alerts.log"
        self.poll()
        self.poll()
        self.assertEqual(len(log.read_text().splitlines()), 1)  # not every 5 minutes

        def age_alerts():
            state = self.state()
            for entry in state["limit_alerts"].values():
                entry["at"] -= 3601
            self.paths.state.write_text(json.dumps(state))

        age_alerts()
        self.poll()
        self.assertEqual(len(log.read_text().splitlines()), 2)
        self.assertEqual(self.alerts(), [])
        hidden.rename(self.notifier)
        age_alerts()
        self.poll()
        self.assertEqual(len(self.alerts()), 1)  # delivered now
        age_alerts()
        self.poll()
        self.assertEqual(len(self.alerts()), 1)  # and never again this period

    def test_rate_limit_backs_off_without_counting_a_failure(self):
        # Seen on the laptop 2026-11-09: HTTP 429 on the 7th call in ~30 min.
        self.do_login("alice")
        self.run_cli("capture")
        self.usage[self.live_token()] = WEEKLY_ONLY
        self.usage_error = accounts.RateLimited("usage requests are rate limited (HTTP 429)")
        code, text = self.poll()
        self.assertEqual(code, 0)
        self.assertIn("no usage request for 15 min", text)
        self.usage_error = None
        for _ in range(3):  # inside the backoff: no request at all
            self.assertIn("backing off", self.poll()[1])
        self.assertEqual(len(self.usage_calls), 1)
        poll = self.state()["poll"]
        self.assertNotIn("failures", poll)
        self.assertEqual(self.alerts(), [])
        # Back off longer each time, and honor a longer Retry-After.
        self.usage_error = accounts.RateLimited("usage requests are rate limited (HTTP 429)")
        self.expire_backoff()
        self.assertIn("for 30 min", self.poll()[1])
        self.usage_error = accounts.RateLimited("rate limited (HTTP 429)", retry_after=3 * 3600)
        self.expire_backoff()
        self.assertIn("for 180 min", self.poll()[1])
        self.usage_error = None
        self.expire_backoff()
        self.assertEqual(self.poll()[0], 0)
        self.assertEqual(self.state()["poll"], {"at": mock.ANY, "ok": True, "account": "alice-uuid-0000"})

    def expire_backoff(self):
        state = self.state()
        state["poll"]["backoff_until"] = int(time.time()) - 1
        self.paths.state.write_text(json.dumps(state))

    def test_rate_limited_profile_check_backs_off_too(self):
        self.do_login("alice")
        self.run_cli("capture")
        self.session_refresh("alice-2", owner="alice")
        with mock.patch.object(accounts, "fetch_account_uuid",
                               side_effect=accounts.RateLimited("profile requests are rate limited (HTTP 429)")):
            code, text = self.poll()
        self.assertEqual(code, 0)
        self.assertIn("backoff_until", self.state()["poll"])
        self.assertEqual(self.usage_calls, [])

    def test_retry_after_header_forms(self):
        self.assertEqual(accounts.parse_retry_after("120"), 120)
        self.assertIsNone(accounts.parse_retry_after(None))
        self.assertIsNone(accounts.parse_retry_after("soon"))
        future = email.utils.formatdate(time.time() + 600, usegmt=True)
        self.assertAlmostEqual(accounts.parse_retry_after(future), 600, delta=5)

    def test_failure_reason_reaches_the_journal(self):
        self.do_login("alice")
        self.run_cli("capture")
        self.usage_error = accounts.AccountError("usage request failed (HTTP 500)")
        code, text = self.poll()
        self.assertEqual(code, 1)
        self.assertIn("poll failed: usage request failed (HTTP 500)", text)

    def test_alerts_name_the_sending_host(self):
        with mock.patch.object(accounts.socket, "gethostname", return_value="pop-os.lan"):
            self.assertEqual(accounts.notify_tag(), "claude-accounts@pop-os")
            self.run_cli("test-alert", "--notify-command", str(self.notifier))
        self.assertEqual(self.alerts(), ["claude-accounts@pop-os|test alert from cm-claude-accounts"])

    def test_test_alert_reports_delivery(self):
        self.assertEqual(self.run_cli("test-alert", "--notify-command", str(self.notifier))[0], 0)
        self.assertEqual(self.alerts(), [f"{accounts.notify_tag()}|test alert from cm-claude-accounts"])
        self.notifier.unlink()
        code, text = self.run_cli("test-alert", "--notify-command", str(self.notifier))
        self.assertEqual(code, 1)
        self.assertIn("NOT delivered", text)

    def test_poll_waits_for_a_running_switch(self):
        self.capture_two()
        self.usage["sk-ant-oat-ACCESS-alice"] = WEEKLY_ONLY
        self.usage["sk-ant-oat-ACCESS-bob"] = WEEKLY_ONLY
        real_write = accounts.write_in_place
        results = {}

        def slow_write(path, data):
            real_write(path, data)
            poller = threading.Thread(target=lambda: results.setdefault("poll", self.poll()))
            poller.start()
            poller.join(0.5)
            results["blocked"] = poller.is_alive()  # the store lock holds it back
            results["thread"] = poller

        with mock.patch.object(accounts, "write_in_place", slow_write):
            self.assertEqual(self.run_cli("switch", "alice@example.com")[0], 0)
        results["thread"].join(5)
        self.assertTrue(results["blocked"])
        self.assertEqual(results["poll"][0], 0)
        self.assertEqual(self.usage_calls, ["sk-ant-oat-ACCESS-alice"])
        self.assertEqual(self.state()["usage"].keys(), {"alice-uuid-0000"})



class AutoswitchHarness(PollHarness):
    """alice (active, week 95%), bob and carol saved."""

    def setUp(self):
        super().setUp()
        self.capture_two()
        self.do_login("carol")
        self.run_cli("capture")
        self.run_cli("switch", "alice@example.com")
        self.usage[self.live_token()] = usage(("weekly_all", None, 95, 3 * 86400, True))

    def set_reading(self, name, *limits, age=3600, **config):
        state = self.state()
        state.setdefault("usage", {})[f"{name}-uuid-0000"] = {
            "at": int(time.time()) - age,
            "limits": accounts.parse_limits(usage(*limits))}
        if config:
            state["config"] = config
        self.paths.state.write_text(json.dumps(state))

    def set_config(self, **config):
        state = self.state()
        state["config"] = config
        self.paths.state.write_text(json.dumps(state))

    def age_last_switch(self, seconds):
        state = self.state()
        for h in state["history"]:
            if h["event"] == "switch":
                h["at"] -= seconds
        self.paths.state.write_text(json.dumps(state))

    def alerts_reset(self):
        state = self.state()
        state["limit_alerts"] = {}
        self.paths.state.write_text(json.dumps(state))


class AutoswitchTests(AutoswitchHarness):
    """On threshold: suggest the account with most room; auto switches behind a flag."""

    def test_suggests_the_account_with_most_room(self):
        self.set_reading("bob", ("weekly_all", None, 40, 2 * 86400, True))
        self.age_last_switch(31 * 60)  # so only the mode keeps it from switching
        code, text = self.poll()  # carol has no reading: ranks after bob
        self.assertEqual(code, 0)
        self.assertEqual(len(self.alerts()), 1)
        self.assertIn("95% of its week limit", self.alerts()[0])
        self.assertIn("Suggest: cm-claude-accounts switch bob@example.com (week 40% 1.0h ago)",
                      self.alerts()[0])
        self.assertEqual(self.live_token(), "sk-ant-oat-ACCESS-alice")  # suggest never switches
        self.assertIn("over threshold: week 95%. Suggest: cm-claude-accounts switch bob@example.com",
                      self.run_cli("status")[1])

    def test_a_passed_reset_counts_as_refilled(self):
        self.set_reading("bob", ("weekly_all", None, 99, -60, True))
        self.poll()
        self.assertIn("switch bob@example.com (likely refilled)", self.alerts()[0])

    def test_unread_account_is_suggested_when_known_ones_are_full(self):
        self.set_reading("bob", ("weekly_all", None, 97, 86400, True))
        self.poll()
        self.assertIn("switch carol@example.com (no usage reading yet)", self.alerts()[0])

    def test_no_room_anywhere_says_so(self):
        self.set_reading("bob", ("weekly_all", None, 97, 86400, True))
        self.set_reading("carol", ("session", None, 85, 3600, True))
        self.poll()
        self.assertIn("No other saved account has room", self.alerts()[0])

    def test_expired_logins_are_never_suggested(self):
        for name in ("bob", "carol"):
            path = self.paths.accounts / f"{name}-uuid-0000.json"
            rec = json.loads(path.read_text())
            rec["claudeAiOauth"]["refreshTokenExpiresAt"] = int(time.time() * 1000) - 1000
            path.write_text(json.dumps(rec))
        self.poll()
        self.assertTrue(any("No other saved account has room" in a for a in self.alerts()))

    def test_off_alerts_without_advice(self):
        self.set_config(autoswitch="off")
        self.poll()
        self.assertEqual(len(self.alerts()), 1)
        self.assertNotIn("Suggest", self.alerts()[0])

    def test_auto_switches_after_the_dwell_time(self):
        self.set_reading("bob", ("weekly_all", None, 40, 2 * 86400, True), autoswitch="auto")
        self.age_last_switch(31 * 60)
        code, text = self.poll()
        self.assertEqual(code, 0, text)
        self.assertEqual(self.live()["claudeAiOauth"]["refreshToken"], "sk-ant-ort-REFRESH-bob")
        self.assertEqual(self.live_email(), "bob@example.com")
        self.assertEqual(self.state()["active"], "bob-uuid-0000")
        self.assertEqual(self.state()["history"][-1]["reason"], "auto: week 95%")
        self.assertEqual(len(self.alerts()), 1)
        self.assertIn("switched alice@example.com (alice-uu) -> bob@example.com (bob-uuid) "
                      "automatically (week 95%; target week 40% 1.0h ago)", self.alerts()[0])

    def test_auto_waits_for_the_dwell_time(self):
        self.set_reading("bob", ("weekly_all", None, 40, 2 * 86400, True), autoswitch="auto")
        self.poll()  # the setUp switch to alice was moments ago
        self.assertEqual(self.live_token(), "sk-ant-oat-ACCESS-alice")
        self.assertIn("automatic switch waits until 30 min after the last switch", self.alerts()[0])

    def test_a_full_model_scoped_limit_alerts_but_does_not_switch(self):
        self.usage[self.live_token()] = usage(("weekly_all", None, 50, 3 * 86400, True),
                                              ("weekly_scoped", "Fable", 97, 3 * 86400, False))
        self.set_reading("bob", ("weekly_all", None, 40, 2 * 86400, True), autoswitch="auto")
        self.age_last_switch(31 * 60)
        self.poll()
        self.assertEqual(self.live_token(), "sk-ant-oat-ACCESS-alice")
        self.assertEqual(len(self.alerts()), 1)
        self.assertIn("97% of its week:Fable limit", self.alerts()[0])
        self.assertNotIn("Suggest", self.alerts()[0])
        status = self.run_cli("status")[1]
        self.assertIn("over threshold: week:Fable 97%\n", status)
        # Opting in makes a scoped limit count, for both target and trigger.
        self.set_config(autoswitch="auto", switch_on_scoped=True)
        self.alerts_reset()
        self.poll()
        self.assertEqual(self.live()["claudeAiOauth"]["refreshToken"], "sk-ant-ort-REFRESH-bob")

    def test_auto_without_a_target_only_alerts(self):
        self.set_reading("bob", ("weekly_all", None, 97, 86400, True), autoswitch="auto")
        self.set_reading("carol", ("weekly_all", None, 97, 86400, True))
        self.age_last_switch(31 * 60)
        self.poll()
        self.assertEqual(self.live_token(), "sk-ant-oat-ACCESS-alice")
        self.assertIn("No other saved account has room", self.alerts()[0])


def peer_export(active=None, **readings):
    """Another host's `export`: readings given as name=(age, *limits)."""
    now = int(time.time())
    accts = {}
    for name, (age, *limits) in readings.items():
        accts[f"{name}-uuid-0000"] = {
            "email": f"{name}@example.com", "login": "login valid 20d",
            "reading": {"at": now - age, "limits": accounts.parse_limits(usage(*limits))}}
    return {"version": 1, "host": "peerhost", "at": now,
            "active": f"{active}-uuid-0000" if active else None, "accounts": accts, "poll": {}}


class PeerTests(AutoswitchHarness):
    """Hosts share readings (never logins) to choose better targets."""

    def setUp(self):
        super().setUp()
        self.peer_data = {}
        self.peer_calls = []

        def fetch(host):
            self.peer_calls.append(host)
            data = self.peer_data.get(host)
            if isinstance(data, Exception):
                raise data
            if data is None:
                raise accounts.AccountError(f"{host}: export failed (exit 255)")
            return data

        self.enterContext(mock.patch.object(accounts, "fetch_peer", side_effect=fetch))

    def test_export_lists_readings_and_never_tokens(self):
        self.poll()
        code, text = self.run_cli("export")
        self.assertEqual(code, 0)
        data = json.loads(text)
        self.assertEqual(data["version"], 1)
        self.assertEqual(data["active"], "alice-uuid-0000")
        self.assertEqual(set(data["accounts"]), {"alice-uuid-0000", "bob-uuid-0000", "carol-uuid-0000"})
        self.assertEqual(data["accounts"]["alice-uuid-0000"]["reading"]["limits"][0]["percent"], 95)
        self.assertIsNone(data["accounts"]["bob-uuid-0000"]["reading"])
        for uuid, record in accounts.load_accounts(self.paths).items():
            self.assertEqual(data["accounts"][uuid]["login_fingerprint"], accounts.login_fingerprint(record))
            self.assertNotIn(record["claudeAiOauth"]["refreshToken"], text)
        self.assertNotIn("sk-ant", text)

    def test_export_does_not_wait_for_the_store_lock(self):
        fd = os.open(self.paths.store / "lock", os.O_CREAT | os.O_RDWR, 0o600)
        self.addCleanup(os.close, fd)
        accounts.fcntl.flock(fd, accounts.fcntl.LOCK_EX)
        result = {}
        t = threading.Thread(target=lambda: result.update(r=self.run_cli("export")))
        t.start()
        t.join(5)
        self.assertFalse(t.is_alive(), "export blocked on the store lock")
        self.assertEqual(result["r"][0], 0)

    def test_a_fresher_reading_from_a_peer_wins(self):
        self.set_reading("bob", ("weekly_all", None, 97, 86400, True), age=2 * 86400)
        self.set_config(peers=["cm-sessions"])
        self.peer_data["cm-sessions"] = peer_export(bob=(300, ("weekly_all", None, 30, 86400, True)))
        self.poll()
        self.assertIn("switch bob@example.com (week 30% 5m ago on cm-sessions)", self.alerts()[0])
        self.assertEqual(self.peer_calls, ["cm-sessions"])

    def test_peers_are_only_read_when_a_switch_is_being_considered(self):
        self.set_config(peers=["cm-sessions"])
        self.usage[self.live_token()] = usage(("weekly_all", None, 20, 3 * 86400, True))
        self.poll()
        self.assertEqual(self.peer_calls, [])

    def test_an_account_active_on_another_host_ranks_after_idle_ones(self):
        self.set_config(peers=["cm-sessions"])
        self.peer_data["cm-sessions"] = peer_export(
            active="bob", bob=(60, ("weekly_all", None, 10, 86400, True)),
            carol=(3600, ("weekly_all", None, 60, 86400, True)))
        self.poll()
        self.assertIn("switch carol@example.com (week 60% 1.0h ago on cm-sessions)", self.alerts()[0])
        # When the busy account is the only one with room, it is still suggested, flagged.
        self.peer_data["cm-sessions"] = peer_export(
            active="bob", bob=(60, ("weekly_all", None, 10, 86400, True)),
            carol=(60, ("weekly_all", None, 99, 86400, True)))
        self.alerts_reset()
        self.age_alerts_sent()
        self.poll()
        self.assertIn("switch bob@example.com (week 10% 1m ago on cm-sessions; active on cm-sessions)",
                      self.alerts()[-1])

    def age_alerts_sent(self):
        if self.sent.exists():
            self.sent.unlink()

    def test_unreachable_peer_does_not_break_poll_and_status_says_so(self):
        self.set_config(peers=["cm-sessions", "cm-manager"])
        self.peer_data["cm-sessions"] = peer_export(bob=(300, ("weekly_all", None, 30, 86400, True)))
        code, _ = self.poll()
        self.assertEqual(code, 0)
        self.assertIn("switch bob@example.com", self.alerts()[0])
        del self.peer_data["cm-sessions"]  # now unreachable: the last good copy is kept
        code, text = self.run_cli("status")
        self.assertEqual(code, 0)
        self.assertIn("peer cm-manager: export failed (exit 255)\n", text)
        self.assertIn("peer cm-sessions: export failed (exit 255); using its reading from", text)
        self.assertIn("week 30% (resets in 24.0h, binding)  [read 5m ago on cm-sessions]", text)

    def test_status_shows_peer_activity_and_peer_only_accounts(self):
        self.set_config(peers=["cm-sessions"])
        data = peer_export(active="bob", bob=(60, ("weekly_all", None, 10, 86400, True)),
                           dave=(60, ("weekly_all", None, 50, 86400, True)))
        self.peer_data["cm-sessions"] = data
        text = self.run_cli("status")[1]
        self.assertIn("bob@example.com (bob-uuid)", text)
        self.assertRegex(text, r"bob@example.com \(bob-uuid\).*; active on cm-sessions")
        self.assertIn("dave@example.com (dave-uui): saved on cm-sessions only, not here", text)
        self.assertIn("peer cm-sessions: active bob@example.com", text)
        self.assertEqual(self.run_cli("status", "--local")[0], 0)
        self.assertEqual(self.peer_calls, ["cm-sessions"])  # --local reads no peer

    def test_dropping_a_peer_forgets_its_readings(self):
        self.set_config(peers=["cm-sessions"])
        self.peer_data["cm-sessions"] = peer_export(bob=(60, ("weekly_all", None, 10, 86400, True)))
        self.run_cli("status")
        self.assertEqual(self.run_cli("config", "peers", "default")[0], 0)
        self.assertNotIn("cm-sessions", self.state().get("peers", {}))


class FetchPeerTests(unittest.TestCase):
    def run_with(self, returncode=0, stdout="", exc=None):
        calls = []

        def fake_run(argv, **kw):
            calls.append((argv, kw))
            if exc:
                raise exc
            return mock.Mock(returncode=returncode, stdout=stdout, stderr="secret-ish detail")

        with mock.patch.object(accounts.subprocess, "run", side_effect=fake_run):
            return calls, accounts.fetch_peer("cm-sessions")

    def test_reads_export_over_batch_ssh(self):
        calls, data = self.run_with(stdout=json.dumps(peer_export()))
        argv, kw = calls[0]
        self.assertEqual(argv[:5], ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=5"])
        self.assertEqual(argv[5:], ["cm-sessions", accounts.REMOTE_EXPORT])
        self.assertEqual(kw["stdin"], accounts.subprocess.DEVNULL)
        self.assertEqual(data["version"], 1)

    def test_failures_name_the_host_only(self):
        cases = [dict(returncode=255), dict(stdout="not json"), dict(stdout='{"version": 9}'),
                 dict(exc=accounts.subprocess.TimeoutExpired("ssh", 20))]
        for case in cases:
            with self.assertRaises(accounts.AccountError) as cm:
                self.run_with(**case)
            self.assertTrue(str(cm.exception).startswith("cm-sessions: "), cm.exception)
            self.assertNotIn("secret-ish", str(cm.exception))

    def test_option_like_host_names_are_refused(self):
        for bad in ("-oProxyCommand=sh", "a b", "", "x;y"):
            with self.assertRaises(accounts.AccountError):
                accounts.fetch_peer(bad)


class SafeguardTests(AutoswitchHarness):
    """auto never switches blind, honours exclusions, and caps switches per day."""

    def auto_ready(self, **config):
        self.set_config(autoswitch="auto", **config)
        self.age_last_switch(31 * 60)

    def test_auto_does_not_switch_to_an_unread_account_by_default(self):
        self.set_reading("bob", ("weekly_all", None, 97, 86400, True))
        self.auto_ready()
        self.poll()
        self.assertEqual(self.live_token(), "sk-ant-oat-ACCESS-alice")
        self.assertIn("Suggest: cm-claude-accounts switch carol@example.com (no usage reading yet); "
                      "automatic switch needs a usage reading for the target", self.alerts()[0])

    def test_auto_allow_unread_lets_it_switch_blind(self):
        self.set_reading("bob", ("weekly_all", None, 97, 86400, True))
        self.auto_ready(auto_allow_unread=True)
        self.poll()
        self.assertEqual(self.live()["claudeAiOauth"]["refreshToken"], "sk-ant-ort-REFRESH-carol")

    def test_excluded_accounts_are_never_targets(self):
        self.set_reading("bob", ("weekly_all", None, 10, 86400, True))
        self.set_reading("carol", ("weekly_all", None, 50, 86400, True))
        code, text = self.run_cli("exclude", "bob@example.com")
        self.assertEqual(code, 0)
        self.assertIn("will never be a switch target", text)
        self.poll()
        self.assertIn("switch carol@example.com", self.alerts()[0])
        self.assertRegex(self.run_cli("status")[1], r"bob@example.com.*; never a switch target")
        self.run_cli("include", "bob")
        self.assertNotIn("exclude", self.state()["config"])

    def test_excluding_everything_else_means_no_room(self):
        self.set_reading("bob", ("weekly_all", None, 10, 86400, True))
        self.run_cli("exclude", "bob@example.com")
        self.run_cli("exclude", "carol@example.com")
        self.auto_ready(exclude=self.state()["config"]["exclude"])
        self.poll()
        self.assertEqual(self.live_token(), "sk-ant-oat-ACCESS-alice")
        self.assertIn("No other saved account has room", self.alerts()[0])

    def test_automatic_switches_are_capped_per_day(self):
        self.set_reading("bob", ("weekly_all", None, 10, 86400, True))
        self.auto_ready(max_auto_switches_per_day=2)
        state = self.state()
        old = int(time.time()) - 3 * 3600
        state["history"] += [{"at": old, "event": "switch", "account": "x", "reason": "auto: week 95%"}
                             for _ in range(2)]
        self.paths.state.write_text(json.dumps(state))
        self.poll()
        self.assertEqual(self.live_token(), "sk-ant-oat-ACCESS-alice")
        self.assertIn("automatic switches paused: 2 in the last 24 h", self.alerts()[0])
        # Manual switches don't count, and day-old ones age out.
        state = self.state()
        for h in state["history"]:
            if h.get("reason", "").startswith("auto:"):
                h["at"] -= 86400
        state["history"].append({"at": int(time.time()) - 7200, "event": "switch", "account": "x"})
        state["limit_alerts"] = {}
        self.paths.state.write_text(json.dumps(state))
        self.poll()
        self.assertEqual(self.live()["claudeAiOauth"]["refreshToken"], "sk-ant-ort-REFRESH-bob")


class ConfigTests(AutoswitchHarness):
    def setUp(self):
        super().setUp()
        self.enterContext(mock.patch.object(accounts, "fetch_peer", side_effect=accounts.AccountError("offline test")))

    def test_show_set_and_reset(self):
        code, text = self.run_cli("config")
        self.assertEqual(code, 0)
        self.assertIn("autoswitch                 suggest (default)", text)
        self.assertIn("thresholds.weekly          90 (default)", text)
        self.assertEqual(self.run_cli("config", "autoswitch", "auto")[0], 0)
        self.assertEqual(self.run_cli("config", "min_dwell_minutes", "45")[0], 0)
        self.assertEqual(self.run_cli("config", "auto_allow_unread", "true")[0], 0)
        self.assertEqual(self.run_cli("config", "thresholds.weekly", "97")[0], 0)
        self.assertEqual(self.run_cli("config", "peers", "cm-sessions,cm-manager")[0], 0)
        cfg = self.state()["config"]
        self.assertEqual(cfg, {"autoswitch": "auto", "min_dwell_minutes": 45, "auto_allow_unread": True,
                               "thresholds": {"weekly": 97.0}, "peers": ["cm-sessions", "cm-manager"]})
        self.assertIn("auto_allow_unread          true", self.run_cli("config")[1])
        self.run_cli("config", "thresholds.weekly", "default")
        self.run_cli("config", "autoswitch", "default")
        cfg = self.state()["config"]
        self.assertNotIn("thresholds", cfg)
        self.assertNotIn("autoswitch", cfg)

    def test_invalid_values_change_nothing(self):
        for argv in (("autoswitch", "yes"), ("min_dwell_minutes", "5m"), ("thresholds.weekly", "150"),
                     ("peers", "a,-oProxyCommand=x"), ("bogus", "1"), ("thresholds.Bad-Kind", "50")):
            code, text = self.run_cli("config", *argv)
            self.assertEqual(code, 1, argv)
        self.assertEqual(self.state().get("config", {}), {})

    def test_threshold_setting_takes_effect(self):
        self.run_cli("config", "thresholds.weekly", "97")
        self.poll()  # alice is at 95%
        self.assertEqual(self.alerts(), [])

    def test_history_shows_switches_and_settings(self):
        self.run_cli("config", "autoswitch", "off")
        code, text = self.run_cli("history", "-n", "5")
        self.assertEqual(code, 0)
        self.assertIn("switch carol@example.com (carol-uu) -> alice@example.com (alice-uu)", text)
        self.assertIn("config autoswitch = off", text)
        self.assertRegex(text, r"\d{4}-\d\d-\d\d \d\d:\d\dZ  capture")


class InstallTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.home = Path(tmp.name)
        self.enterContext(mock.patch.dict(os.environ, {"HOME": str(self.home)}))
        self.calls = []
        self.enterContext(mock.patch.object(accounts, "systemctl",
                                            side_effect=lambda *a: self.calls.append(a)))

    def run_cli(self, *argv):
        out = io.StringIO()
        with contextlib.redirect_stderr(io.StringIO()):
            return accounts.main(list(argv), paths=accounts.Paths(
                self.home / ".claude", self.home / ".claude.json", self.home / "store"), out=out), out.getvalue()

    def test_installs_script_and_timer(self):
        code, text = self.run_cli("install")
        self.assertEqual(code, 0, text)
        target = self.home / ".local/bin/cm-claude-accounts"
        self.assertEqual(target.read_bytes(), SCRIPT.read_bytes())
        self.assertEqual(stat.S_IMODE(target.stat().st_mode), 0o755)
        for unit in accounts.UNITS:
            self.assertTrue((self.home / ".config/systemd/user" / unit).is_file())
        self.assertEqual(self.calls, [("daemon-reload",), ("enable", "--now", accounts.UNITS[1])])
        self.assertFalse((self.home / "store").exists())  # install touches no saved logins

    def test_no_timer_only_copies(self):
        code, _ = self.run_cli("install", "--no-timer")
        self.assertEqual(code, 0)
        self.assertEqual(self.calls, [])
        self.assertFalse((self.home / ".config/systemd").exists())

    def test_refuses_outside_a_checkout(self):
        with mock.patch.object(accounts, "__file__", str(self.home / "elsewhere/cm-claude-accounts")):
            code, _ = self.run_cli("install")
        self.assertEqual(code, 1)
        self.assertEqual(self.calls, [])


def host_export(host, active=None, saved=(), **readings):
    """Another host's check-in: saved accounts, readings as name=(age, *limits)."""
    now = int(time.time())
    accts = {}
    for name in set(saved) | set(readings) | ({active} if active else set()):
        reading = None
        if name in readings:
            age, *limits = readings[name]
            reading = {"at": now - age, "limits": accounts.parse_limits(usage(*limits))}
        accts[f"{name}-uuid-0000"] = {"email": f"{name}@example.com", "login": "login valid 20d",
                                      "reading": reading}
    return {"version": 1, "host": host, "at": now, "active": f"{active}-uuid-0000" if active else None,
            "accounts": accts, "mode": "suggest", "apply": None, "events": [], "sessions": None, "poll": {}}


class HubHarness(AutoswitchHarness):
    """This harness is the laptop ("laptop"); a second store is the hub on cm-manager."""

    def setUp(self):
        super().setUp()
        root = self.paths.store.parent
        self.hub_paths = accounts.Paths(root / "hub-home/.claude", root / "hub-home/.claude.json",
                                        root / "hub-home/store")
        self.hub_down = False
        self.checkins = []

        def remote(hub, export):
            self.checkins.append((hub, export))
            if self.hub_down:
                raise accounts.AccountError(f"{hub}: unreachable (TimeoutExpired)")
            with accounts.Hub(self.hub_paths) as h:
                return json.loads(json.dumps(accounts.hub_checkin(h, json.loads(json.dumps(export)))))

        self.enterContext(mock.patch.object(accounts, "remote_checkin", side_effect=remote))
        self.set_config(hub="cm-manager", host_name="laptop")

    def hub(self, *argv, stdin=None):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stderr(err), mock.patch.object(sys, "stdin", io.StringIO(stdin or "")):
            code = accounts.main(["hub", *argv], paths=self.hub_paths, out=out)
        text = out.getvalue() + err.getvalue()
        for token in self.tokens:
            self.assertNotIn(token, text)
        return code, text

    def other_host(self, export):
        with accounts.Hub(self.hub_paths) as h:
            accounts.hub_checkin(h, export)

    def hub_files_have_no_tokens(self):
        for path in self.hub_paths.store.rglob("*"):
            if path.is_file():
                text = path.read_text()
                for token in self.tokens:
                    self.assertNotIn(token, text, path)


class PoolTests(HubHarness):
    """Owner 2026-11-15: any account on any host. Logins live in the hub's pool
    and are lent to one host at a time."""

    def setUp(self):
        super().setUp()
        self.assertEqual(self.hub("policy", "pool", "on")[0], 0)
        self.age_last_switch(31 * 60)

    def pool_uuids(self):
        with accounts.Hub(self.hub_paths) as h:
            return sorted(h.pool())

    def saved_here(self):
        return sorted(p.stem for p in self.paths.accounts.glob("*.json"))

    def files_outside_pool_have_no_tokens(self):
        pool = self.hub_paths.store / "hub" / "pool"
        for path in self.hub_paths.store.rglob("*"):
            if path.is_file() and pool not in path.parents:
                text = path.read_text()
                for token in self.tokens:
                    self.assertNotIn(token, text, path)
        for path in pool.rglob("*.json"):
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
        self.assertEqual(stat.S_IMODE(pool.stat().st_mode), 0o700)

    def test_spares_go_to_the_pool_and_tokens_stay_out_of_reports(self):
        self.poll()  # learns the policy
        code, text = self.poll()  # hands spares back
        self.assertIn("returned bob@example.com (bob-uuid) to the pool", text)
        self.assertEqual(self.saved_here(), ["alice-uuid-0000"])
        self.assertEqual(self.pool_uuids(), ["bob-uuid-0000", "carol-uuid-0000"])
        self.assertIn("saved on pool", self.hub("status")[1])
        self.files_outside_pool_have_no_tokens()
        self.assert_no_tokens_on_disk()

    def test_a_just_used_account_is_kept_for_half_an_hour(self):
        state = self.state()
        for h in state["history"]:
            if h["event"] == "switch":
                h["at"] = int(time.time()) - 60  # alice became active a minute ago
        self.paths.state.write_text(json.dumps(state))
        self.poll()
        self.poll()
        previous = [h["previous"] for h in self.state()["history"] if h["event"] == "switch"][-1]
        self.assertIn(previous, self.saved_here())

    def test_auto_borrows_the_pool_account_with_most_room(self):
        self.set_reading("bob", ("weekly_all", None, 40, 2 * 86400, True), hub="cm-manager", host_name="laptop")
        self.set_reading("carol", ("weekly_all", None, 10, 86400, True), hub="cm-manager", host_name="laptop")
        self.poll()
        self.assertEqual(self.hub("assign", "laptop", "alice@example.com", "--auto")[0], 0)
        self.poll()  # spares returned; alice is at 95%, so carol is lent and switched to
        self.assertEqual(self.live()["claudeAiOauth"]["refreshToken"], "sk-ant-ort-REFRESH-carol")
        self.assertEqual(self.pool_uuids(), ["bob-uuid-0000"])
        self.assertTrue(self.state()["history"][-1]["reason"].startswith("auto (pool): week 95%"))
        self.poll()  # confirms the lend
        self.assertEqual(list((self.hub_paths.store / "hub/pool/lent").glob("*.json")), [])
        self.assertIn("alice-uuid-0000", self.saved_here())  # kept 30 min after use
        self.assertIn("lent carol@example.com to laptop", self.hub("history")[1])
        self.files_outside_pool_have_no_tokens()
        self.assert_no_tokens_on_disk()

    def test_pin_to_a_pool_account_borrows_it(self):
        self.poll()
        self.poll()
        code, text = self.hub("assign", "laptop", "bob@example.com")
        self.assertEqual(code, 0, text)
        code, text = self.poll()
        self.assertIn("borrowed bob@example.com", text)
        self.assertIn("assignment rev", text)
        self.assertEqual(self.live()["claudeAiOauth"]["refreshToken"], "sk-ant-ort-REFRESH-bob")

    def test_a_lend_the_host_never_saved_goes_back_to_the_pool(self):
        self.poll()
        self.poll()
        self.hub("assign", "laptop", "bob@example.com")
        with mock.patch.object(accounts, "save_account", side_effect=OSError("disk full")):
            self.poll()
        self.assertNotIn("bob-uuid-0000", self.pool_uuids())
        self.poll()  # not listed by the host: the lend is undone, then made again
        self.assertEqual(self.live()["claudeAiOauth"]["refreshToken"], "sk-ant-ort-REFRESH-bob")
        self.assertIn("not confirmed; back in the pool", self.hub("history")[1])

    def test_reservations_and_last_resort(self):
        self.set_reading("bob", ("weekly_all", None, 10, 86400, True), hub="cm-manager", host_name="laptop")
        self.set_reading("carol", ("weekly_all", None, 60, 86400, True), hub="cm-manager", host_name="laptop")
        self.poll()
        self.hub("assign", "laptop", "alice@example.com", "--auto")
        self.hub("reserve", "bob@example.com", "--for", "cm-manager", "--last-resort")
        self.hub("reserve", "carol@example.com", "--for", "cm-manager")
        with mock.patch.object(accounts, "pool_lend", return_value=None):
            self.poll()  # spares returned, nothing lent yet
        with accounts.Hub(self.hub_paths) as h:
            cands = accounts.pool_candidates(h, "laptop", h.reports(), time.time())
        self.assertEqual([c[1] for c in cands], ["bob-uuid-0000"])  # carol is not allowed at all
        self.assertTrue(cands[0][0][0])  # and bob only as a last resort
        self.assertIn("last resort; first reserved for cm-manager", self.hub("status")[1])

    def test_auto_never_lends_an_account_another_host_is_running_on(self):
        self.set_reading("carol", ("weekly_all", None, 10, 86400, True), hub="cm-manager", host_name="laptop")
        self.poll()
        self.poll()  # bob and carol in the pool
        self.other_host({**host_export("cm-sessions", active="bob", saved=["bob"]), "last_switch_at": 0})
        self.assertEqual(self.pool_uuids(), ["bob-uuid-0000", "carol-uuid-0000"])  # both logins kept
        with accounts.Hub(self.hub_paths) as h:
            cands = accounts.pool_candidates(h, "laptop", h.reports(), time.time())
        self.assertEqual([c[1] for c in cands], ["carol-uuid-0000"])

    def test_a_logged_out_host_with_saved_accounts_still_takes_its_pin(self):
        self.poll()
        creds = self.live()
        creds.pop("claudeAiOauth")
        self.paths.credentials.write_text(json.dumps(creds))
        self.hub("assign", "laptop", "bob@example.com")
        code, text = self.poll()
        self.assertEqual(code, 0, text)
        self.assertIn("switched no login -> bob@example.com", text)

    def test_old_hosts_get_no_login_and_keep_the_pool_intact(self):
        self.poll()
        self.poll()
        old = host_export("cm-sessions", active="dave", saved=["dave", "carol"])
        with accounts.Hub(self.hub_paths) as h:
            h.desired["hosts"]["cm-sessions"] = {"account": "bob-uuid-0000", "email": "bob@example.com",
                                                 "mode": "pin", "at": 0}
            h.save("pin")
            reply = accounts.hub_checkin(h, old)
        self.assertNotIn("lend", reply)
        self.assertEqual(self.pool_uuids(), ["bob-uuid-0000", "carol-uuid-0000"])

    def test_a_lost_reply_never_puts_one_login_on_two_hosts(self):
        self.poll()
        returning = accounts.spares_to_return(self.paths, self.state(), time.time())
        self.poll()  # bob and carol go to the pool...
        for uuid, rec in returning.items():  # ...but say the reply was lost: copies are back
            (self.paths.accounts / f"{uuid}.json").write_text(json.dumps(rec))
        sessions = {**host_export("cm-sessions", active="dave", saved=["dave"]), "last_switch_at": 0}
        with accounts.Hub(self.hub_paths) as h:
            h.desired["hosts"]["cm-sessions"] = {"account": "bob-uuid-0000", "email": "bob@example.com",
                                                 "mode": "pin", "at": 0}
            h.save("pin")
            self.assertIn("lend", accounts.hub_checkin(h, sessions))  # bob's login goes to cm-sessions
        code, text = self.poll()  # the laptop hands in its stale copy of that same login
        self.assertIn("deleted a stale copy of bob@example.com", text)
        self.assertNotIn("bob-uuid-0000", self.saved_here())
        self.assertNotIn("bob-uuid-0000", self.pool_uuids())
        self.assertIn("returned a login already lent to cm-sessions", self.hub("history")[1])
        self.files_outside_pool_have_no_tokens()

    def test_auto_never_moves_a_host_onto_an_unread_account(self):
        self.poll()
        self.hub("assign", "laptop", "alice@example.com", "--auto")
        code, text = self.poll()  # bob and carol go to the pool with no reading
        self.assertEqual(self.live_token(), "sk-ant-oat-ACCESS-alice")
        self.assertNotIn("borrowed", text)
        self.assertEqual(self.hub("policy", "auto_allow_unread", "true")[0], 0)
        self.poll()
        self.assertNotEqual(self.live_token(), "sk-ant-oat-ACCESS-alice")

    def prepare_auto_pool_with_scoped_reading(self):
        self.set_reading("bob", ("weekly_all", None, 10, 2 * 86400, True))
        self.set_reading("carol", ("weekly_all", None, 20, 3 * 86400, True))
        self.poll()
        self.poll()  # bob and carol go to the pool
        self.hub("policy", "thresholds.weekly", "98")
        self.hub("assign", "laptop", "alice@example.com", "--auto")
        self.set_reading("alice", ("weekly_scoped", "Fable", 98, 2 * 86400, False))

    def lend_for_current_export(self):
        export = json.loads(self.run_cli("export")[1])
        with accounts.Hub(self.hub_paths) as h:
            return accounts.pool_lend(h, "laptop", export, h.reports(), time.time())

    def test_auto_scoped_limit_lends_for_non_leftover_but_not_leftover(self):
        self.prepare_auto_pool_with_scoped_reading()
        self.assertEqual(self.hub("policy", "leftover_hosts", "laptop")[0], 0)
        self.assertIsNone(self.lend_for_current_export())
        self.assertEqual(self.hub("policy", "leftover_hosts", "default")[0], 0)
        lend = self.lend_for_current_export()
        self.assertIsNotNone(lend)
        self.assertTrue(lend["switch"])

    def test_auto_scoped_limit_can_be_disabled(self):
        self.prepare_auto_pool_with_scoped_reading()
        self.assertEqual(self.hub("policy", "switch_on_scoped", "false")[0], 0)
        self.assertIsNone(self.lend_for_current_export())

    def test_the_hub_reads_pool_usage_with_valid_tokens_only(self):
        self.poll()
        self.poll()  # bob and carol in the pool
        pool = self.hub_paths.store / "hub" / "pool"
        for path in pool.glob("*.json"):
            rec = json.loads(path.read_text())
            rec["claudeAiOauth"]["expiresAt"] = int(time.time() * 1000) + (
                3600_000 if "bob" in path.name else -1000)
            path.write_text(json.dumps(rec))
        self.usage["sk-ant-oat-ACCESS-bob"] = usage(("weekly_all", None, 20, 86400, True))
        out = io.StringIO()
        accounts.read_pool_usage(self.hub_paths, out)
        self.assertIn("pool usage bob@example.com: week 20%", out.getvalue())
        self.assertNotIn("pool usage carol", out.getvalue())  # expired access token: never used to read
        cache = json.loads((self.hub_paths.store / "hub" / "readings.json").read_text())
        self.assertEqual(cache["bob-uuid-0000"]["host"], "pool")

    def test_a_new_login_is_noticed_during_a_usage_backoff(self):
        self.poll()
        state = self.state()
        state.setdefault("poll", {})["backoff_until"] = int(time.time()) + 900
        self.paths.state.write_text(json.dumps(state))
        self.do_login("carol")  # someone runs /login on the host
        code, text = self.poll()
        self.assertIn("backing off", text)
        self.assertEqual(self.state()["active"], "carol-uuid-0000")

    def candidates(self):
        with accounts.Hub(self.hub_paths) as h:
            return [c[1] for c in accounts.pool_candidates(h, "laptop", h.reports(), time.time())]

    def test_the_account_resetting_soonest_goes_first(self):
        # Owner 11-15: use every account to 99%; room left at a reset is lost.
        self.set_reading("bob", ("weekly_all", None, 40, 5 * 86400, True), hub="cm-manager", host_name="laptop")
        self.set_reading("carol", ("weekly_all", None, 80, 86400, True), hub="cm-manager", host_name="laptop")
        self.poll()
        self.poll()
        self.assertEqual(self.candidates(), ["carol-uuid-0000", "bob-uuid-0000"])

    def test_switch_point_is_99_percent_and_old_readings_do_not_count(self):
        self.hub("policy", "thresholds.weekly", "99")
        self.set_reading("bob", ("weekly_all", None, 98, 86400, True), hub="cm-manager", host_name="laptop")
        self.set_reading("carol", ("weekly_all", None, 10, 2 * 86400, True), age=4 * 3600,
                         hub="cm-manager", host_name="laptop")
        self.poll()
        self.poll()
        self.assertEqual(self.candidates(), ["bob-uuid-0000"])  # 1-2% left still counts; carol's number is 4h old

    def test_low_room_account_ranks_after_a_fresh_account(self):
        self.hub("policy", "thresholds.weekly", "98")
        self.set_reading("bob", ("weekly_all", None, 97, 1.4 * 86400, True),
                         hub="cm-manager", host_name="laptop")
        self.set_reading("carol", ("weekly_all", None, 0, 6.4 * 86400, True),
                         hub="cm-manager", host_name="laptop")
        self.poll()
        self.poll()
        self.assertEqual(self.candidates(), ["carol-uuid-0000", "bob-uuid-0000"])

    def test_pool_candidates_exclude_a_full_scoped_limit(self):
        self.hub("policy", "thresholds.weekly", "98")
        self.set_reading("bob", ("weekly_scoped", "Fable", 98, 2 * 86400, False),
                         hub="cm-manager", host_name="laptop")
        self.set_reading("carol", ("weekly_all", None, 10, 3 * 86400, True),
                         hub="cm-manager", host_name="laptop")
        self.poll()
        self.poll()
        self.assertEqual(self.candidates(), ["carol-uuid-0000"])

    def test_account_threshold_excludes_only_that_pool_account(self):
        self.hub("policy", "thresholds.weekly", "98")
        self.set_reading("bob", ("weekly_all", None, 95, 2 * 86400, True))
        self.set_reading("carol", ("weekly_all", None, 95, 3 * 86400, True))
        self.poll()
        self.poll()
        self.hub("policy", "account_thresholds.bob@example.com", "94")
        self.assertEqual(self.candidates(), ["carol-uuid-0000"])

    def test_account_threshold_triggers_auto_lend_but_global_threshold_does_not(self):
        self.hub("policy", "thresholds.weekly", "98")
        self.set_reading("bob", ("weekly_all", None, 10, 2 * 86400, True))
        self.set_reading("carol", ("weekly_all", None, 20, 3 * 86400, True))
        self.poll()
        self.poll()
        self.hub("assign", "laptop", "alice@example.com", "--auto")
        self.hub("policy", "account_thresholds.alice@example.com", "94")
        export = json.loads(self.run_cli("export")[1])
        export["accounts"]["alice-uuid-0000"]["reading"]["limits"][0]["percent"] = 94.5
        with accounts.Hub(self.hub_paths) as h:
            lend = accounts.pool_lend(h, "laptop", export, h.reports(), time.time())
        self.assertIsNotNone(lend)
        self.assertTrue(lend["switch"])

        self.poll()  # return the borrowed login before checking the unmodified policy
        with accounts.Hub(self.hub_paths) as h:
            h.desired["policy"].pop("account_thresholds", None)
            h.save("remove test account threshold")
            lend = accounts.pool_lend(h, "laptop", export, h.reports(), time.time())
        self.assertIsNone(lend)

    def test_account_threshold_applies_to_scoped_weekly_limits(self):
        self.hub("policy", "thresholds.weekly", "98")
        self.hub("policy", "switch_on_scoped", "true")
        self.set_reading("bob", ("weekly_scoped", "Fable", 95, 2 * 86400, False))
        self.set_reading("carol", ("weekly_all", None, 10, 3 * 86400, True))
        self.poll()
        self.poll()
        self.hub("policy", "account_thresholds.bob@example.com", "94")
        self.assertEqual(self.candidates(), ["carol-uuid-0000"])

    def test_account_threshold_does_not_change_the_session_limit(self):
        self.hub("policy", "thresholds.session", "98")
        self.hub("policy", "thresholds.weekly", "98")
        self.set_reading("bob", ("session", None, 96, 5 * 3600, True),
                         ("weekly_all", None, 10, 2 * 86400, True))
        self.set_reading("carol", ("weekly_all", None, 20, 3 * 86400, True))
        self.poll()
        self.poll()
        self.hub("policy", "account_thresholds.bob@example.com", "94")
        self.assertIn("bob-uuid-0000", self.candidates())

    def test_room_by_account_uses_account_threshold(self):
        self.hub("policy", "thresholds.weekly", "98")
        self.hub("policy", "account_thresholds.bob@example.com", "94")
        self.other_host(host_export("cm-sessions", active="bob", saved=["bob"],
                                    bob=(0, ("weekly_all", None, 95, 86400, True))))
        with accounts.Hub(self.hub_paths) as h:
            room = accounts.room_by_account(h.reports(), h.desired["policy"], time.time())
        self.assertFalse(room["bob-uuid-0000"][0])

    def test_the_hub_renews_pool_logins_before_they_expire(self):
        self.poll()
        self.poll()
        pool = self.hub_paths.store / "hub" / "pool"
        for path in pool.glob("*.json"):
            rec = json.loads(path.read_text())
            rec["claudeAiOauth"]["expiresAt"] = int(time.time() * 1000) + 60_000  # expires in a minute
            path.write_text(json.dumps(rec))
        self.tokens.update(["sk-ant-oat-RENEWED-bob", "sk-ant-ort-RENEWED-bob"])

        def renew(oauth):
            if "bob" not in oauth["refreshToken"]:
                raise accounts.TokenRejected("the login was refused (HTTP 400); it needs a new /login")
            return {**oauth, "accessToken": "sk-ant-oat-RENEWED-bob", "refreshToken": "sk-ant-ort-RENEWED-bob",
                    "expiresAt": int(time.time() * 1000) + 8 * 3600_000}

        self.usage["sk-ant-oat-RENEWED-bob"] = usage(("weekly_all", None, 30, 86400, True))
        out = io.StringIO()
        with mock.patch.object(accounts, "refresh_login", side_effect=renew):
            accounts.read_pool_usage(self.hub_paths, out)
        self.assertIn("pool login renewed: bob@example.com", out.getvalue())
        self.assertIn("pool usage bob@example.com: week 30%", out.getvalue())
        with accounts.Hub(self.hub_paths) as h:
            pool_now = h.pool()
        self.assertEqual(pool_now["bob-uuid-0000"]["claudeAiOauth"]["refreshToken"], "sk-ant-ort-RENEWED-bob")
        self.assertTrue(pool_now["carol-uuid-0000"].get("needs_login"))  # refused: never lent
        self.assertEqual(list((pool / "renewing").glob("*")), [])
        self.assertIn("carol@example.com: the login was refused", self.hub("history")[1])
        self.files_outside_pool_have_no_tokens()

    def test_renewals_stop_after_a_failure_that_is_not_a_refusal(self):
        self.poll()
        self.poll()
        pool = self.hub_paths.store / "hub" / "pool"
        for path in pool.glob("*.json"):
            rec = json.loads(path.read_text())
            rec["claudeAiOauth"]["expiresAt"] = int(time.time() * 1000) + 60_000
            path.write_text(json.dumps(rec))
        calls = []

        def renew(oauth):
            calls.append(oauth["refreshToken"])
            raise accounts.AccountError("token refresh response was not understood")

        with mock.patch.object(accounts, "refresh_login", side_effect=renew):
            accounts.read_pool_usage(self.hub_paths, io.StringIO())
            accounts.read_pool_usage(self.hub_paths, io.StringIO())
        self.assertEqual(len(calls), 2)  # the first run's two, then paused
        self.assertIn("pool renewals paused 6 h", self.hub("history")[1])
        self.assertEqual(self.pool_uuids(), ["bob-uuid-0000", "carol-uuid-0000"])  # both back, not lost

    def test_a_reserved_account_is_the_hosts_next_and_others_hand_it_over(self):
        self.set_reading("bob", ("weekly_all", None, 10, 2 * 86400, True), hub="cm-manager", host_name="laptop")
        self.set_reading("carol", ("weekly_all", None, 50, 86400, True), hub="cm-manager", host_name="laptop")
        self.usage[self.live_token()] = usage(("weekly_all", None, 50, 3 * 86400, True))  # alice has room
        self.poll()
        self.poll()  # bob and carol in the pool
        self.hub("reserve", "bob@example.com", "--for", "laptop")
        self.assertEqual(self.candidates(), ["bob-uuid-0000", "carol-uuid-0000"])  # mine first, despite the reset
        self.hub("release", "bob@example.com")
        self.hub("reserve", "alice@example.com", "--for", "cm-sessions")
        self.hub("assign", "laptop", "alice@example.com", "--auto", "--force")
        self.poll()  # alice is reserved for another host: the laptop moves off it with room left
        self.assertEqual(self.live()["claudeAiOauth"]["refreshToken"], "sk-ant-ort-REFRESH-carol")
        self.assertIn("reserved for cm-sessions", self.state()["history"][-1]["reason"])

    def test_a_leftovers_host_takes_the_least_room_above_five_percent(self):
        self.set_reading("bob", ("weekly_all", None, 30, 2 * 86400, True), hub="cm-manager", host_name="laptop")
        self.set_reading("carol", ("weekly_all", None, 92, 4 * 86400, True), hub="cm-manager", host_name="laptop")
        self.usage[self.live_token()] = usage(("weekly_all", None, 50, 3 * 86400, True))
        self.poll()
        self.poll()  # bob and carol in the pool
        self.hub("policy", "thresholds.weekly", "99")
        self.assertEqual(self.candidates(), ["bob-uuid-0000", "carol-uuid-0000"])  # soonest reset
        self.hub("policy", "leftover_hosts", "laptop")
        self.assertEqual(self.candidates(), ["carol-uuid-0000", "bob-uuid-0000"])  # 7% left before 69%
        self.hub("policy", "leftover_room", "10")
        self.assertEqual(self.candidates(), ["bob-uuid-0000", "carol-uuid-0000"])  # carol is under 10%

    def test_a_blank_host_borrows_its_first_login(self):
        self.poll()
        self.poll()
        for p in self.paths.accounts.glob("*.json"):
            p.unlink()
        creds = self.live()
        creds.pop("claudeAiOauth")
        self.paths.credentials.write_text(json.dumps(creds))
        self.hub("assign", "laptop", "carol@example.com", "--force")
        code, text = self.poll()
        self.assertIn("switched no login -> carol@example.com", text)


class LendCopyTests(HubHarness):
    def setUp(self):
        super().setUp()
        self.assertEqual(self.hub("policy", "pool", "on")[0], 0)
        self.assertEqual(self.hub("policy", "lend_caps.bob@example.com", "week=80,five_hour=60")[0], 0)
        accounts.init_store(self.hub_paths)
        state = {"config": {"host_name": "cm-manager"}}
        accounts.atomic_write(self.hub_paths.state, state)
        self.primary = json.loads((self.paths.accounts / "bob-uuid-0000.json").read_text())
        accounts.save_account(self.hub_paths, self.primary)
        self.folder = self.paths.store.parent / "lend-incoming"
        (self.folder / "accounts").mkdir(parents=True, mode=0o700)
        self.copy = {"oauthAccount": profile("bob"), "claudeAiOauth": login("bob-copy")}
        self.tokens.update(self.copy["claudeAiOauth"][k] for k in ("accessToken", "refreshToken"))
        self.anthropic[self.copy["claudeAiOauth"]["accessToken"]] = "bob-uuid-0000"
        self.write_copy(self.copy)
        self.manager_reading(20, 30)
        self.assertEqual(self.hub("reserve", "bob@example.com", "--for", "cm-manager")[0], 0)

    def write_copy(self, record):
        accounts.atomic_write(self.folder / "accounts" / "bob-uuid-0000.json", record)

    def manager_reading(self, week, session):
        self.other_host(host_export("cm-manager", active="bob", saved=["bob"],
            bob=(0, ("weekly_all", None, week, 86400, True), ("session", None, session, 3600, True))))

    def import_copy(self):
        code, text = self.hub("import-lend-copy", "bob@example.com", str(self.folder))
        self.assertEqual(code, 0, text)

    def borrower_export(self, active="alice", saved=(), week=20, session=30):
        readings = {"alice": (0, ("weekly_all", None, 99, 86400, True))} if active == "alice" else {}
        if active == "bob":
            readings["bob"] = (0, ("weekly_all", None, week, 86400, True),
                               ("session", None, session, 3600, True))
        exp = host_export("borrower", active=active, saved=set(saved) | {"alice"}, **readings)
        exp["last_switch_at"] = 0
        return exp

    def lend(self, exp):
        with accounts.Hub(self.hub_paths) as h:
            h.desired["hosts"]["borrower"] = {"mode": "auto"}
            h.save()
            return accounts.hub_checkin(h, exp)

    def test_import_validation_permissions_and_status(self):
        self.write_copy(self.primary)
        self.assertEqual(self.hub("import-lend-copy", "bob@example.com", str(self.folder))[0], 1)
        self.write_copy(self.copy)
        self.anthropic[self.copy["claudeAiOauth"]["accessToken"]] = "other-uuid"
        self.assertEqual(self.hub("import-lend-copy", "bob@example.com", str(self.folder))[0], 1)
        self.anthropic[self.copy["claudeAiOauth"]["accessToken"]] = "bob-uuid-0000"
        self.assertEqual(self.hub("import-lend-copy", "unknown@example.com", str(self.folder))[0], 1)
        self.import_copy()
        self.assertEqual(self.hub("import-lend-copy", "bob@example.com", str(self.folder))[0], 1)
        with accounts.Hub(self.hub_paths) as h:
            self.assertEqual(stat.S_IMODE(h.copy_path("bob-uuid-0000").stat().st_mode), 0o600)
            self.assertEqual(stat.S_IMODE(h.copy_path("bob-uuid-0000").parent.stat().st_mode), 0o700)
        view = json.loads(self.hub("status", "--json")[1])["accounts"]["bob-uuid-0000"]
        self.assertEqual(view["lend_copy"]["caps"], {"week": 80, "five_hour": 60})
        self.assertTrue(view["lend_copy"]["available"])
        self.assertIn("lend copy", self.hub("status")[1])
        self.assertEqual(json.loads((self.hub_paths.accounts / "bob-uuid-0000.json").read_text()), self.primary)

    def remote_holder(self, fingerprint=None, week=20, session=30, age=0):
        exp = host_export("holder", active="bob", saved=["bob"],
                          bob=(age, ("weekly_all", None, week, 86400, True),
                               ("session", None, session, 3600, True)))
        if fingerprint is not None:
            exp["accounts"]["bob-uuid-0000"]["login_fingerprint"] = fingerprint
        return exp

    def use_remote_holder(self, fingerprint=None):
        (self.hub_paths.accounts / "bob-uuid-0000.json").unlink()
        with accounts.Hub(self.hub_paths) as h:
            h.desired["reservations"].pop("bob-uuid-0000", None)
            h.save()
            (h.dir / "reports/cm-manager.json").unlink()
        self.other_host(self.remote_holder(fingerprint))

    def test_import_remote_holder_requires_distinct_reported_fingerprint(self):
        self.use_remote_holder()
        code, message = self.hub("import-lend-copy", "bob@example.com", str(self.folder))
        self.assertEqual(code, 1)
        self.assertIn("missing login fingerprint from holder", message)
        self.assertIn("update the host or wait for its next check-in", message)
        self.other_host(self.remote_holder(accounts.login_fingerprint(self.copy)))
        code, message = self.hub("import-lend-copy", "bob@example.com", str(self.folder))
        self.assertEqual(code, 1)
        self.assertIn("login matches an account held on holder", message)
        self.other_host(self.remote_holder(accounts.login_fingerprint(self.primary)))
        self.import_copy()
        with accounts.Hub(self.hub_paths) as h:
            self.assertEqual(h.copies()["bob-uuid-0000"]["fingerprint"], accounts.login_fingerprint(self.copy))

    def test_remote_active_holder_copy_lending_and_shared_caps(self):
        self.use_remote_holder(accounts.login_fingerprint(self.primary))
        self.import_copy()
        with accounts.Hub(self.hub_paths) as h:
            self.assertTrue(any(c[1] == "bob-uuid-0000~copy" for c in
                                accounts.pool_candidates(h, "borrower", h.reports(), time.time())))
            self.assertFalse(any(c[1] == "bob-uuid-0000~copy" for c in
                                 accounts.pool_candidates(h, "holder", h.reports(), time.time())))
        self.assertIn("lend", self.lend(self.borrower_export()))
        # A newer borrower reading cannot conceal the holder's recent cap.
        holder = self.remote_holder(accounts.login_fingerprint(self.primary), week=80, age=1)
        self.other_host(holder)
        borrower = self.borrower_export("bob", ["bob"], week=20)
        reply = self.lend(borrower)
        self.assertEqual(reply["entry"]["account"], "alice-uuid-0000")
        self.assertNotIn("lend", reply)
        with accounts.Hub(self.hub_paths) as h:
            self.assertTrue(h.copies()["bob-uuid-0000"]["evicting"])
            self.assertIn("blocked_at", h.copies()["bob-uuid-0000"])
        returned = self.borrower_export("alice", ["bob"])
        returned["returning"] = {"bob-uuid-0000": self.copy}
        self.assertIn("bob-uuid-0000", self.lend(returned)["returned"])
        with accounts.Hub(self.hub_paths) as h:
            blocked = h.copies()["bob-uuid-0000"]["blocked_at"]
            self.assertFalse(any(c[1] == "bob-uuid-0000~copy" for c in
                                 accounts.pool_candidates(h, "borrower", h.reports(), blocked)))
        holder = self.remote_holder(accounts.login_fingerprint(self.primary))
        holder["accounts"]["bob-uuid-0000"]["reading"]["at"] = blocked + 1
        self.other_host(holder)
        with accounts.Hub(self.hub_paths) as h:
            self.assertTrue(any(c[1] == "bob-uuid-0000~copy" for c in
                                accounts.pool_candidates(h, "borrower", h.reports(), blocked + 1)))

    def test_weekly_only_copy_lends_and_evicts_at_week_cap(self):
        self.use_remote_holder(accounts.login_fingerprint(self.primary))
        self.import_copy()
        holder = self.remote_holder(accounts.login_fingerprint(self.primary))
        holder["accounts"]["bob-uuid-0000"]["reading"]["limits"] = accounts.parse_limits(
            usage(("weekly_all", None, 20, 86400, True),
                  ("weekly_scoped", "opus", 30, 86400, False)))
        self.other_host(holder)
        reply = self.lend(self.borrower_export())
        self.assertEqual(reply["lend"]["record"]["oauthAccount"]["accountUuid"], "bob-uuid-0000")
        borrower = self.borrower_export("bob", ["bob"])
        reading = borrower["accounts"]["bob-uuid-0000"]["reading"]
        reading["limits"] = accounts.parse_limits(usage(("weekly_all", None, 79, 86400, True),
                                                       ("weekly_scoped", "opus", 30, 86400, False)))
        reply = self.lend(borrower)
        self.assertNotIn("lend", reply)
        self.assertNotEqual((reply.get("entry") or {}).get("account"), "alice-uuid-0000")
        reading["limits"] = accounts.parse_limits(usage(("weekly_all", None, 80, 86400, True),
                                                       ("weekly_scoped", "opus", 30, 86400, False)))
        reply = self.lend(borrower)
        self.assertEqual(reply["entry"]["account"], "alice-uuid-0000")
        with accounts.Hub(self.hub_paths) as h:
            self.assertTrue(h.copies()["bob-uuid-0000"]["evicting"])

    def test_copy_without_weekly_reading_is_never_lent(self):
        self.use_remote_holder(accounts.login_fingerprint(self.primary))
        self.import_copy()
        holder = self.remote_holder(accounts.login_fingerprint(self.primary))
        reading = holder["accounts"]["bob-uuid-0000"]["reading"]
        reading["limits"] = accounts.parse_limits(usage(("weekly_scoped", "opus", 20, 86400, False)))
        self.other_host(holder)
        # Replace the cached earlier complete reading with the current one.
        with accounts.Hub(self.hub_paths) as h:
            accounts.atomic_write(h.dir / "readings.json", {"bob-uuid-0000": reading})
            self.assertFalse(any(c[1] == "bob-uuid-0000~copy" for c in
                                 accounts.pool_candidates(h, "borrower", h.reports(), time.time())))
        self.assertNotIn("lend", self.lend(self.borrower_export()))

    def test_borrower_without_weekly_reading_is_evicted(self):
        self.use_remote_holder(accounts.login_fingerprint(self.primary))
        self.import_copy()
        self.assertIn("lend", self.lend(self.borrower_export()))
        borrower = self.borrower_export("bob", ["bob"])
        borrower["accounts"]["bob-uuid-0000"]["reading"]["limits"] = accounts.parse_limits(
            usage(("weekly_scoped", "opus", 20, 86400, False)))
        reply = self.lend(borrower)
        self.assertEqual(reply["entry"]["account"], "alice-uuid-0000")

    def test_holder_fingerprint_conflict_blocks_copy_and_records_event(self):
        self.use_remote_holder(accounts.login_fingerprint(self.primary))
        self.import_copy()
        self.other_host(self.remote_holder(accounts.login_fingerprint(self.copy)))
        with accounts.Hub(self.hub_paths) as h:
            self.assertFalse(any(c[1] == "bob-uuid-0000~copy" for c in
                                 accounts.pool_candidates(h, "borrower", h.reports(), time.time())))
            events = [e for e in h.events() if "conflicts with login held on holder" in e.get("text", "")]
            self.assertEqual(len(events), 1)
        self.other_host(self.remote_holder(accounts.login_fingerprint(self.copy)))
        with accounts.Hub(self.hub_paths) as h:
            self.assertEqual(len([e for e in h.events() if "conflicts with login held on holder" in e.get("text", "")]), 1)
        self.other_host(self.remote_holder(accounts.login_fingerprint(self.primary)))
        with accounts.Hub(self.hub_paths) as h:
            self.assertTrue(any(c[1] == "bob-uuid-0000~copy" for c in
                                accounts.pool_candidates(h, "borrower", h.reports(), time.time())))
        self.hub_files_have_no_tokens_except_copy()

    def test_conflict_detects_borrower_rotation_and_survives_return(self):
        self.use_remote_holder(accounts.login_fingerprint(self.primary))
        self.import_copy()
        self.assertIn("lend", self.lend(self.borrower_export()))
        rotated = {**self.copy, "claudeAiOauth": {**self.copy["claudeAiOauth"],
                   "refreshToken": "new-copy-refresh-token"}}
        self.tokens.add(rotated["claudeAiOauth"]["refreshToken"])
        exp = self.borrower_export("bob", ["bob"])
        exp["accounts"]["bob-uuid-0000"]["login_fingerprint"] = accounts.login_fingerprint(rotated)
        self.lend(exp)
        self.other_host(self.remote_holder(accounts.login_fingerprint(rotated)))
        exp = self.borrower_export("alice", ["bob"])
        exp["returning"] = {"bob-uuid-0000": rotated}
        self.assertIn("bob-uuid-0000", self.lend(exp)["returned"])
        with accounts.Hub(self.hub_paths) as h:
            self.assertFalse(any(c[1] == "bob-uuid-0000~copy" for c in
                                 accounts.pool_candidates(h, "another", h.reports(), time.time())))
            self.assertEqual(len([e for e in h.events() if "conflicts with login held on holder" in e.get("text", "")]), 1)
        self.hub_files_have_no_tokens_except_copy()

    def hub_files_have_no_tokens_except_copy(self):
        with accounts.Hub(self.hub_paths) as h:
            for path in h.dir.rglob("*"):
                if path.is_file() and path != h.copy_path("bob-uuid-0000"):
                    for token in self.tokens:
                        self.assertNotIn(token, path.read_text(), path)

    def test_order_and_reserved_host_is_untouched(self):
        self.import_copy()
        carol = json.loads((self.paths.accounts / "carol-uuid-0000.json").read_text())
        with accounts.Hub(self.hub_paths) as h:
            h.pool_put(carol)
            h.desired["reservations"]["carol-uuid-0000"] = {"for": ["other"], "last_resort": True}
            accounts.atomic_write(h.dir / "readings.json", {"carol-uuid-0000": {"at": int(time.time()),
                "limits": accounts.parse_limits(usage(("weekly_all", None, 20, 86400, True)))}})
            candidates = accounts.pool_candidates(h, "borrower", h.reports(), time.time())
            self.assertEqual([c[1] for c in candidates], ["carol-uuid-0000", "bob-uuid-0000~copy"])
            self.assertFalse(any(c[1].endswith("~copy") for c in accounts.pool_candidates(h, "cm-manager", h.reports(), time.time())))
        self.assertEqual(json.loads((self.hub_paths.accounts / "bob-uuid-0000.json").read_text()), self.primary)

    def test_caps_switch_and_return_to_copy_slot(self):
        for week, session in ((80, 30), (20, 60)):
            with self.subTest(week=week, session=session):
                if not (self.hub_paths.store / "hub/lend-copies.json").exists():
                    self.import_copy()
                first = self.lend(self.borrower_export())
                self.assertEqual(first["lend"]["record"]["claudeAiOauth"]["refreshToken"], self.copy["claudeAiOauth"]["refreshToken"])
                with accounts.Hub(self.hub_paths) as h:
                    self.assertFalse(any(c[1].endswith("~copy") for c in
                                         accounts.pool_candidates(h, "another", h.reports(), time.time())))
                    view = accounts.hub_status_view(h, time.time())
                    self.assertEqual(view["accounts"]["bob-uuid-0000"]["lend_copy"]["borrower"], "borrower")
                carol = json.loads((self.paths.accounts / "carol-uuid-0000.json").read_text())
                with accounts.Hub(self.hub_paths) as h:
                    h.pool_put(carol)
                    accounts.atomic_write(h.dir / "readings.json", {"carol-uuid-0000": {"at": int(time.time()),
                        "limits": accounts.parse_limits(usage(("weekly_all", None, 20, 86400, True)))}})
                self.manager_reading(week, session)
                exp = self.borrower_export("bob", ["bob"], week, session)
                exp["last_switch_at"] = time.time()  # caps bypass dwell and daily switch limit
                exp["auto_switches_24h"] = 24
                reply = self.lend(exp)
                self.assertEqual(reply["lend"]["record"]["oauthAccount"]["accountUuid"], "carol-uuid-0000")
                exp = self.borrower_export("carol", ["bob", "carol"])
                exp["returning"] = {"bob-uuid-0000": self.copy}
                reply = self.lend(exp)
                self.assertIn("bob-uuid-0000", reply["returned"])
                with accounts.Hub(self.hub_paths) as h:
                    self.assertEqual(json.loads(h.copy_path("bob-uuid-0000").read_text()), self.copy)
                    self.assertFalse((h.pool_dir / "bob-uuid-0000.json").exists())
                    self.assertIsNone(h.copies()["bob-uuid-0000"]["borrower"])
                    # Remove the unconfirmed carol lend for the next iteration.
                    (h.pool_dir / "lent" / "carol-uuid-0000.borrower.json").unlink(missing_ok=True)
                with accounts.Hub(self.hub_paths) as h:
                    copies = h.copies()
                    copies["bob-uuid-0000"]["blocked_at"] = int(time.time()) - 1
                    h.save_copies(copies)
                self.manager_reading(20, 30)
                with accounts.Hub(self.hub_paths) as h:
                    report = h.reports()["cm-manager"]
                    report["export"]["accounts"]["bob-uuid-0000"]["reading"]["at"] = int(time.time()) + 1
                    accounts.atomic_write(h.dir / "reports/cm-manager.json", report)

    def test_same_second_reading_cannot_hide_borrower_cap(self):
        self.import_copy()
        self.lend(self.borrower_export())
        now = int(time.time())
        borrower = self.borrower_export("bob", ["bob"], week=20, session=60)
        manager = host_export("cm-manager", active="bob", saved=["bob"],
                              bob=(0, ("weekly_all", None, 20, 86400, True),
                                   ("session", None, 30, 3600, True)))
        borrower["accounts"]["bob-uuid-0000"]["reading"]["at"] = now
        manager["accounts"]["bob-uuid-0000"]["reading"]["at"] = now
        with accounts.Hub(self.hub_paths) as h:
            carol = json.loads((self.paths.accounts / "carol-uuid-0000.json").read_text())
            h.pool_put(carol)
            accounts.atomic_write(h.dir / "readings.json", {"carol-uuid-0000": {"at": now,
                "limits": accounts.parse_limits(usage(("weekly_all", None, 20, 86400, True)))}})
            reports = h.reports()
            reports["cm-manager"] = {"at": now, "export": manager}
            reports["borrower"] = {"at": now, "export": borrower}
            reply = accounts.pool_lend(h, "borrower", borrower, reports, now)
            self.assertEqual(reply["record"]["oauthAccount"]["accountUuid"], "carol-uuid-0000")

    def test_existing_host_borrows_switches_and_returns_copy(self):
        self.import_copy()
        for name in ("bob", "carol"):
            (self.paths.accounts / f"{name}-uuid-0000.json").unlink()
        self.age_last_switch(3600)
        self.usage[self.live_token()] = usage(("weekly_all", None, 99, 86400, True))
        self.usage[self.copy["claudeAiOauth"]["accessToken"]] = usage(
            ("weekly_all", None, 20, 86400, True), ("session", None, 30, 3600, True))
        with accounts.Hub(self.hub_paths) as h:
            h.desired["hosts"]["laptop"] = {"mode": "auto"}
            h.save()
        code, text = self.poll()
        self.assertEqual(code, 0, text)
        self.assertEqual(self.live_email(), "bob@example.com")
        with accounts.Hub(self.hub_paths) as h:
            self.assertEqual(h.copies()["bob-uuid-0000"]["borrower"], "laptop")
            carol = {"oauthAccount": profile("carol"), "claudeAiOauth": login("carol-target")}
            h.pool_put(carol)
            accounts.atomic_write(h.dir / "readings.json", {"carol-uuid-0000": {"at": int(time.time()),
                "limits": accounts.parse_limits(usage(("weekly_all", None, 20, 86400, True)))}})
        self.usage[carol["claudeAiOauth"]["accessToken"]] = usage(("weekly_all", None, 20, 86400, True))
        self.usage[self.copy["claudeAiOauth"]["accessToken"]] = usage(
            ("weekly_all", None, 20, 86400, True), ("session", None, 60, 3600, True))
        code, text = self.poll()
        self.assertEqual(code, 0, text)
        self.assertEqual(self.live_email(), "carol@example.com")
        self.age_last_switch(3600)
        state = self.state()
        for event in state["history"]:
            if event.get("event") == "borrowed":
                event["at"] -= 3600
        accounts.atomic_write(self.paths.state, state)
        code, text = self.poll()
        self.assertEqual(code, 0, text)
        with accounts.Hub(self.hub_paths) as h:
            returned = json.loads(h.copy_path("bob-uuid-0000").read_text())
            self.assertEqual(returned["claudeAiOauth"], self.copy["claudeAiOauth"])
            self.assertEqual(returned["oauthAccount"], self.copy["oauthAccount"])
            self.assertIsNone(h.copies()["bob-uuid-0000"]["borrower"])
        self.assertEqual(json.loads((self.hub_paths.accounts / "bob-uuid-0000.json").read_text()), self.primary)

    def test_reborrow_checks_all_newest_readings_when_timestamps_tie(self):
        self.import_copy()
        now = int(time.time())
        with accounts.Hub(self.hub_paths) as h:
            copies = h.copies()
            copies["bob-uuid-0000"]["blocked_at"] = now - 1
            h.save_copies(copies)
            report = h.reports()["cm-manager"]
            report["export"]["accounts"]["bob-uuid-0000"]["reading"]["at"] = now
            accounts.atomic_write(h.dir / "reports/cm-manager.json", report)
            accounts.atomic_write(h.dir / "readings.json", {"bob-uuid-0000": {"at": now,
                "limits": accounts.parse_limits(usage(("weekly_all", None, 20, 86400, True),
                                                      ("session", None, 60, 3600, True)))}})
            self.assertFalse(any(c[1].endswith("~copy") for c in
                                 accounts.pool_candidates(h, "borrower", h.reports(), now)))

    def test_copy_is_not_lent_without_a_held_fallback(self):
        self.import_copy()
        exp = host_export("borrower")
        exp["last_switch_at"] = 0
        reply = self.lend(exp)
        self.assertNotIn("lend", reply)
        with accounts.Hub(self.hub_paths) as h:
            self.assertTrue(h.copy_path("bob-uuid-0000").exists())
            self.assertIsNone(h.copies()["bob-uuid-0000"]["borrower"])

    def test_copy_can_use_another_held_login_if_the_previous_one_is_unusable(self):
        self.import_copy()
        exp = self.borrower_export("alice", ["alice", "carol"])
        exp["accounts"]["alice-uuid-0000"]["login"] = "needs /login"
        self.assertIn("lend", self.lend(exp))
        exp = self.borrower_export("bob", ["alice", "bob", "carol"], session=60)
        exp["accounts"]["alice-uuid-0000"]["login"] = "needs /login"
        reply = self.lend(exp)
        self.assertEqual(reply["entry"]["account"], "carol-uuid-0000")

    def test_cap_with_empty_pool_switches_to_held_account_and_returns_copy(self):
        self.import_copy()
        for name in ("bob", "carol"):
            (self.paths.accounts / f"{name}-uuid-0000.json").unlink()
        self.age_last_switch(3600)
        self.usage[self.live_token()] = usage(("weekly_all", None, 99, 86400, True))
        copy_token = self.copy["claudeAiOauth"]["accessToken"]
        self.usage[copy_token] = usage(("weekly_all", None, 20, 86400, True),
                                       ("session", None, 30, 3600, True))
        with accounts.Hub(self.hub_paths) as h:
            h.desired["hosts"]["laptop"] = {"mode": "auto"}
            h.save()
        self.assertEqual(self.poll()[0], 0)
        self.assertEqual(self.live_email(), "bob@example.com")
        # The old login normally becomes a spare after 30 minutes. The hub
        # must leave it on this host so it can be used for eviction later.
        self.age_last_switch(3600)
        state = self.state()
        for event in state["history"]:
            if event.get("event") == "borrowed":
                event["at"] -= 3600
        accounts.atomic_write(self.paths.state, state)
        self.assertEqual(self.poll()[0], 0)
        self.assertTrue((self.paths.accounts / "alice-uuid-0000.json").exists())
        with accounts.Hub(self.hub_paths) as h:
            self.assertEqual(h.pool(), {})
            self.assertEqual(h.copies()["bob-uuid-0000"]["fallback"], "alice-uuid-0000")
        self.usage[copy_token] = usage(("weekly_all", None, 20, 86400, True),
                                       ("session", None, 60, 3600, True))
        code, text = self.poll()
        self.assertEqual(code, 0, text)
        self.assertEqual(self.live_email(), "alice@example.com")  # 99% is still a valid fallback
        self.assertEqual(self.state()["hub"]["reply"]["entry"]["mode"], "pin")
        with accounts.Hub(self.hub_paths) as h:
            self.assertEqual(h.desired["hosts"]["laptop"]["mode"], "auto")
        self.age_last_switch(3600)
        self.assertEqual(self.poll()[0], 0)
        with accounts.Hub(self.hub_paths) as h:
            self.assertIsNone(h.copies()["bob-uuid-0000"]["borrower"])
            self.assertEqual(json.loads(h.copy_path("bob-uuid-0000").read_text())["claudeAiOauth"],
                             self.copy["claudeAiOauth"])
            blocked = h.copies()["bob-uuid-0000"]["blocked_at"]
            self.assertFalse(any(c[1].endswith("~copy") for c in
                                 accounts.pool_candidates(h, "laptop", h.reports(), time.time())))
        # A reading taken after the eviction, with both limits under cap,
        # makes the copy eligible again.
        with accounts.Hub(self.hub_paths) as h:
            report = h.reports()["cm-manager"]
            reading = report["export"]["accounts"]["bob-uuid-0000"]["reading"]
            reading["at"] = blocked + 1
            accounts.atomic_write(h.dir / "reports/cm-manager.json", report)
            self.assertTrue(any(c[1].endswith("~copy") for c in
                                accounts.pool_candidates(h, "laptop", h.reports(), blocked + 1)))
        self.usage[copy_token] = usage(("weekly_all", None, 20, 86400, True),
                                       ("session", None, 30, 3600, True))
        self.age_last_switch(3600)
        self.assertEqual(self.poll()[0], 0)
        self.assertEqual(self.live_email(), "bob@example.com")

    def test_eviction_prefers_the_previous_account_over_another_held_login(self):
        self.import_copy()
        exp = host_export("borrower", active="zoe", saved=["alice", "zoe"],
                          zoe=(0, ("weekly_all", None, 99, 86400, True)))
        exp["last_switch_at"] = 0
        self.assertIn("lend", self.lend(exp))
        exp = self.borrower_export("bob", ["alice", "bob", "zoe"], session=60)
        reply = self.lend(exp)
        self.assertEqual(reply["entry"]["account"], "zoe-uuid-0000")

    def test_eviction_retries_and_overrides_pin_until_return(self):
        self.import_copy()
        self.assertIn("lend", self.lend(self.borrower_export()))
        with accounts.Hub(self.hub_paths) as h:
            h.desired["hosts"]["borrower"] = {"mode": "pin", "account": "bob-uuid-0000"}
            h.save()
            exp = self.borrower_export("bob", ["bob"], session=60)
            reply = accounts.hub_checkin(h, exp)
            self.assertEqual(reply["entry"]["account"], "alice-uuid-0000")
            # A lost reply and a later below-cap reading must not cancel eviction.
            exp = self.borrower_export("bob", ["bob"], session=30)
            exp["accounts"]["bob-uuid-0000"]["reading"]["at"] += 1
            reports = h.reports()
            reports["borrower"] = {"at": int(time.time()), "export": exp}
            self.assertIsNone(accounts.borrowed_copy_over_cap(h, "borrower", exp, reports, time.time()))
            reply = accounts.hub_checkin(h, exp)
            self.assertEqual(reply["entry"]["account"], "alice-uuid-0000")
            # Even the persisted pin to bob must not block its return.
            exp = self.borrower_export("alice", ["alice", "bob"])
            exp["returning"] = {"bob-uuid-0000": self.copy}
            reply = accounts.hub_checkin(h, exp)
            self.assertIn("bob-uuid-0000", reply["returned"])
            self.assertEqual(h.desired["hosts"]["borrower"]["account"], "bob-uuid-0000")
            self.assertTrue(h.copy_path("bob-uuid-0000").exists())

    def test_missing_and_stale_borrower_readings_evict_without_pool(self):
        self.import_copy()
        for reading_kind in ("missing", "stale"):
            with self.subTest(reading_kind=reading_kind):
                self.assertIn("lend", self.lend(self.borrower_export()))
                exp = self.borrower_export("bob", ["bob"])
                if reading_kind == "missing":
                    exp["accounts"]["bob-uuid-0000"]["reading"] = None
                else:
                    exp["accounts"]["bob-uuid-0000"]["reading"]["at"] -= 4 * 3600
                reply = self.lend(exp)
                self.assertEqual(reply["entry"]["mode"], "pin")
                self.assertEqual(reply["entry"]["account"], "alice-uuid-0000")
                self.assertNotIn("lend", reply)
                exp = self.borrower_export("alice", ["bob", "alice"])
                exp["returning"] = {"bob-uuid-0000": self.copy}
                reply = self.lend(exp)
                self.assertIn("bob-uuid-0000", reply["returned"])
                with accounts.Hub(self.hub_paths) as h:
                    self.assertFalse(any(c[1].endswith("~copy") for c in
                                         accounts.pool_candidates(h, "borrower", h.reports(), time.time())))
                    copies = h.copies()
                    copies["bob-uuid-0000"]["blocked_at"] = int(time.time()) - 1
                    h.save_copies(copies)
                self.manager_reading(20, 30)

    def test_lost_return_ack_cannot_replace_primary(self):
        self.import_copy()
        self.lend(self.borrower_export())
        with accounts.Hub(self.hub_paths) as h:
            accounts.pool_settle(h, "borrower", self.borrower_export("bob", ["bob"]), [])
            h.pool_put(self.primary)
            rotated = {**self.copy, "claudeAiOauth": {**self.copy["claudeAiOauth"],
                       "refreshToken": "rotated-copy-token"}}
            self.tokens.add("rotated-copy-token")
            discard = []
            returned = accounts.pool_take_returns(h, "borrower", self.borrower_export("carol"),
                                                  {"bob-uuid-0000": rotated}, discard)
            self.assertEqual(returned, ["bob-uuid-0000"])
            discard = []
            returned = accounts.pool_take_returns(h, "borrower", self.borrower_export("carol"),
                                                  {"bob-uuid-0000": rotated}, discard)
            self.assertEqual(returned, [])
            self.assertEqual(discard, ["bob-uuid-0000"])
            self.assertEqual(h.pool()["bob-uuid-0000"], self.primary)
            self.assertEqual(json.loads(h.copy_path("bob-uuid-0000").read_text()), rotated)

    def test_unconfirmed_copy_returns_without_touching_primary(self):
        self.import_copy()
        self.lend(self.borrower_export())
        with accounts.Hub(self.hub_paths) as h:
            h.pool_put(self.primary)
            accounts.pool_settle(h, "borrower", self.borrower_export(), [])
            self.assertEqual(h.pool()["bob-uuid-0000"], self.primary)
            self.assertEqual(json.loads(h.copy_path("bob-uuid-0000").read_text()), self.copy)
            self.assertIsNone(h.copies()["bob-uuid-0000"]["borrower"])

    def test_copy_fails_closed_on_missing_or_stale_readings(self):
        self.import_copy()
        for reading in (None, {"at": time.time(), "limits": []},
                        {"at": time.time() - 4 * 3600,
                         "limits": accounts.parse_limits(usage(("session", None, 1, 3600, True),
                                                              ("weekly_all", None, 1, 86400, True)))}):
            self.assertFalse(accounts.lend_copy_has_room(reading, {"week": 80, "five_hour": 60}, time.time()))
        self.assertEqual(self.hub("policy", "lend_caps.bob@example.com", "week=80,five_hour=101")[0], 1)

    def test_renewal_collision_preserves_both_logins(self):
        self.import_copy()
        with accounts.Hub(self.hub_paths) as h:
            rec = {**self.primary, "claudeAiOauth": {**self.primary["claudeAiOauth"],
                   "expiresAt": int(time.time() * 1000) + 1000}}
            h.pool_put(rec)
        returned = {**self.primary, "claudeAiOauth": login("bob-other")}
        def renew(oauth):
            with accounts.Hub(self.hub_paths) as h:
                h.pool_put(returned)
            return {**oauth, "refreshToken": oauth["refreshToken"] + "-rotated"}
        with mock.patch.object(accounts, "refresh_login", side_effect=renew):
            accounts.read_pool_usage(self.hub_paths, io.StringIO())
        with accounts.Hub(self.hub_paths) as h:
            self.assertEqual(h.pool()["bob-uuid-0000"], returned)
            preserved = json.loads((h.pool_dir / "renewing" / "bob-uuid-0000.json").read_text())
            self.assertTrue(preserved["claudeAiOauth"]["refreshToken"].endswith("bob-rotated"))
            self.assertEqual(json.loads(h.copy_path("bob-uuid-0000").read_text()), self.copy)

    def test_refused_copy_renewal_does_not_touch_primary(self):
        self.import_copy()
        with accounts.Hub(self.hub_paths) as h:
            h.pool_put(self.primary)
            path = h.copy_path("bob-uuid-0000")
            rec = json.loads(path.read_text())
            rec["claudeAiOauth"]["expiresAt"] = int(time.time() * 1000) + 1000
            accounts.atomic_write(path, rec)
        with mock.patch.object(accounts, "refresh_login", side_effect=accounts.TokenRejected("refused")):
            accounts.read_pool_usage(self.hub_paths, io.StringIO())
        with accounts.Hub(self.hub_paths) as h:
            self.assertEqual(h.pool()["bob-uuid-0000"], self.primary)
            self.assertTrue(json.loads(h.copy_path("bob-uuid-0000").read_text())["needs_login"])

    def test_renewal_keeps_both_slots(self):
        self.import_copy()
        with accounts.Hub(self.hub_paths) as h:
            h.pool_put(self.primary)
            for path in (h.pool_dir / "bob-uuid-0000.json", h.copy_path("bob-uuid-0000")):
                rec = json.loads(path.read_text())
                rec["claudeAiOauth"]["expiresAt"] = int(time.time() * 1000) + 1000
                accounts.atomic_write(path, rec)
        def renew(oauth):
            return {**oauth, "refreshToken": oauth["refreshToken"] + "-rotated",
                    "expiresAt": int(time.time() * 1000) + 3600000}
        with mock.patch.object(accounts, "refresh_login", side_effect=renew):
            accounts.read_pool_usage(self.hub_paths, io.StringIO())
        with accounts.Hub(self.hub_paths) as h:
            primary = h.pool()["bob-uuid-0000"]
            copy = json.loads(h.copy_path("bob-uuid-0000").read_text())
        self.assertTrue(primary["claudeAiOauth"]["refreshToken"].endswith("bob-rotated"))
        self.assertTrue(copy["claudeAiOauth"]["refreshToken"].endswith("bob-copy-rotated"))
        self.assertEqual(json.loads((self.hub_paths.accounts / "bob-uuid-0000.json").read_text()), self.primary)


class HubTests(HubHarness):
    def test_check_in_reports_without_tokens_and_status_shows_it(self):
        code, text = self.poll()
        self.assertEqual(code, 0, text)
        self.assertEqual(self.checkins[0][0], "cm-manager")
        code, text = self.hub("status")
        self.assertEqual(code, 0)
        self.assertIn("laptop         on alice@example.com; wants no assignment (manual); checked in 0m ago",
                      text)
        self.assertIn("alice@example.com              active on laptop; NO ROOM; saved on laptop", text)
        self.assertIn("week 95% (resets in 3.0d, binding)  [read 0m ago on laptop]", text)
        self.assertIn("1. trading system: live and real-money work", text)
        view = json.loads(self.hub("status", "--json")[1])
        self.assertEqual(view["hosts"]["laptop"]["active"], "alice-uuid-0000")
        self.hub_files_have_no_tokens()
        self.assertIn("hub cm-manager as laptop: checked in 0m ago, rev 0, no assignment (manual)",
                      self.run_cli("status", "--local")[1])

    def test_threshold_alerts_go_to_the_coordinator_not_owner(self):
        self.poll()
        self.assertEqual(self.alerts(), [])  # no Telegram
        code, text = self.hub("alerts")
        self.assertIn("alert       laptop       alice@example.com (alice-uu) is at 95% of its week limit", text)
        self.poll()  # delivered once, not again
        self.assertEqual(len(json.loads(self.hub("alerts", "--json")[1])), 1)

    def test_alerts_wait_while_the_hub_is_down(self):
        self.hub_down = True
        code, text = self.poll()
        self.assertEqual(code, 0)
        self.assertIn("1 alert(s) waiting for the hub", self.run_cli("status", "--local")[1])
        self.assertIn("hub cm-manager as laptop: cm-manager: unreachable", self.run_cli("status", "--local")[1])
        self.hub_down = False
        self.poll()
        self.assertEqual(len(json.loads(self.hub("alerts", "--json")[1])), 1)
        self.assertNotIn("waiting for the hub", self.run_cli("status", "--local")[1])

    def test_login_problems_still_reach_owner(self):
        path = self.paths.accounts / "carol-uuid-0000.json"
        rec = json.loads(path.read_text())
        rec["claudeAiOauth"]["refreshTokenExpiresAt"] = int(time.time() * 1000) - 1000
        path.write_text(json.dumps(rec))
        self.poll()
        self.assertTrue(any("carol@example.com (carol-uu) needs a new login" in a for a in self.alerts()))

    def test_pin_switches_once_and_reports(self):
        self.poll()
        code, text = self.hub("assign", "laptop", "bob@example.com")
        self.assertEqual(code, 0, text)
        self.assertIn("laptop: pin bob@example.com (rev 1; applies at its next check-in)", text)
        code, text = self.poll()  # reports, gets rev 1, switches
        self.assertIn("assignment rev 1: switched", text)
        self.assertEqual(self.live()["claudeAiOauth"]["refreshToken"], "sk-ant-ort-REFRESH-bob")
        self.assertEqual(self.state()["history"][-1]["reason"], "hub rev 1")
        self.usage[self.live_token()] = usage(("weekly_all", None, 30, 3 * 86400, True))
        self.poll()
        self.assertEqual(self.state()["apply"]["result"], "ok")
        text = self.hub("history")[1]
        self.assertIn("laptop: pin bob@example.com", text)
        self.assertIn("apply       laptop       rev 1: switched", text)
        self.assertIn("active      laptop       alice@example.com -> bob@example.com", text)
        self.assertEqual(self.hub("assign", "laptop", "bob")[1].strip(), "laptop: already pin bob@example.com")
        self.hub_files_have_no_tokens()

    def test_pin_converges_after_a_local_switch(self):
        self.poll()
        self.hub("assign", "laptop", "alice@example.com")
        self.run_cli("switch", "carol@example.com")
        self.poll()
        self.assertEqual(self.live()["claudeAiOauth"]["refreshToken"], "sk-ant-ort-REFRESH-alice")

    def test_pinned_host_never_autoswitches(self):
        self.set_reading("bob", ("weekly_all", None, 10, 86400, True))
        self.poll()
        self.hub("assign", "laptop", "alice@example.com")
        state = self.state()
        state["config"]["autoswitch"] = "auto"  # local setting is ignored under a hub
        self.paths.state.write_text(json.dumps(state))
        self.age_last_switch(31 * 60)
        self.poll()  # fetches the pin; a mode change applies from the next cycle
        self.alerts_reset()
        self.poll()
        self.assertEqual(self.live_token(), "sk-ant-oat-ACCESS-alice")
        self.assertIn("Pinned by the coordinator", self.hub("alerts")[1])

    def test_hub_auto_mode_honours_reservations(self):
        self.set_reading("bob", ("weekly_all", None, 10, 86400, True))
        self.set_reading("carol", ("weekly_all", None, 50, 86400, True))
        self.poll()
        self.assertEqual(self.hub("reserve", "bob@example.com", "--for", "cm-manager", "--note", "coord")[0], 0)
        self.hub("assign", "laptop", "alice@example.com", "--auto")
        self.poll()  # picks up rev 2 (auto, bob reserved)
        self.age_last_switch(31 * 60)
        self.alerts_reset()
        self.poll()
        self.assertEqual(self.live()["claudeAiOauth"]["refreshToken"], "sk-ant-ort-REFRESH-carol")
        self.assertIn("reserved for cm-manager (coord)", self.hub("status")[1])

    def test_assign_refuses_unusable_targets_unless_forced(self):
        self.poll()
        self.other_host(host_export("cm-sessions", active="dave", saved=["dave"]))
        cases = [(("ghost", "bob@example.com"), "has never checked in"),
                 (("laptop", "dave@example.com"), "has no valid saved login on laptop"),
                 (("laptop", "nobody@example.com"), "matches 0 accounts")]
        for argv, why in cases:
            code, text = self.hub("assign", *argv)
            self.assertEqual(code, 1, argv)
            self.assertIn(why, text)
        self.hub("reserve", "bob", "--for", "cm-sessions")
        code, text = self.hub("assign", "laptop", "bob")
        self.assertIn("reserved for cm-sessions", text)
        self.assertEqual(self.hub("assign", "laptop", "bob", "--force")[0], 0)
        self.assertEqual(self.hub("assign", "laptop", "none")[0], 0)
        self.assertNotIn("laptop", json.loads(self.hub("status", "--json")[1])["hosts"]["laptop"].get("desired") or {})

    def test_forced_pin_to_an_account_missing_here_is_blocked_not_applied(self):
        self.poll()
        self.other_host(host_export("cm-sessions", active="dave", saved=["dave"]))
        self.hub("assign", "laptop", "dave@example.com", "--force")
        code, text = self.poll()
        self.assertIn("blocked: dave@example.com is not saved on laptop; needs /login + capture", text)
        self.assertEqual(self.live_token(), "sk-ant-oat-ACCESS-alice")
        self.poll()
        self.assertIn("last apply: blocked: dave@example.com is not saved on laptop", self.hub("status")[1])

    def test_drain_moves_each_host_to_the_best_other_account(self):
        self.set_reading("bob", ("weekly_all", None, 20, 86400, True))
        self.set_reading("carol", ("weekly_all", None, 10, 86400, True))
        self.poll()
        # cm-sessions is already on carol, so the laptop goes to bob (fewest hosts).
        self.other_host(host_export("cm-sessions", active="carol", saved=["alice", "bob", "carol"]))
        code, text = self.hub("drain", "alice@example.com", "--dry-run")
        self.assertEqual(code, 0, text)
        self.assertIn("laptop: alice@example.com -> bob@example.com (20% worst limit)", text)
        self.assertIn("dry run: nothing changed", text)
        self.assertEqual(json.loads(self.hub("status", "--json")[1])["revision"], 0)
        code, text = self.hub("drain", "alice@example.com")
        self.assertEqual(code, 0, text)
        self.poll()
        self.assertEqual(self.live()["claudeAiOauth"]["refreshToken"], "sk-ant-ort-REFRESH-bob")
        self.assertIn("DRAINING", self.hub("status")[1])
        code, text = self.hub("assign", "cm-sessions", "alice")
        self.assertIn("is draining", text)
        self.hub("undrain", "alice")
        self.assertEqual(self.hub("assign", "cm-sessions", "alice")[0], 0)

    def test_drain_never_uses_an_account_reserved_for_others(self):
        self.set_reading("bob", ("weekly_all", None, 10, 86400, True))
        self.set_reading("carol", ("weekly_all", None, 60, 86400, True))
        self.poll()
        self.other_host(host_export("cm-sessions", active="dave", saved=["dave"]))
        self.hub("reserve", "bob", "--for", "cm-sessions")
        code, text = self.hub("drain", "alice", "--dry-run")
        self.assertIn("laptop: alice@example.com -> carol@example.com", text)

    def test_drain_with_nowhere_to_go_says_so(self):
        self.set_reading("bob", ("weekly_all", None, 97, 86400, True))
        self.set_reading("carol", ("weekly_all", None, 97, 86400, True))
        self.poll()
        code, text = self.hub("drain", "alice")
        self.assertEqual(code, 1)
        self.assertIn("laptop: alice@example.com -> stays (no other usable account with room)", text)

    def test_owner_hears_when_accounts_run_low_once(self):
        notifier = self.notifier
        self.hub("policy", "notify_command", str(notifier))
        self.set_reading("bob", ("weekly_all", None, 97, 86400, True))
        self.poll()  # alice 95, bob 97, carol unread: 1 of 3 has room
        low = [a for a in self.alerts() if "running low" in a]
        self.assertEqual(len(low), 1, self.alerts())
        self.assertIn("Claude accounts running low: 1 of 3 have room (carol@example.com)", low[0])
        self.poll()
        self.assertEqual(len([a for a in self.alerts() if "running low" in a]), 1)
        self.other_host(host_export("cm-sessions", active="carol", carol=(60, ("weekly_all", None, 99, 86400, True))))
        low = [a for a in self.alerts() if "running low" in a]
        self.assertEqual(len(low), 2)
        self.assertIn("0 of 3 have room", low[1])

    def test_policy_and_critical_list(self):
        policy_text = self.hub("policy")[1]
        self.assertIn("switch_on_scoped", policy_text)
        self.assertIn("min_lend_room", policy_text)
        status_policy = json.loads(self.hub("status", "--json")[1])["policy"]
        self.assertTrue(status_policy["switch_on_scoped"])
        self.assertEqual(status_policy["min_lend_room"], 5.0)
        self.assertEqual(self.hub("policy", "switch_on_scoped", "false")[0], 0)
        self.assertEqual(self.hub("policy", "min_lend_room", "7.5")[0], 0)
        self.assertEqual(self.hub("policy", "switch_on_scoped", "maybe")[0], 1)
        self.assertEqual(self.hub("policy", "min_lend_room", "-1")[0], 1)
        self.assertEqual(self.hub("policy", "min_lend_room", "wat")[0], 1)
        self.assertEqual(self.hub("policy", "default_mode", "auto")[0], 0)
        self.assertEqual(self.hub("policy", "default_mode", "sometimes")[0], 1)
        self.assertEqual(self.hub("policy", "thresholds.weekly", "95")[0], 0)
        self.assertIn("thresholds.weekly      95", self.hub("policy")[1])
        self.assertEqual(self.hub("policy", "account_thresholds.Bob@Example.com", "94")[0], 0)
        self.assertIn("account_thresholds.bob@example.com 94", self.hub("policy")[1])
        status_policy = json.loads(self.hub("status", "--json")[1])["policy"]
        self.assertEqual(status_policy["account_thresholds"], {"bob@example.com": 94.0})
        self.assertEqual(self.hub("policy", "account_thresholds.bad-value", "94")[0], 1)
        self.assertEqual(self.hub("policy", "account_thresholds.bobexample.com", "94")[0], 1)
        self.assertEqual(self.hub("policy", "account_thresholds.bob@example.com", "default")[0], 0)
        self.assertNotIn("account_thresholds", json.loads(self.hub("status", "--json")[1])["policy"])
        code, text = self.hub("critical", "add", "the F build")
        self.assertIn("3. the F build", text)
        code, text = self.hub("critical", "remove", "1")
        self.assertNotIn("trading", text)
        self.assertIn("2. the F build", text)
        self.assertEqual(self.hub("critical", "remove", "9")[0], 1)
        self.poll()  # policy default_mode reaches the host
        self.assertEqual(self.state()["hub"]["reply"]["policy"]["default_mode"], "auto")
        self.hub("policy", "account_thresholds.alice@example.com", "94")
        self.assertIn("switch at 94% (Owner's reserve)", self.hub("status")[1])

    def test_checkin_refuses_a_mismatched_or_bad_export(self):
        export = host_export("cm-sessions")
        self.assertEqual(self.hub("checkin", "laptop", stdin=json.dumps(export))[0], 1)
        self.assertEqual(self.hub("checkin", "cm-sessions", stdin="not json")[0], 1)
        bad = dict(export, host="a;b")
        self.assertEqual(self.hub("checkin", "a;b", stdin=json.dumps(bad))[0], 1)
        code, text = self.hub("checkin", "cm-sessions", stdin=json.dumps(export))
        self.assertEqual(code, 0, text)
        self.assertEqual(json.loads(text)["version"], 1)

    def test_coordinator_command_is_woken_per_alert(self):
        wake = self.hub_paths.store.parent / "wake.txt"
        script = self.hub_paths.store.parent / "wake.sh"
        script.parent.mkdir(parents=True, exist_ok=True)
        script.write_text(f'#!/bin/sh\nprintf "%s\\n" "$1" >> {wake}\n')
        script.chmod(0o700)
        _test_subprocess_paths.add(str(script))
        self.addCleanup(_test_subprocess_paths.discard, str(script))
        self.hub("policy", "coordinator_command", str(script))
        self.poll()  # gets the policy
        self.alerts_reset()
        self.hub_down = True
        self.poll()  # queues an alert
        export = self.export_now()
        self.assertEqual(len(export["events"]), 1)
        self.hub("checkin", "laptop", stdin=json.dumps(export))
        self.assertIn("laptop: alice@example.com (alice-uu) is at 95%", wake.read_text())

    def export_now(self):
        return json.loads(self.run_cli("export")[1])

    def test_peers_from_the_hub_reading_count_for_targets(self):
        self.set_config(hub="cm-manager", host_name="laptop", autoswitch="suggest")
        self.other_host(host_export("cm-sessions", active="carol", bob=(60, ("weekly_all", None, 5, 86400, True))))
        self.poll()  # first check-in brings bob's reading
        self.alerts_reset()
        self.poll()
        self.assertIn("switch bob@example.com (week 5% 1m ago on cm-sessions)", self.hub("alerts")[1])


class UsageIntervalTests(HubHarness):
    def test_usage_is_read_at_most_once_per_interval_while_check_ins_continue(self):
        with mock.patch.object(accounts, "USAGE_INTERVAL_SECONDS", 9 * 60):
            self.poll()
            self.poll()
            self.assertEqual(len(self.usage_calls), 1)
            self.assertEqual(len(self.checkins), 2)
            state = self.state()
            state["usage"]["alice-uuid-0000"]["at"] -= 9 * 60
            self.paths.state.write_text(json.dumps(state))
            self.poll()
            self.assertEqual(len(self.usage_calls), 2)

    def test_log_line_says_when_the_reading_is_cached(self):
        with mock.patch.object(accounts, "USAGE_INTERVAL_SECONDS", 9 * 60):
            _, fresh = self.poll()
            self.assertNotIn("cached", fresh)
            state = self.state()
            state["usage"]["alice-uuid-0000"]["at"] -= 70  # alice is at 95%: re-read after 170 s
            state.setdefault("config", {})["thresholds"] = {"weekly": 99}  # 95% is below the switch point
            self.paths.state.write_text(json.dumps(state))
            _, cached = self.poll()
            self.assertEqual(len(self.usage_calls), 1)
            self.assertIn("(cached, read 1m ago)", cached)
            state = self.state()
            state["usage"]["alice-uuid-0000"]["at"] -= 120
            self.paths.state.write_text(json.dumps(state))
            self.poll()  # 190 s old and near the limit: read again, well before 9 min
            self.assertEqual(len(self.usage_calls), 2)

    def test_check_in_continues_while_backing_off(self):
        self.usage_error = accounts.RateLimited("usage requests are rate limited (HTTP 429)")
        self.poll()
        self.poll()  # backing off: no usage request, but it still checks in
        self.assertEqual(len(self.usage_calls), 1)
        self.assertEqual(len(self.checkins), 2)


class RemoteCheckinTests(unittest.TestCase):
    def test_ssh_carries_the_export_on_stdin(self):
        calls = []

        def fake_run(argv, **kw):
            calls.append((argv, kw))
            return mock.Mock(returncode=0, stdout=json.dumps({"version": 1, "revision": 3}))

        export = host_export("laptop")
        with mock.patch.object(accounts.subprocess, "run", side_effect=fake_run):
            reply = accounts.remote_checkin("cm-manager", export)
        argv, kw = calls[0]
        self.assertEqual(argv, ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=5", "cm-manager",
                                accounts.REMOTE_CHECKIN + " laptop"])
        self.assertEqual(json.loads(kw["input"]), export)
        self.assertEqual(reply["revision"], 3)
        with self.assertRaises(accounts.AccountError):
            accounts.remote_checkin("-oProxyCommand=x", export)


def ts_iso(offset):
    return datetime.fromtimestamp(time.time() + offset, timezone.utc).isoformat().replace("+00:00", "Z")


class SweepTests(AutoswitchHarness):
    """After a switch, re-prompt idle agent sessions stopped by an API error, once."""

    def setUp(self):
        super().setUp()  # switched to alice moments ago
        self.tdir = self.paths.store.parent / "transcripts"
        self.tdir.mkdir()
        self.sessions = []
        self.sent_inputs = []
        self.daemon_down = False

        def rpc(method, params, timeout=10.0):
            if self.daemon_down:
                raise accounts.AccountError("CM daemon unavailable (FileNotFoundError)")
            if method == "list_sessions":
                return [dict(s) for s in self.sessions]
            if method == "resolve_authorized_session":
                return {"transcript_path": str(self.tdir / f"{params['session_uid']}.jsonl")}
            if method == "send_input":
                self.sent_inputs.append(params)
                return {"ok": True}
            raise AssertionError(method)

        self.enterContext(mock.patch.object(accounts, "daemon_rpc", side_effect=rpc))

    def session(self, uid, last=None, error="rate_limit", offset=60, idle=True, **fields):
        item = {"session_uid": uid, "label": uid, "type": "claude-code", "state": "ready", "idle": idle,
                "managed_by_uid": "ts-lead", "continuous_task_id": None, "workflow_run_id": None}
        item.update(fields)
        self.sessions.append(item)
        lines = [{"type": "user", "message": {"content": "do work"}, "timestamp": ts_iso(-7200)}]
        if last == "error":
            lines.append({"type": "assistant", "isApiErrorMessage": True, "error": error,
                          "uuid": f"e-{uid}", "timestamp": ts_iso(offset),
                          "message": {"content": [{"type": "text", "text": "API Error"}]}})
        elif last == "ok":
            lines.append({"type": "assistant", "timestamp": ts_iso(offset),
                          "message": {"content": [{"type": "text", "text": "done"}]}})
        (self.tdir / f"{uid}.jsonl").write_text("\n".join(json.dumps(l) for l in lines) + "\n")

    def prompted(self):
        return [p["session_uid"] for p in self.sent_inputs]

    def test_errored_agent_session_is_reprompted_once_and_logged(self):
        self.session("w1", "error", "authentication_failed", offset=5)
        self.poll()
        self.assertEqual(self.prompted(), ["w1"])
        self.assertEqual(self.sent_inputs[0]["text"], accounts.REPROMPT_TEXT)
        self.assertTrue(self.sent_inputs[0]["submit"])
        self.poll()
        self.assertEqual(self.prompted(), ["w1"])  # once per switch
        log = (self.paths.store / "alerts.log").read_text()
        self.assertIn("sweep: re-prompted w1 [w1] after authentication_failed", log)
        self.assertIn("re-prompted 1 session(s) stopped by an API error around the last switch: "
                      "w1 (authentication_failed)", log)
        self.assertEqual(self.state()["history"][-1]["event"], "sweep")
        self.assertEqual(self.state()["sessions"]["live"], 1)

    def test_stopped_on_the_limit_before_the_switch_counts_recently_only(self):
        self.session("recent", "error", "rate_limit", offset=-3600)
        self.session("old", "error", "rate_limit", offset=-20 * 3600)
        self.session("other", "error", "authentication_failed", offset=-3600)
        self.poll()
        self.assertEqual(self.prompted(), ["recent"])

    def test_busy_sessions_and_normal_endings_are_left_alone(self):
        self.session("busy", "error", offset=5, idle=False)
        self.session("fine", "ok", offset=5)
        self.poll()
        self.assertEqual(self.prompted(), [])

    def test_shells_scheduler_owned_and_owner_sessions_are_never_prompted(self):
        self.session("shell", "error", offset=5, type="bash")
        self.session("cont", "error", offset=5, continuous_task_id="bug-triage")
        self.session("wf", "error", offset=5, workflow_run_id="run-1")
        self.session("owner", "error", offset=5, managed_by_uid=None)
        self.poll()
        self.assertEqual(self.prompted(), [])
        self.assertEqual(self.run_cli("config", "sweep_owner_sessions", "true")[0], 0)
        self.poll()
        self.assertEqual(self.prompted(), ["owner"])

    def test_billing_and_hold_errors_go_to_owner_not_reprompted(self):
        self.session("held", "error", "account_on_hold", offset=5)
        self.poll()
        self.assertEqual(self.prompted(), [])
        self.assertTrue(any("billing or account problem, not re-prompted: held (account_on_hold)" in a
                            for a in self.alerts()))

    def test_nothing_happens_outside_the_window_or_when_disabled(self):
        self.session("w1", "error", offset=-60)
        self.age_last_switch(31 * 60)
        self.poll()
        self.assertEqual(self.prompted(), [])
        self.age_last_switch(-31 * 60)
        self.run_cli("config", "sweep", "false")
        self.poll()
        self.assertEqual(self.prompted(), [])

    def test_each_new_switch_allows_one_more(self):
        self.session("w1", "error", offset=5)
        self.poll()
        self.run_cli("switch", "bob@example.com")
        self.session_error_again("w1")
        self.poll()
        self.assertEqual(self.prompted(), ["w1", "w1"])

    def session_error_again(self, uid):
        path = self.tdir / f"{uid}.jsonl"
        with open(path, "a") as f:
            f.write(json.dumps({"type": "assistant", "isApiErrorMessage": True, "error": "rate_limit",
                                "uuid": f"e2-{uid}", "timestamp": ts_iso(5),
                                "message": {"content": [{"type": "text", "text": "API Error"}]}}) + "\n")

    def test_daemon_down_does_not_break_poll(self):
        self.daemon_down = True
        self.session("w1", "error", offset=5)
        code, text = self.poll()
        self.assertEqual(code, 0, text)
        self.assertIsNone(self.state()["sessions"])

    def test_cap_per_run(self):
        for i in range(accounts.SWEEP_MAX_PER_RUN + 3):
            self.session(f"w{i}", "error", offset=5)
        self.poll()
        self.assertEqual(len(self.prompted()), accounts.SWEEP_MAX_PER_RUN)
        self.poll()
        self.assertEqual(len(self.prompted()), accounts.SWEEP_MAX_PER_RUN + 3)


class DaemonRpcTests(unittest.TestCase):
    def test_framing_token_and_errors(self):
        import socket as socketlib
        import struct
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        home = Path(tmp.name)
        (home / ".cm").mkdir()
        (home / ".cm/operator-token").write_text("tok-123\n")
        sock_path = str(home / "d.sock")
        server = socketlib.socket(socketlib.AF_UNIX)
        server.bind(sock_path)
        server.listen(2)
        seen = []

        def serve():
            for reply in ({"ok": True, "result": [{"session_uid": "a"}]},
                          {"ok": False, "error": {"code": "unauthorized", "message": "x"}}):
                conn, _ = server.accept()
                with conn:
                    n = struct.unpack(">I", conn.recv(4))[0]
                    data = b""
                    while len(data) < n:
                        data += conn.recv(n - len(data))
                    seen.append(json.loads(data))
                    body = json.dumps(reply).encode()
                    conn.sendall(struct.pack(">I", len(body)) + body)

        thread = threading.Thread(target=serve, daemon=True)
        thread.start()
        with mock.patch.dict(os.environ, {"HOME": str(home), "CM_DAEMON_SOCKET": sock_path}):
            self.assertEqual(accounts.daemon_rpc("list_sessions", {}), [{"session_uid": "a"}])
            with self.assertRaises(accounts.AccountError) as cm:
                accounts.daemon_rpc("send_input", {"session_uid": "a"})
            self.assertIn("unauthorized", str(cm.exception))
        thread.join(5)
        server.close()
        self.assertEqual(seen[0]["caller"], {"token_id": "tok-123"})
        self.assertEqual(seen[0]["method"], "list_sessions")
        with mock.patch.dict(os.environ, {"HOME": str(home), "CM_DAEMON_SOCKET": str(home / "none.sock")}):
            with self.assertRaises(accounts.AccountError):
                accounts.daemon_rpc("list_sessions", {})


class LoginTests(Harness):
    """`login` adds an account in a scratch config dir; the host's login never moves."""

    def setUp(self):
        super().setUp()
        self.do_login("alice")
        self.run_cli("capture")
        root = self.paths.store.parent
        self.fake_claude = root / "fake-claude.sh"
        _test_subprocess_paths.add(str(self.fake_claude))
        self.addCleanup(_test_subprocess_paths.discard, str(self.fake_claude))

    def claude_that_logs_in_as(self, name):
        oauth = login(f"scratch-{name}")
        self.tokens.update({oauth["accessToken"], oauth["refreshToken"]})
        self.anthropic[oauth["accessToken"]] = profile(name)["accountUuid"]
        creds = json.dumps({"claudeAiOauth": oauth})
        prof = json.dumps({"oauthAccount": profile(name), "numStartups": 1})
        self.fake_claude.write_text(
            "#!/bin/sh\n"
            f"printf '%s' '{creds}' > \"$CLAUDE_CONFIG_DIR/.credentials.json\"\n"
            f"printf '%s' '{prof}' > \"$CLAUDE_CONFIG_DIR/.claude.json\"\n")
        self.fake_claude.chmod(0o700)

    def login_cli(self):
        return self.run_cli("login", "--claude-command", str(self.fake_claude))

    def test_saves_the_new_account_without_touching_the_host(self):
        before = self.paths.credentials.read_bytes(), self.paths.claude_json.read_bytes()
        self.claude_that_logs_in_as("bob")
        code, text = self.login_cli()
        self.assertEqual(code, 0, text)
        self.assertIn("saved bob@example.com (bob-uuid); this host stays on its current account", text)
        self.assertEqual((self.paths.credentials.read_bytes(), self.paths.claude_json.read_bytes()), before)
        saved = json.loads((self.paths.accounts / "bob-uuid-0000.json").read_text())
        self.assertEqual(saved["claudeAiOauth"]["refreshToken"], "sk-ant-ort-REFRESH-scratch-bob")
        self.assertEqual(saved["oauthAccount"]["emailAddress"], "bob@example.com")
        self.assertEqual(self.state()["active"], "alice-uuid-0000")
        self.assertFalse((self.paths.store / "login-scratch").exists())
        # and switching to it works
        self.assertEqual(self.run_cli("switch", "bob@example.com")[0], 0)
        self.assertEqual(self.live()["claudeAiOauth"]["refreshToken"], "sk-ant-ort-REFRESH-scratch-bob")

    def test_no_login_made_is_an_error_and_cleans_up(self):
        self.fake_claude.write_text("#!/bin/sh\nexit 0\n")
        self.fake_claude.chmod(0o700)
        code, text = self.login_cli()
        self.assertEqual(code, 1)
        self.assertIn("no login was made in the scratch session", text)
        self.assertFalse((self.paths.store / "login-scratch").exists())

    def test_the_active_account_is_not_overwritten(self):
        self.claude_that_logs_in_as("alice")
        code, text = self.login_cli()
        self.assertEqual(code, 1)
        self.assertIn("already this host's active login", text)
        saved = json.loads((self.paths.accounts / "alice-uuid-0000.json").read_text())
        self.assertEqual(saved["claudeAiOauth"]["refreshToken"], "sk-ant-ort-REFRESH-alice")
        self.assertFalse((self.paths.store / "login-scratch").exists())

    def test_runs_without_holding_the_store_lock(self):
        flag = self.paths.store.parent / "lock-was-free"
        lock = self.paths.store / "lock"
        self.fake_claude.write_text(
            "#!/bin/sh\n"
            f"python3 -c 'import fcntl,os; fd=os.open(\"{lock}\", os.O_RDWR); "
            f"fcntl.flock(fd, fcntl.LOCK_EX|fcntl.LOCK_NB); open(\"{flag}\",\"w\")'\n")
        self.fake_claude.chmod(0o700)
        self.login_cli()
        self.assertTrue(flag.exists(), "the store lock was held while claude ran")

    def test_login_into_a_folder_leaves_this_hosts_store_alone(self):
        folder = self.paths.store.parent / "outgoing"
        before = sorted(p.name for p in self.paths.accounts.iterdir())
        self.claude_that_logs_in_as("alice")  # even this host's active account
        code, text = self.run_cli("login", "--into", str(folder), "--claude-command", str(self.fake_claude))
        self.assertEqual(code, 0, text)
        self.claude_that_logs_in_as("bob")
        code, text = self.run_cli("login", "--into", str(folder), "--claude-command", str(self.fake_claude))
        self.assertEqual(code, 0, text)
        self.assertEqual(sorted(p.name for p in (folder / "accounts").iterdir()),
                         ["alice-uuid-0000.json", "bob-uuid-0000.json"])
        self.assertEqual(sorted(p.name for p in self.paths.accounts.iterdir()), before)
        self.assertEqual(oct((folder / "accounts").stat().st_mode & 0o777), "0o700")
        self.assertFalse((folder / "login-scratch").exists())

    def outgoing_with(self, *names):
        folder = self.paths.store.parent / "outgoing"
        for name in names:
            self.claude_that_logs_in_as(name)
            self.assertEqual(self.run_cli("login", "--into", str(folder),
                                          "--claude-command", str(self.fake_claude))[0], 0)
        return folder

    def test_import_saves_logins_and_deletes_the_folder_but_never_the_active_one(self):
        folder = self.outgoing_with("alice", "bob")
        code, text = self.run_cli("import", str(folder))
        self.assertEqual(code, 0, text)
        self.assertIn("imported bob@example.com", text)
        self.assertIn("skipped alice@example.com (alice-uu): it is this host's active login", text)
        bob = json.loads((self.paths.accounts / "bob-uuid-0000.json").read_text())
        self.assertEqual(bob["claudeAiOauth"]["refreshToken"], "sk-ant-ort-REFRESH-scratch-bob")
        alice = json.loads((self.paths.accounts / "alice-uuid-0000.json").read_text())
        self.assertEqual(alice["claudeAiOauth"]["refreshToken"], "sk-ant-ort-REFRESH-alice")
        self.assertFalse(folder.exists())
        self.assertEqual(self.run_cli("switch", "bob@example.com")[0], 0)

    def test_switch_works_on_a_host_with_a_blank_login(self):
        self.run_cli("import", str(self.outgoing_with("bob")))
        blank = {"claudeAiOauth": {"accessToken": "", "refreshToken": "", "expiresAt": 0}}
        self.paths.credentials.write_text(json.dumps(blank))
        inode = self.paths.credentials.stat().st_ino
        code, text = self.run_cli("switch", "bob@example.com")
        self.assertEqual(code, 0, text)
        self.assertIn("switched no login -> bob@example.com", text)
        self.assertEqual(self.live()["claudeAiOauth"]["refreshToken"], "sk-ant-ort-REFRESH-scratch-bob")
        self.assertEqual(self.paths.credentials.stat().st_ino, inode)
        self.assertEqual(self.state()["active"], "bob-uuid-0000")
        alice = json.loads((self.paths.accounts / "alice-uuid-0000.json").read_text())
        self.assertEqual(alice["claudeAiOauth"]["refreshToken"], "sk-ant-ort-REFRESH-alice")

    def test_transfer_sends_imports_then_deletes_and_keeps_the_folder_on_failure(self):
        folder = self.outgoing_with("bob")
        calls = []

        def fake_run(argv, **kw):
            calls.append(argv)
            return mock.Mock(returncode=self.fail_at if len(calls) == self.fail_step else 0,
                             stdout="", stderr="boom")

        self.fail_step, self.fail_at = 2, 1
        with mock.patch.object(accounts.subprocess, "run", side_effect=fake_run):
            code, text = self.run_cli("transfer", str(folder), "cm-sessions")
        self.assertEqual(code, 1)
        self.assertIn("failed at `scp`", text)
        self.assertTrue((folder / "accounts" / "bob-uuid-0000.json").exists())

        calls.clear()
        self.fail_step = 0
        with mock.patch.object(accounts.subprocess, "run", side_effect=fake_run):
            code, text = self.run_cli("transfer", str(folder), "cm-sessions")
        self.assertEqual(code, 0, text)
        self.assertEqual([c[0] for c in calls], ["ssh", "scp", "ssh"])
        remote = calls[0][-1].split()[-1].rsplit("/accounts", 1)[0]
        self.assertTrue(remote.startswith(accounts.INCOMING_PREFIX))
        self.assertEqual(calls[1][-1], f"cm-sessions:{remote}/accounts/")
        self.assertEqual(calls[2][-1], f"{accounts.REMOTE_IMPORT} {remote}")
        self.assertFalse(folder.exists())
        self.assertEqual(self.run_cli("transfer", str(folder), "a;b")[0], 1)


if __name__ == "__main__":
    unittest.main()
