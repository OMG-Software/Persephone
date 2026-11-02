"""Evidence validation rejects unsafe Git options and invalid document data."""

from contextlib import redirect_stdout
import hashlib
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "tools"))
import check_evidence


class CheckEvidenceTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.git("init", "-q")
        self.git("config", "user.name", "Synthetic checker test")
        self.git("config", "user.email", "checker@synthetic.test")
        self.git("config", "commit.gpgsign", "false")
        (self.root / "source.txt").write_bytes(b"original\n")
        self.git("add", "source.txt")
        self.git("commit", "-qm", "initial")
        self.first = self.git("rev-parse", "HEAD").strip()
        self.git("tag", "original")
        (self.root / "source.txt").write_bytes(b"changed\n")
        self.git("commit", "-qam", "second")
        self.second = self.git("rev-parse", "HEAD").strip()
        self.git("branch", "snapshot")
        patch = mock.patch.object(check_evidence, "ROOT", self.root)
        patch.start()
        self.addCleanup(patch.stop)
        self.write_manifest({})

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.root, text=True)

    def write_manifest(self, files):
        (self.root / "manifest.json").write_text(json.dumps({"files": files}), encoding="utf-8")

    def run_checker(self, *args):
        with mock.patch.object(sys, "argv", ["check_evidence", "--manifest", "manifest.json", *args]), redirect_stdout(io.StringIO()) as output:
            check_evidence.main()
        return output.getvalue()

    def test_given_historical_refs_then_resolve_one_commit_and_check_its_blob(self):
        for ref, expected in [("HEAD", self.second), ("HEAD~1", self.first),
                              ("snapshot", self.second), ("original", self.first),
                              (self.first[:12], self.first)]:
            with self.subTest(ref=ref):
                self.assertEqual(check_evidence.resolve_revision(ref), expected)
        self.write_manifest({"source.txt": hashlib.sha256(b"original\n").hexdigest()})
        self.assertIn("PASS: 1 source hashes", self.run_checker("--revision", "original"))
        with self.assertRaisesRegex(ValueError, "hash mismatch"):
            self.run_checker()

    def test_given_git_option_revisions_then_reject_before_running_git(self):
        for option in ["--output=capture", "--help", "-p", ""]:
            with self.subTest(option=option), mock.patch.object(subprocess, "check_output") as command:
                with self.assertRaises(ValueError):
                    self.run_checker("--revision=" + option)
                command.assert_not_called()
        self.assertFalse((self.root / "capture").exists())
        self.assertFalse((self.root / "capture:source.txt").exists())

    def test_given_tree_or_missing_revision_then_refuse_instead_of_reinterpreting_it(self):
        for revision in ["HEAD^{tree}", "missing-revision"]:
            with self.subTest(revision=revision), self.assertRaises(subprocess.CalledProcessError):
                check_evidence.resolve_revision(revision)

    def test_given_nonfinite_json_constants_then_reject_manifest_and_selected_json(self):
        for token in ["NaN", "Infinity", "-Infinity"]:
            with self.subTest(token=token):
                (self.root / "input.json").write_text('{"value":' + token + '}', encoding="utf-8")
                with self.assertRaisesRegex(ValueError, "Invalid JSON constant"):
                    self.run_checker("--json", "input.json")
                (self.root / "manifest.json").write_text('{"files":{},"value":' + token + '}', encoding="utf-8")
                with self.assertRaisesRegex(ValueError, "Invalid JSON constant"):
                    self.run_checker()
                self.write_manifest({})
        (self.root / "input.json").write_text('{"value":12.5}', encoding="utf-8")
        self.assertIn("PASS:", self.run_checker("--json", "input.json"))

    def test_given_titled_links_then_check_local_targets_and_allow_remote_targets(self):
        for link in ['[run](missing.md "Run")', "[run](missing.md 'Run')",
                     '[run](missing.md (Run))', '[run](<missing file.md> "Run")']:
            with self.subTest(link=link):
                (self.root / "guide.md").write_text(link, encoding="utf-8")
                with self.assertRaisesRegex(ValueError, "Missing local Markdown target"):
                    self.run_checker("--markdown", "guide.md")
        (self.root / "local file.md").write_text("local", encoding="utf-8")
        (self.root / "guide.md").write_text('[local](<local file.md> "Run")\n[web](<https://example.org> "Web")', encoding="utf-8")
        self.assertIn("1 Markdown files", self.run_checker("--markdown", "guide.md"))


if __name__ == "__main__":
    unittest.main()
