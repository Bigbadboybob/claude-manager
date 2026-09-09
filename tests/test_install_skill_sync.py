"""A scheduled command must execute the sync and leave locking to the sync itself."""
import importlib.machinery
import importlib.util
import subprocess
import tempfile
import unittest
from pathlib import Path

script = Path(__file__).resolve().parents[1] / "scripts/install-cm-skill-sync"
loader = importlib.machinery.SourceFileLoader("install_skill_sync", str(script))
spec = importlib.util.spec_from_loader(loader.name, loader)
installer = importlib.util.module_from_spec(spec)
loader.exec_module(installer)


class CronTests(unittest.TestCase):
    def test_scheduled_shell_command_executes_and_passes_config(self):
        with tempfile.TemporaryDirectory(prefix="skill sync ") as directory:
            root = Path(directory)
            executable = root / "sync"
            executable.write_text('#!/bin/sh\nprintf "%s\\n" "$@"\n')
            executable.chmod(0o755)
            config = root / "config.json"
            log = root / "cron.log"
            entry = installer.cron_entry(executable, config, log)
            # Cron hands everything after the five schedule columns to /bin/sh.
            command = entry.split(maxsplit=5)[5]
            subprocess.run(["/bin/sh", "-c", command], check=True)
            self.assertEqual(log.read_text().splitlines(), ["--config", str(config)])
            self.assertNotIn("flock", command)
