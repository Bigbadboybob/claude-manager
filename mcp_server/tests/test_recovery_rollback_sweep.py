import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[2] / "scripts" / "cm-recovery-rollback-sweep"


class RecoveryRollbackSweepTests(unittest.TestCase):
    def test_dry_run_and_apply_preserve_mixed_queue_and_inflight_receipt(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "cm"
            notifications = root / "notifications"
            queue = notifications / "uid-digest"
            queue.mkdir(parents=True)
            (notifications / "claude-limit-recovery-state").write_text('{"version":1,"enabled":false}')
            controls = {"queue.lock": b"", "consumer.lock": b"", "hold": b"hold",
                        "transport.json": b'{"status":"ready"}', "takeover-1.request": b"request"}
            for name, data in controls.items():
                (queue / name).write_bytes(data)
            kept = {}
            for name, version, source, status in [
                ("chat.json", 1, "chat", "pending"),
                ("old-recovery.json", 1, "claude-limit-recovery", "submitted"),
                ("other-v2.json", 2, "chat", "pending"),
            ]:
                data = json.dumps({"version":version, "source":source, "id":name, "status":status}).encode()
                (queue / name).write_bytes(data); kept[name] = data
            moved = {}
            for status in ("pending", "submitted", "observed", "cancelled", "submitting"):
                name = f"recovery-{status}.json"
                data = json.dumps({"version":2, "source":"claude-limit-recovery", "id":name, "status":status}).encode()
                (queue / name).write_bytes(data); moved[name] = data
            backup = Path(tmp) / "backup"
            def run(*args):
                return subprocess.run([sys.executable, str(SCRIPT), "--root", str(root), "--backup", str(backup), *args],
                                      capture_output=True, text=True, timeout=10)
            dry = run()
            self.assertEqual(dry.returncode, 1, dry.stderr)
            self.assertIn("dry run", dry.stdout)
            self.assertFalse(backup.exists())
            for name, data in {**controls, **kept, **moved}.items():
                self.assertEqual((queue / name).read_bytes(), data)
            applied = run("--apply")
            self.assertEqual(applied.returncode, 1, applied.stderr)
            self.assertIn("in flight; skipped", applied.stderr)
            self.assertIn("cm-recovery-rollback-sweep", (backup / "OWNER.md").read_text())
            for name, data in {**controls, **kept}.items():
                self.assertEqual((queue / name).read_bytes(), data)
            for name, data in moved.items():
                if name == "recovery-submitting.json":
                    self.assertEqual((queue / name).read_bytes(), data)
                else:
                    self.assertFalse((queue / name).exists())
                    self.assertEqual((backup / "uid-digest" / name).read_bytes(), data)
            inflight = queue / "recovery-submitting.json"
            event = json.loads(inflight.read_text()); event["status"] = "submitted"
            inflight.write_text(json.dumps(event))
            # Simulate interruption after linking the backup but before unlink.
            os.link(inflight, backup / "uid-digest" / inflight.name)
            self.assertEqual(run("--apply").returncode, 0)
            self.assertEqual(run("--apply").returncode, 0)
            self.assertFalse(inflight.exists())

    def test_apply_requires_disabled_marker_and_backup_outside_queues(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "cm"; queue = root / "notifications" / "uid"
            queue.mkdir(parents=True)
            (queue / "queue.lock").touch()
            path = queue / "event.json"
            path.write_text('{"version":2,"source":"claude-limit-recovery","id":"e","status":"pending"}')
            command = [sys.executable, str(SCRIPT), "--root", str(root), "--apply"]
            result = subprocess.run(command + ["--backup", str(Path(tmp) / "backup")], capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("disabled marker not confirmed", result.stderr)
            self.assertTrue(path.exists())
            result = subprocess.run(command + ["--backup", str(root / "notifications" / "backup")], capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertTrue(path.exists())


if __name__ == "__main__":
    unittest.main()
