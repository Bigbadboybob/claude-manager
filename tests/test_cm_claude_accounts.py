"""Account switching must follow Claude Code's files and lock, and never leak tokens."""

import contextlib
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


class PollTests(Harness):
    """poll reads only the live login, never refreshes, and alerts once."""

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
        self.assertIn("claude-accounts|alice@example.com (alice-uu) is at 85% of its 5h limit",
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

    def test_test_alert_reports_delivery(self):
        self.assertEqual(self.run_cli("test-alert", "--notify-command", str(self.notifier))[0], 0)
        self.assertEqual(self.alerts(), ["claude-accounts|test alert from cm-claude-accounts"])
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


if __name__ == "__main__":
    unittest.main()
