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
        """Create a canonical disposable Git repository with two historical versions."""
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name).resolve()
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
        """Run Git in the fixture repository and propagate a nonzero exit."""
        return subprocess.check_output(["git", *args], cwd=self.root, text=True)

    def write_manifest(self, files):
        """Write the source hash mapping used by the checker invocation."""
        (self.root / "manifest.json").write_text(json.dumps({"files": files}), encoding="utf-8")

    def run_checker(self, *args):
        """Invoke the checker with fixture arguments and capture its success output."""
        with mock.patch.object(sys, "argv", ["check_evidence", "--manifest", "manifest.json", *args]), redirect_stdout(io.StringIO()) as output:
            check_evidence.main()
        return output.getvalue()

    def test_given_temporary_root_alias_then_fixture_accepts_only_canonical_in_root_paths(self):
        """Check root canonicalization without weakening rejection of outside files."""
        with tempfile.TemporaryDirectory() as directory, tempfile.TemporaryDirectory() as outside:
            canonical = Path(directory).resolve()
            (canonical / "alias-parent").mkdir()
            alias = canonical / "alias-parent" / ".."
            self.assertNotEqual(alias, canonical)
            temporary = mock.Mock()
            temporary.name = str(alias)
            temporary.cleanup = mock.Mock()
            fixture = CheckEvidenceTests()
            try:
                with mock.patch.object(tempfile, "TemporaryDirectory", return_value=temporary):
                    fixture.setUp()
                self.assertEqual(fixture.root, canonical)
                self.assertEqual(check_evidence.repository_file("source.txt"), canonical / "source.txt")
                fixture.write_manifest({"source.txt": hashlib.sha256(b"changed\n").hexdigest()})
                self.assertIn("PASS: 1 source hashes", fixture.run_checker())
                target = Path(outside) / "outside.txt"
                target.write_text("outside", encoding="utf-8")
                with self.assertRaises(ValueError):
                    check_evidence.repository_file(str(target))
            finally:
                fixture.doCleanups()

    def test_given_historical_refs_then_resolve_one_commit_and_check_its_blob(self):
        """Check supported commit references against actual historical Git blobs."""
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
        """Reject option-shaped revisions before Git can interpret them."""
        for option in ["--output=capture", "--help", "-p", ""]:
            with self.subTest(option=option), mock.patch.object(subprocess, "check_output") as command:
                with self.assertRaises(ValueError):
                    self.run_checker("--revision=" + option)
                command.assert_not_called()
        self.assertFalse((self.root / "capture").exists())
        self.assertFalse((self.root / "capture:source.txt").exists())

    def test_given_tree_or_missing_revision_then_refuse_instead_of_reinterpreting_it(self):
        """Reject missing revisions and objects that cannot resolve to a commit."""
        for revision in ["HEAD^{tree}", "missing-revision"]:
            with self.subTest(revision=revision), self.assertRaises(subprocess.CalledProcessError):
                check_evidence.resolve_revision(revision)

    def test_given_nonfinite_json_constants_then_reject_manifest_and_selected_json(self):
        """Reject non-finite JSON values in manifests and selected documents."""
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
        """Check titled local links while accepting existing and remote targets."""
        for link in ['[run](missing.md "Run")', "[run](missing.md 'Run')",
                     '[run](missing.md (Run))', '[run](<missing file.md> "Run")']:
            with self.subTest(link=link):
                (self.root / "guide.md").write_text(link, encoding="utf-8")
                with self.assertRaisesRegex(ValueError, "Missing local Markdown target"):
                    self.run_checker("--markdown", "guide.md")
        (self.root / "local file.md").write_text("local", encoding="utf-8")
        (self.root / "guide.md").write_text('[local](<local file.md> "Run")\n[web](<https://example.org> "Web")', encoding="utf-8")
        self.assertIn("1 Markdown files", self.run_checker("--markdown", "guide.md"))

    def test_given_inline_code_links_then_only_real_links_are_checked(self):
        """Ignore code examples but continue checking adjacent real links."""
        examples = [
            '`[example](missing.md)`',
            '``[example](missing.md "Title") with ` inside``',
            '`multiline\n[example](missing.md)\ncode`',
            '``[example](missing.md)``` unmatched inner run``',
        ]
        for code in examples:
            with self.subTest(code=code):
                (self.root / "guide.md").write_text(code, encoding="utf-8")
                self.assertIn("1 Markdown files", self.run_checker("--markdown", "guide.md"))
                (self.root / "guide.md").write_text(code + '\n[real](missing.md "Title")', encoding="utf-8")
                with self.assertRaisesRegex(ValueError, "Missing local Markdown target"):
                    self.run_checker("--markdown", "guide.md")
        (self.root / "guide.md").write_text('`unclosed [real](missing.md)', encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "Missing local Markdown target"):
            self.run_checker("--markdown", "guide.md")

    def replace_source_with_symlink(self, target):
        """Replace the working source with a symlink, or skip unsupported platforms."""
        source = self.root / "source.txt"
        source.unlink()
        try:
            source.symlink_to(target)
        except (OSError, NotImplementedError) as error:
            self.skipTest(f"File symlinks unavailable: {error}")

    def test_given_escaped_ticks_or_paragraph_breaks_then_real_links_are_checked(self):
        """Keep real links visible after escaped ticks and paragraph boundaries."""
        examples = [
            r'\`[real](missing.md)`',
            r'\\\`[real](missing.md)`',
            '`unclosed\n\n[real](missing.md)`',
            '`unclosed\n \t\n[real](missing.md)`',
        ]
        for text in examples:
            with self.subTest(text=text):
                (self.root / "guide.md").write_text(text, encoding="utf-8")
                with self.assertRaisesRegex(ValueError, "Missing local Markdown target"):
                    self.run_checker("--markdown", "guide.md")

    def test_given_valid_code_ticks_then_escape_rules_do_not_hide_adjacent_links(self):
        """Honor code-span closing rules while checking links outside the span."""
        for code in [r'\\`[example](missing.md)`', r'`[example](missing.md)\`',
                     '`first line\n[example](missing.md)`']:
            with self.subTest(code=code):
                (self.root / "guide.md").write_text(code, encoding="utf-8")
                self.assertIn("1 Markdown files", self.run_checker("--markdown", "guide.md"))
                (self.root / "guide.md").write_text(code + '\n[real](missing.md)', encoding="utf-8")
                with self.assertRaisesRegex(ValueError, "Missing local Markdown target"):
                    self.run_checker("--markdown", "guide.md")

    def test_given_historical_blob_then_source_filesystem_resolution_is_not_used(self):
        """Read a real Git blob without resolving its current filesystem path."""
        self.write_manifest({"source.txt": hashlib.sha256(b"original\n").hexdigest()})
        original = check_evidence.repository_file

        def current_file(name):
            """Fail if historical source lookup consults the current filesystem."""
            if name == "source.txt":
                self.fail("Historical source lookup consulted the current filesystem")
            return original(name)

        with mock.patch.object(check_evidence, "repository_file", side_effect=current_file):
            self.assertIn("PASS: 1 source hashes", self.run_checker("--revision", "original"))

    def test_given_current_inside_symlink_then_historical_lookup_uses_original_tree_path(self):
        """Keep historical lookup on its original tree path despite an in-root symlink."""
        target = self.root / "different.txt"
        target.write_bytes(b"different\n")
        self.replace_source_with_symlink(target)
        self.write_manifest({"source.txt": hashlib.sha256(b"original\n").hexdigest()})
        self.assertIn("PASS: 1 source hashes", self.run_checker("--revision", "original"))

    def test_given_current_outside_symlink_then_history_works_but_current_mode_rejects(self):
        """Allow historical blobs but reject current sources that escape through symlinks."""
        with tempfile.TemporaryDirectory() as outside:
            target = Path(outside) / "outside.txt"
            target.write_bytes(b"original\n")
            self.replace_source_with_symlink(target)
            self.write_manifest({"source.txt": hashlib.sha256(b"original\n").hexdigest()})
            self.assertIn("PASS: 1 source hashes", self.run_checker("--revision", "original"))
            with self.assertRaises(ValueError):
                self.run_checker()

    def test_given_nonportable_historical_paths_then_reject_before_blob_lookup(self):
        """Reject unsafe tree paths before Git receives a blob lookup."""
        for name in ["/source.txt", "C:/source.txt", "C:source.txt", "\\\\server\\source.txt",
                     "dir\\source.txt", "../source.txt", "dir/../source.txt", "./source.txt",
                     "dir//source.txt", "source.txt/", "source\0.txt", ""]:
            with self.subTest(name=name):
                self.write_manifest({name: hashlib.sha256(b"original\n").hexdigest()})
                original = subprocess.check_output
                with mock.patch.object(subprocess, "check_output", wraps=original) as command:
                    with self.assertRaisesRegex(ValueError, "Invalid historical source path"):
                        self.run_checker("--revision", "original")
                    self.assertEqual(len(command.call_args_list), 1)
                    self.assertEqual(command.call_args.args[0][1], "rev-parse")


if __name__ == "__main__":
    unittest.main()
