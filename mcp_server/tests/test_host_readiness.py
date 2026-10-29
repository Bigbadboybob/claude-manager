import contextlib
import hashlib
import importlib.machinery
import importlib.util
import io
import json
import os
from pathlib import Path
import tempfile
import types
import unittest
from unittest import mock


SCRIPT = Path(__file__).resolve().parents[2] / "scripts" / "cm-op"
loader = importlib.machinery.SourceFileLoader("cm_op_readiness", str(SCRIPT))
spec = importlib.util.spec_from_loader(loader.name, loader)
cm_op = importlib.util.module_from_spec(spec)
loader.exec_module(cm_op)


def snapshot(root):
    return {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest()
            for p in root.rglob("*") if p.is_file()}


class HostReadinessTests(unittest.TestCase):
    def test_version_search_uses_holder_environment_not_ssh_path(self):
        with tempfile.TemporaryDirectory() as tmp:
            proc = Path(tmp)
            brain = proc / "123"; brain.mkdir()
            holder = proc / "45"; holder.mkdir()
            (brain / "stat").write_text("123 (cm-daemon) S 45 0 0\n")
            (holder / "cmdline").write_bytes(b"/opt/cm-daemon/cm-holder\0")
            (holder / "environ").write_bytes(b"PRIVATE_TOKEN=SECRET\0PATH=/holder/bin:/usr/bin\0")
            self.assertEqual(cm_op._daemon_search_path({"brain_pid":123}, proc_root=proc), "/holder/bin:/usr/bin")

    def test_static_ip_requires_matching_reserved_address(self):
        token = "SECRET_GCP_TOKEN"
        values = {"instance/network-interfaces/0/access-configs/0/external-ip":"35.221.41.39",
                  "project/project-id":"test-project", "instance/zone":"projects/test/zones/us-east4-a",
                  "instance/service-accounts/default/token":json.dumps({"access_token":token})}
        class Response:
            def __enter__(self): return self
            def __exit__(self, *args): pass
            def read(self, size):
                return json.dumps({"items":[{"address":"35.221.41.39", "addressType":"EXTERNAL", "status":"IN_USE"}]}).encode()
        opener = mock.Mock(); opener.open.return_value = Response()
        with mock.patch.object(cm_op, "_metadata", side_effect=lambda key: values[key]), \
             mock.patch.object(cm_op.urllib.request, "build_opener", return_value=opener):
            result = cm_op._external_ip_status()
        self.assertEqual(result, ("35.221.41.39", True))
        request = opener.open.call_args.args[0]
        self.assertEqual(request.get_header("Authorization"), "Bearer " + token)
        self.assertNotIn(token, str(result))

    def test_loopback_pool_health_uses_health_path(self):
        class Response:
            status = 200
            def __enter__(self): return self
            def __exit__(self, *args): pass
        opener = mock.Mock(); opener.open.return_value = Response()
        with mock.patch.object(cm_op.urllib.request, "build_opener", return_value=opener):
            self.assertTrue(cm_op._loopback_ready("http://127.0.0.1:2455/backend-api/codex"))
            self.assertEqual(opener.open.call_args.args[0], "http://127.0.0.1:2455/health")
            self.assertFalse(cm_op._loopback_ready("https://external.example/path"))
            self.assertEqual(opener.open.call_count, 1)

    def test_auth_checks_use_holder_environment_without_exposing_values(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            (home / ".claude.json").write_text('{"hasCompletedOnboarding":true}')
            mem = home / "meminfo"; mem.write_text("MemTotal: 1 kB\nMemAvailable: 1 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n")
            holder_env = {"HOME":str(home), "PATH":"/holder/bin", "ANTHROPIC_API_KEY":"SECRET_HOLDER_CLAUDE",
                          "OPENAI_API_KEY":"SECRET_HOLDER_CODEX"}
            with mock.patch.dict(os.environ, {}, clear=True), mock.patch.object(cm_op, "rpc", side_effect=OSError()), \
                 mock.patch.object(cm_op, "_daemon_environment", return_value=holder_env), \
                 mock.patch.object(cm_op, "_version", return_value="1.2.3") as version, \
                 mock.patch.object(cm_op, "_service_active", return_value=False), \
                 mock.patch.object(cm_op, "_external_ip_status", return_value=(None, False)):
                out = cm_op.host_readiness({}, home=home, meminfo=mem)
            checks = {row["name"]:row for row in out["checks"]}
            self.assertEqual(checks["claude_auth"]["status"], "PASS")
            self.assertEqual(checks["codex_auth"]["status"], "PASS")
            self.assertEqual(checks["claude_version"]["detail"]["path_source"], "holder/daemon")
            version.assert_any_call("claude", "/holder/bin")
            self.assertNotIn("SECRET_HOLDER", json.dumps(out))

    def test_read_only_probe_reports_evidence_and_redacts_credentials(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            worktree = home / ".cm" / "worktrees"; worktree.mkdir(parents=True)
            repo = home / "code" / "projects" / "example"; (repo / ".git").mkdir(parents=True)
            (home / ".cm" / "operator-token").write_text("SECRET_OPERATOR_TOKEN")
            (home / ".cm" / "claude-notifications.json").write_text('{"transport":"channel"}')
            (home / ".claude").mkdir()
            (home / ".claude" / ".credentials.json").write_text(json.dumps({"claudeAiOauth":{"accessToken":"SECRET_CLAUDE_TOKEN"}}))
            (home / ".claude.json").write_text(json.dumps({"hasCompletedOnboarding":True,
                "projects":{str(worktree):{"hasTrustDialogAccepted":True}}}))
            codex = home / ".codex"; codex.mkdir()
            (codex / "auth.json").write_text(json.dumps({"tokens":{"access_token":"SECRET_CODEX_TOKEN"}}))
            (codex / "config.toml").write_text(f'model_provider="local_pool"\n[model_providers.local_pool]\nbase_url="http://127.0.0.1:2455"\n[model_providers.local_pool.auth]\ncommand="~/bin/pool-token"\n[projects."{worktree}"]\ntrust_level="trusted"\n')
            helper = home / "bin" / "pool-token"; helper.parent.mkdir(); helper.write_text("# fixture\n"); helper.chmod(0o700)
            key = home / ".config" / "cm-codex-lb" / "proxy.key"; key.parent.mkdir(parents=True); key.write_text("SECRET_POOL_KEY")
            mcp = home / "payload"; mcp.mkdir()
            (mcp / "server.py").write_text("# fixture\n")
            (mcp / "VERSION").write_text(json.dumps({"version":"abc123", "payload_hash":cm_op._payload_hash(mcp)}))
            mcp_dir = home / ".cm" / "mcp"; mcp_dir.mkdir()
            (mcp_dir / "launcher-candidates.json").write_text(json.dumps([{"server":str(mcp / "server.py")}]))
            meminfo = home / "meminfo"; meminfo.write_text("MemTotal: 8192 kB\nMemAvailable: 4096 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n")
            def fake_rpc(method, params, **kwargs):
                if method == "daemon.health":
                    return {"ok":True, "result":{"ok":True, "build_id":"v+abc", "split":True,
                            "holder_epoch":7, "holder_build_id":"holder", "mcp_ok":True, "restarting":False, "draining":False,
                            "messaging_sync_state":"ready", "messaging_sync":{"enabled":True, "role":"replica", "connected":True,
                                                           "pending":0, "handoff_paused":False}}}
                raise AssertionError("preflight must not call messaging.sync")
            git = types.SimpleNamespace(returncode=0, stdout="https://token:SECRET_GIT_TOKEN@example.test/org/repo.git?password=SECRET_QUERY\n")
            before = snapshot(home)
            with mock.patch.dict(os.environ, {}, clear=True), mock.patch.object(cm_op, "rpc", side_effect=fake_rpc), \
                 mock.patch.object(cm_op, "_daemon_environment", return_value={"HOME":str(home), "PATH":"/holder/bin"}), \
                 mock.patch.object(cm_op, "_version", return_value="1.2.3"), \
                 mock.patch.object(cm_op, "_service_active", side_effect=lambda name: name == "cm-codex-lb-tunnel.service"), \
                 mock.patch.object(cm_op, "_loopback_ready", return_value=True), \
                 mock.patch.object(cm_op, "_external_ip_status", return_value=("203.0.113.7", True)), \
                 mock.patch.object(cm_op.subprocess, "run", return_value=git), \
                 mock.patch.object(cm_op.socket, "gethostname", return_value="f-sessions"), \
                 mock.patch.object(cm_op.getpass, "getuser", return_value="lucas"):
                out = cm_op.host_readiness({"required_repos":["example"]}, home=home, meminfo=meminfo)
            self.assertEqual(snapshot(home), before)
            rendered = json.dumps(out)
            for secret in ("SECRET_OPERATOR_TOKEN", "SECRET_CLAUDE_TOKEN", "SECRET_CODEX_TOKEN", "SECRET_POOL_KEY", "SECRET_GIT_TOKEN", "SECRET_QUERY"):
                self.assertNotIn(secret, rendered)
            checks = {row["name"]:row for row in out["checks"]}
            for name in ("daemon", "holder_split", "replica_sync", "repositories", "claude_auth", "claude_onboarding",
                         "claude_worktree_trust", "claude_notification_channel", "codex_auth", "codex_worktree_trust", "mcp_stamp", "codex_lb", "codex_provider_auth", "static_external_ip"):
                self.assertEqual(checks[name]["status"], "PASS", checks[name])
            self.assertEqual(checks["repositories"]["detail"]["repositories"][str(repo)], "https://example.test/org/repo.git")
            self.assertTrue(checks["codex_lb"]["detail"]["tunnel_active"])
            self.assertFalse(checks["codex_lb"]["detail"]["service_active"])
            self.assertEqual(checks["laptop_hosts_entry"]["status"], "FAIL")
            self.assertFalse(checks["laptop_hosts_entry"]["detail"]["checked"])
            self.assertIn('ssh_host = "203.0.113.7"', checks["laptop_hosts_entry"]["detail"]["suggested_block"])
            self.assertIn('remote_socket = "' + str(home / '.cm' / 'daemon.sock') + '"', checks["laptop_hosts_entry"]["detail"]["suggested_block"])
            self.assertEqual(checks["swap"]["detail"]["total_bytes"], 0)
            self.assertFalse(out["ok"], "uninspected laptop is never reported as verified")

    def test_missing_onboarding_trust_and_stale_stamp_fail_independently(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            (home / ".claude.json").write_text('{"hasCompletedOnboarding":false,"projects":{}}')
            mem = home / "meminfo"; mem.write_text("MemTotal: 1 kB\nMemAvailable: 1 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n")
            with mock.patch.dict(os.environ, {}, clear=True), mock.patch.object(cm_op, "rpc", side_effect=OSError()), \
                 mock.patch.object(cm_op, "_version", return_value=None), mock.patch.object(cm_op, "_service_active", return_value=False), \
                 mock.patch.object(cm_op, "_external_ip_status", return_value=(None, False)):
                out = cm_op.host_readiness({"required_repos":["missing"]}, home=home, meminfo=mem)
            checks = {row["name"]:row for row in out["checks"]}
            for name in ("daemon", "replica_sync", "repositories", "claude_onboarding", "claude_worktree_trust",
                         "claude_notification_channel", "codex_worktree_trust", "codex_lb", "codex_provider_auth", "mcp_stamp", "static_external_ip"):
                self.assertEqual(checks[name]["status"], "FAIL", checks[name])
                self.assertTrue(checks[name]["hint"])
            self.assertIsNone(checks["laptop_hosts_entry"]["detail"]["suggested_block"])

    def test_local_pool_auth_does_not_require_unrelated_direct_login(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp); codex = home / ".codex"; codex.mkdir()
            (codex / "config.toml").write_text('model_provider="local_pool"\n[model_providers.local_pool]\nbase_url="http://127.0.0.1:2455"\n[model_providers.local_pool.auth]\ncommand="~/bin/pool-token"\n')
            helper = home / "bin" / "pool-token"; helper.parent.mkdir(); helper.write_text("# fixture\n"); helper.chmod(0o700)
            key = home / ".config" / "cm-codex-lb" / "proxy.key"; key.parent.mkdir(parents=True); key.write_text("SECRET_KEY")
            mem = home / "meminfo"; mem.write_text("MemTotal: 1 kB\nMemAvailable: 1 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n")
            with mock.patch.dict(os.environ, {}, clear=True), mock.patch.object(cm_op, "rpc", side_effect=OSError()), \
                 mock.patch.object(cm_op, "_daemon_environment", return_value={"HOME":str(home), "PATH":"/holder/bin"}), \
                 mock.patch.object(cm_op, "_version", return_value=None), mock.patch.object(cm_op, "_service_active", return_value=False), \
                 mock.patch.object(cm_op, "_external_ip_status", return_value=(None, False)):
                out = cm_op.host_readiness({}, home=home, meminfo=mem)
            checks = {row["name"]:row for row in out["checks"]}
            self.assertEqual(checks["codex_auth"]["status"], "PASS")
            self.assertEqual(checks["codex_provider_auth"]["status"], "PASS")
            self.assertNotIn("SECRET_KEY", json.dumps(out))

    def test_missing_or_busy_sync_is_unknown_without_calling_mutating_rpc(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp); mem = home / "meminfo"
            mem.write_text("MemTotal: 1 kB\nMemAvailable: 1 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n")
            for extra, expected in (({}, "unknown (old brain)"), ({"messaging_sync_state":"busy"}, "unknown (busy)")):
                data = dict({"ok":True, "build_id":"v+1", "split":True, "holder_epoch":1,
                             "mcp_ok":True, "restarting":False, "draining":False}, **extra)
                with mock.patch.dict(os.environ, {}, clear=True), \
                     mock.patch.object(cm_op, "rpc", return_value={"ok":True, "result":data}) as rpc, \
                     mock.patch.object(cm_op, "_version", return_value=None), \
                     mock.patch.object(cm_op, "_service_active", return_value=False), \
                     mock.patch.object(cm_op, "_external_ip_status", return_value=(None, False)):
                    out = cm_op.host_readiness({}, home=home, meminfo=mem)
                self.assertEqual([call.args[0] for call in rpc.call_args_list], ["daemon.health"])
                sync = next(row for row in out["checks"] if row["name"] == "replica_sync")
                self.assertEqual(sync["status"], "FAIL")
                self.assertEqual(sync["detail"]["availability"], expected)

    def test_missing_channel_reports_fix_and_restart_for_existing_sessions(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp); mem = home / "meminfo"
            mem.write_text("MemTotal: 1 kB\nMemAvailable: 1 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n")
            with mock.patch.dict(os.environ, {}, clear=True), mock.patch.object(cm_op, "rpc", side_effect=OSError()), \
                 mock.patch.object(cm_op, "_daemon_environment", return_value={"HOME":str(home), "PATH":"/holder/bin"}), \
                 mock.patch.object(cm_op, "_version", return_value=None), mock.patch.object(cm_op, "_service_active", return_value=False), \
                 mock.patch.object(cm_op, "_external_ip_status", return_value=(None, False)):
                out = cm_op.host_readiness({}, home=home, meminfo=mem)
            channel = next(row for row in out["checks"] if row["name"] == "claude_notification_channel")
            self.assertEqual(channel["status"], "FAIL")
            self.assertIn('"transport":"channel"', channel["hint"])
            self.assertIn("sessions launched before the fix require a restart", channel["hint"])

    def test_command_routes_readiness_locally_without_generic_rpc(self):
        with mock.patch.object(cm_op, "host_readiness", return_value={"ok":False, "checks":[], "_account_identity":None}) as readiness, \
             mock.patch.object(cm_op, "rpc") as rpc, contextlib.redirect_stdout(io.StringIO()) as out:
            self.assertEqual(cm_op.main(["host.readiness", '{"required_repos":["x"]}']), 0)
            readiness.assert_called_once()
            rpc.assert_not_called()
            self.assertEqual(json.loads(out.getvalue())["checks"][0]["status"], "WARN")

    def test_ssh_routing_streams_same_read_only_helper(self):
        remote = types.SimpleNamespace(stdout='{"ok":false,"checks":[],"_account_identity":null}', stderr="", returncode=0)
        with mock.patch.object(cm_op.subprocess, "run", return_value=remote) as run, \
             contextlib.redirect_stdout(io.StringIO()) as out:
            self.assertEqual(cm_op.main(["--ssh", "f-sessions", "host.readiness", '{"required_repos":["x"]}']), 0)
        args, kwargs = run.call_args
        self.assertEqual(args[0][:4], ["ssh", "-o", "ConnectTimeout=15", "f-sessions"])
        self.assertIn("host.readiness", args[0][4])
        self.assertIn("def host_readiness", kwargs["input"])
        self.assertEqual(json.loads(out.getvalue())["checks"][0]["status"], "WARN")

    def test_account_comparison_warns_on_absence_or_unreachable_and_fails_on_match(self):
        account = "a" * 64; other = "b" * 64
        target = {"fingerprint":account, "machine":"x", "hostname":"target"}
        cases = [
            ({}, {}, "WARN", "unverified (no peers given)"),
            ({"peer_hosts":["peer"]}, {"peer":(None, "SSH timed out")}, "WARN", None),
            ({"peer_hosts":["peer"]}, {"peer":({"fingerprint":account,"machine":"y","hostname":"peer"}, None)}, "FAIL", None),
            ({"peer_hosts":["peer"]}, {"peer":({"fingerprint":other,"machine":"y","hostname":"peer"}, None)}, "PASS", None),
        ]
        for params, answers, expected, reason in cases:
            with mock.patch.object(cm_op, "_peer_account_probe", side_effect=lambda peer, timeout: answers[peer]):
                out = cm_op._account_comparison({"ok":True, "checks":[]}, target, params)
            check = out["checks"][0]
            self.assertEqual(check["status"], expected)
            self.assertEqual(check["detail"]["reason"], reason)
            self.assertNotIn(account, json.dumps(out))
            self.assertNotIn(other, json.dumps(out))
            if expected == "FAIL": self.assertEqual(check["detail"]["matching"], ["peer"])
            if answers.get("peer", (None, None))[1] == "SSH timed out":
                self.assertEqual(check["detail"]["unavailable"]["peer"], "SSH timed out")

    def test_account_identity_hashes_uuid_and_same_host_is_not_collision(self):
        with tempfile.TemporaryDirectory() as tmp:
            machine = Path(tmp) / "machine-id"; machine.write_text("fixture-machine")
            raw = "123e4567-e89b-12d3-a456-426614174000"
            with mock.patch.object(cm_op.socket, "gethostname", return_value="host-a"):
                target = cm_op._account_identity({"oauthAccount":{"accountUuid":raw}}, machine)
            self.assertNotIn(raw, json.dumps(target))
            with mock.patch.object(cm_op, "_peer_account_probe", return_value=(target, None)):
                out = cm_op._account_comparison({"ok":True,"checks":[]}, target, {"peer_hosts":["alias"]})
            self.assertEqual(out["checks"][0]["status"], "WARN")
            self.assertEqual(out["checks"][0]["detail"]["same_host_skipped"], ["alias"])


if __name__ == "__main__":
    unittest.main()
