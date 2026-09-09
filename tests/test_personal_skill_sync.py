import base64, importlib.machinery, json, os, tempfile
from pathlib import Path
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "scripts/cm-sync-skills"
mod = importlib.machinery.SourceFileLoader("cm_sync_skills", str(SCRIPT)).load_module()

class PersonalSkillSyncTests(unittest.TestCase):
    def test_bundle_includes_skill_files_and_rejects_private_key(self):
        with tempfile.TemporaryDirectory() as d:
            home = Path(d)
            skill = home / ".claude/skills/demo"
            skill.mkdir(parents=True)
            (skill / "SKILL.md").write_text("demo")
            (skill / "notes.txt").write_text("safe")
            bundle = mod.build_bundle(home)
            self.assertIn(".claude/skills/demo/SKILL.md", bundle["entries"])
            (skill / "leak.pem").write_text("-----BEGIN PRIVATE KEY-----")
            clean = mod.build_bundle(home)
            self.assertNotIn(".claude/skills/demo/leak.pem", clean["entries"])

    def test_codex_symlink_is_preserved_as_portable_link(self):
        with tempfile.TemporaryDirectory() as d:
            home = Path(d)
            skill = home / ".claude/skills/demo"
            skill.mkdir(parents=True)
            (skill / "SKILL.md").write_text("demo")
            agents = home / ".agents/skills"
            agents.mkdir(parents=True)
            (agents / "demo").symlink_to("../../.claude/skills/demo")
            bundle = mod.build_bundle(home)
            destination = Path(d) / "dest"
            result = mod.install(bundle, destination)
            self.assertEqual(result["changed"], 2)
            self.assertEqual((destination / ".agents/skills/demo").readlink(), Path("../../.claude/skills/demo"))
            self.assertEqual((destination / ".claude/skills/demo/SKILL.md").read_text(), "demo")

    def test_install_is_idempotent_and_does_not_delete_remote_skill(self):
        with tempfile.TemporaryDirectory() as d:
            home = Path(d) / "source"
            skill = home / ".claude/skills/demo"
            skill.mkdir(parents=True)
            (skill / "SKILL.md").write_text("demo")
            bundle = mod.build_bundle(home)
            destination = Path(d) / "dest"
            mod.install(bundle, destination)
            extra = destination / ".claude/skills/remote/SKILL.md"
            extra.parent.mkdir(parents=True)
            extra.write_text("keep")
            self.assertEqual(mod.install(bundle, destination)["changed"], 0)
            self.assertEqual(extra.read_text(), "keep")

    def test_install_rejects_tampered_bundle_before_writing(self):
        with tempfile.TemporaryDirectory() as d:
            home = Path(d) / "source"
            skill = home / ".claude/skills/demo"
            skill.mkdir(parents=True)
            (skill / "SKILL.md").write_text("demo")
            bundle = mod.build_bundle(home)
            bundle["entries"][".claude/skills/demo/SKILL.md"]["data"] = base64.b64encode(b"tampered").decode()
            destination = Path(d) / "dest"
            with self.assertRaises(ValueError):
                mod.install(bundle, destination)
            self.assertFalse(destination.exists())

if __name__ == "__main__":
    unittest.main()
