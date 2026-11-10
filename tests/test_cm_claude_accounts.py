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
        for stamp in ("2026-11-14T06:00:00.020Z", "2026-11-14T05:59:59.680Z"):
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
        self.assertEqual(self.hub("policy", "default_mode", "auto")[0], 0)
        self.assertEqual(self.hub("policy", "default_mode", "sometimes")[0], 1)
        self.assertEqual(self.hub("policy", "thresholds.weekly", "95")[0], 0)
        self.assertIn("thresholds.weekly      95", self.hub("policy")[1])
        code, text = self.hub("critical", "add", "the F build")
        self.assertIn("3. the F build", text)
        code, text = self.hub("critical", "remove", "1")
        self.assertNotIn("trading", text)
        self.assertIn("2. the F build", text)
        self.assertEqual(self.hub("critical", "remove", "9")[0], 1)
        self.poll()  # policy default_mode reaches the host
        self.assertEqual(self.state()["hub"]["reply"]["policy"]["default_mode"], "auto")

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


if __name__ == "__main__":
    unittest.main()
